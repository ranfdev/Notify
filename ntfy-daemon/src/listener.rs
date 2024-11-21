use std::cell::RefCell;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::spawn;
use tokio::sync::RwLock;
use tokio::task::{self, spawn_local, AbortHandle, LocalSet};
use tokio::{
    select,
    sync::{mpsc, oneshot, watch},
};
use tokio_stream::wrappers::LinesStream;
use tracing::{debug, error, info};

use crate::credentials::{Credential, Credentials};
use crate::http_client::{HttpClient, NullableClient};
use crate::output_tracker::OutputTracker;
use crate::{models, Error, SharedEnv};
use tokio::time::timeout;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240); // 4 minutes

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ServerEvent {
    #[serde(rename = "open")]
    Open {
        id: String,
        time: usize,
        expires: Option<usize>,
        topic: String,
    },
    #[serde(rename = "message")]
    Message(models::Message),
    #[serde(rename = "keepalive")]
    KeepAlive {
        id: String,
        time: usize,
        expires: Option<usize>,
        topic: String,
    },
}

#[derive(Debug, Clone)]
pub enum ListenerEvent {
    Message(models::Message),
    ConnectionStateChanged(ConnectionState),
}

#[derive(Clone)]
pub struct ListenerConfig {
    pub(crate) http_client: HttpClient,
    pub(crate) credentials: Credentials,
    pub(crate) endpoint: String,
    pub(crate) topic: String,
    pub(crate) since: u64,
}

#[derive(Debug)]
pub enum ListenerCommand {
    Restart,
    Shutdown,
    GetState(oneshot::Sender<ConnectionState>),
}

fn topic_request(
    client: &HttpClient,
    endpoint: &str,
    topic: &str,
    since: u64,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<reqwest::Request> {
    let url = models::Subscription::build_url(endpoint, topic, since)?;
    let mut req = client
        .get(url.as_str())
        .header("Content-Type", "application/x-ndjson")
        .header("Transfer-Encoding", "chunked");
    if let Some(username) = username {
        req = req.basic_auth(username, password);
    }

    Ok(req.build()?)
}

async fn response_lines(
    res: impl tokio::io::AsyncBufRead,
) -> Result<impl futures::Stream<Item = Result<String, std::io::Error>>, reqwest::Error> {
    let lines = LinesStream::new(res.lines());
    Ok(lines)
}

#[derive(Clone, Debug)]
pub enum ConnectionState {
    Unitialized,
    Connected,
    Reconnecting {
        retry_count: u64,
        delay: Duration,
        error: Option<Arc<anyhow::Error>>,
    },
}

pub struct ListenerActor {
    pub event_tx: async_channel::Sender<ListenerEvent>,
    pub commands_rx: Option<mpsc::Receiver<ListenerCommand>>,
    pub config: ListenerConfig,
    pub state: ConnectionState,
}

impl ListenerActor {
    pub async fn run_loop(mut self) {
        let mut commands_rx = self.commands_rx.take().unwrap();
        loop {
            select! {
                _ = self.run_supervised_loop() => {
                    // the supervised loop cannot fail. If it finished, don't restart.
                    break;
                },
                cmd = commands_rx.recv() => {
                    match cmd {
                        Some(ListenerCommand::Restart) => {
                            info!("Received restart command");
                            continue;
                        }
                        Some(ListenerCommand::Shutdown) => {
                            info!("Received shutdown command");
                            break;
                        }
                        Some(ListenerCommand::GetState(tx)) => {
                            info!("Received get state command");
                            let state = self.state.clone();
                            let _ = tx.send(state);
                        }
                        None => {
                            error!("Channel closed for ListenerActor");
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn set_state(&mut self, state: ConnectionState) {
        self.state = state.clone();
        self.event_tx
            .send(ListenerEvent::ConnectionStateChanged(state))
            .await
            .unwrap();
    }
    async fn run_supervised_loop(&mut self) {
        dbg!("supervised");
        let retrier = || {
            crate::retry::WaitExponentialRandom::builder()
                .min(Duration::from_secs(1))
                .max(Duration::from_secs(5 * 60))
                .build()
        };
        let mut retry = retrier();
        loop {
            let start_time = std::time::Instant::now();

            if let Err(e) = self.recv_and_forward_loop().await {
                let uptime = std::time::Instant::now().duration_since(start_time);
                // Reset retry delay to minimum if uptime was decent enough
                if uptime > Duration::from_secs(60 * 4) {
                    retry = retrier();
                }
                error!(error = ?e);
                self.set_state(ConnectionState::Reconnecting {
                    retry_count: retry.count(),
                    delay: retry.next_delay(),
                    error: Some(Arc::new(e)),
                })
                .await;
                info!(delay = ?retry.next_delay(), "restarting");
                retry.wait().await;
            } else {
                break;
            }
        }
    }

    async fn recv_and_forward_loop(&mut self) -> anyhow::Result<()> {
        let creds = self.config.credentials.get(&self.config.endpoint);
        let req = topic_request(
            &self.config.http_client,
            &self.config.endpoint,
            &self.config.topic,
            self.config.since,
            creds.as_ref().map(|x| x.username.as_str()),
            creds.as_ref().map(|x| x.password.as_str()),
        );
        let res = self.config.http_client.execute(req?).await?;
        let res = res.error_for_status()?;
        let reader = tokio_util::io::StreamReader::new(
            res.bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string())),
        );
        let stream = response_lines(reader).await?;
        tokio::pin!(stream);

        self.set_state(ConnectionState::Connected).await;

        info!(topic = %&self.config.topic, "listening");
        while let Some(msg) = stream.next().await {
            let msg = msg?;

            let min_msg = serde_json::from_str::<models::MinMessage>(&msg)
                .map_err(|e| Error::InvalidMinMessage(msg.to_string(), e))?;
            self.config.since = min_msg.time.max(self.config.since);

            let event = serde_json::from_str(&msg)
                .map_err(|e| Error::InvalidMessage(msg.to_string(), e))?;

            match event {
                ServerEvent::Message(msg) => {
                    debug!("message event");
                    self.event_tx
                        .send(ListenerEvent::Message(msg))
                        .await
                        .unwrap();
                }
                ServerEvent::KeepAlive { .. } => {
                    debug!("keepalive event");
                }
                ServerEvent::Open { .. } => {
                    debug!("open event");
                }
            }
        }

        Ok(())
    }
}

// Reliable listener implementation
#[derive(Clone)]
pub struct ListenerHandle {
    pub events: async_channel::Receiver<ListenerEvent>,
    pub config: ListenerConfig,
    pub commands: mpsc::Sender<ListenerCommand>,
    join_handle: Arc<Option<task::JoinHandle<()>>>,
    listener_actor: Arc<RwLock<Option<ListenerActor>>>,
}

impl ListenerHandle {
    pub fn new(config: ListenerConfig) -> ListenerHandle {
        let (event_tx, event_rx) = async_channel::bounded(64);
        let (commands_tx, commands_rx) = mpsc::channel(1);

        let config_clone = config.clone();

        // use a new local set to isolate panics
        let local_set = LocalSet::new();
        local_set.spawn_local(async move {
            let this = ListenerActor {
                event_tx,
                commands_rx: Some(commands_rx),
                config: config_clone,
                state: ConnectionState::Unitialized,
            };

            this.run_loop().await;
        });
        spawn_local(local_set);

        Self {
            events: event_rx,
            config,
            commands: commands_tx,
            listener_actor: Arc::new(RwLock::new(None)),
            join_handle: Arc::new(None),
        }
    }

    // the response will be sent as an event in self.events
    pub async fn request_state(&self) -> ConnectionState {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(ListenerCommand::GetState(tx))
            .await
            .unwrap();
        rx.await.unwrap()
    }
}

#[cfg(test)]
mod tests {
    use models::Subscription;
    use serde_json::json;
    use task::LocalSet;
    use tokio_stream::wrappers::WatchStream;

    use super::*;

    // takes a list of pattern matches. It recvs events and then matches them
    // against the macro parameters
    macro_rules! assert_event_matches {
        ($listener:expr, $( $pattern:pat_param ),+ $(,)?) => {
            $(
                $listener.events.changed().await.unwrap();
                let event = $listener.events.borrow().clone();

                panic!("{:?}", &event);
                assert!(matches!(event, $pattern));
            )+
        };
    }

    #[tokio::test]
    async fn test_listener_reconnects_on_http_status_500() {
        let local_set = LocalSet::new();
        local_set
            .spawn_local(async {
                let http_client = HttpClient::new_nullable({
                    let url = Subscription::build_url("http://localhost", "test", 0).unwrap();
                    let nullable = NullableClient::builder()
                        .text_response(url.clone(), 500, "failed")
                        .json_response(url, 200, json!({"id":"SLiKI64DOt","time":1635528757,"event":"open","topic":"mytopic"})).unwrap()
                        .build();
                    nullable
                });
                let credentials = Credentials::new_nullable(vec![]).await.unwrap();

                let config = ListenerConfig {
                    http_client,
                    credentials,
                    endpoint: "http://localhost".to_string(),
                    topic: "test".to_string(),
                    since: 0,
                };

                let mut listener = ListenerHandle::new(config.clone());
                let items: Vec<_> = listener.events.take(3).collect().await;

                dbg!(&items);
                assert!(matches!(
                    &items[..],
                    &[
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Unitialized),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Reconnecting { .. }),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Connected { .. }),
                    ]
                ));

                // assert!(matches!(
                //     listener,
                //     ListenerEvent::Error { .. },
                //     ListenerEvent::Disconnected { .. },
                //     ListenerEvent::Connected { .. },
                // ));
            });
        local_set.await;
    }

    #[tokio::test]
    async fn test_listener_reconnects_on_invalid_message() {
        let local_set = LocalSet::new();
        local_set
            .spawn_local(async {
                let http_client = HttpClient::new_nullable({
                    let url = Subscription::build_url("http://localhost", "test", 0).unwrap();
                    let nullable = NullableClient::builder()
                        .text_response(url.clone(), 200, "invalid message")
                        .json_response(url, 200, json!({"id":"SLiKI64DOt","time":1635528757,"event":"open","topic":"mytopic"})).unwrap()
                        .build();
                    nullable
                });
                let credentials = Credentials::new_nullable(vec![]).await.unwrap();

                let config = ListenerConfig {
                    http_client,
                    credentials,
                    endpoint: "http://localhost".to_string(),
                    topic: "test".to_string(),
                    since: 0,
                };

                let mut listener = ListenerHandle::new(config.clone());
                let items: Vec<_> = listener.events.take(3).collect().await;

                dbg!(&items);
                assert!(matches!(
                    &items[..],
                    &[
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Unitialized),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Reconnecting { .. }),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Connected { .. }),
                    ]
                ));
            });
        local_set.await;
    }

    #[tokio::test]
    async fn integration_connects_sends_receives_simple() {
        let local_set = LocalSet::new();
        local_set.spawn_local(async {
            let http_client = HttpClient::new(reqwest::Client::new());
            let credentials = Credentials::new_nullable(vec![]).await.unwrap();

            let config = ListenerConfig {
                http_client,
                credentials,
                endpoint: "http://localhost:8000".to_string(),
                topic: "test".to_string(),
                since: 0,
            };

            let mut listener = ListenerHandle::new(config.clone());

            // assert_event_matches!(listener, ListenerEvent::Connected { .. },);
        });
        local_set.await;
    }
}
