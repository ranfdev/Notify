use std::cell::RefCell;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::{rc::Rc, time::Duration};

use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::task::{self, spawn_local, AbortHandle, LocalSet};
use tokio::{
    select,
    sync::{broadcast, mpsc, watch},
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

#[derive(Debug, Serialize, Deserialize)]
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
    Message {
        id: String,
        expires: Option<usize>,
        #[serde(flatten)]
        message: models::Message,
    },
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

#[derive(Clone, Debug)]
pub enum ListenerCommand {
    Restart,
    Shutdown,
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

pub struct ConnectionHandler {
    pub event_tx: watch::Sender<ListenerEvent>,
    pub commands_rx: Option<broadcast::Receiver<ListenerCommand>>,
    pub config: ListenerConfig,
    pub state: Rc<RefCell<ConnectionState>>,
}

impl ConnectionHandler {
    fn new(
        config: ListenerConfig,
        event_tx: watch::Sender<ListenerEvent>,
        commands_rx: broadcast::Receiver<ListenerCommand>,
    ) -> Self {
        let this = Self {
            event_tx,
            commands_rx: Some(commands_rx),
            config,
            state: Rc::new(RefCell::new(ConnectionState::Unitialized)),
        };
        this
    }

    pub fn run(mut self) -> task::JoinHandle<()> {
        spawn_local(async move {
            let mut commands_rx = self.commands_rx.take().unwrap();
            loop {
                select! {
                        _ = self.run_supervised_loop() => {
                            // the supervised loop cannot fail. If it finished, don't restart.
                            break;
                        },
                        cmd = commands_rx.recv() => {
                            match cmd {
                                Ok(ListenerCommand::Restart) => {
                                    info!("Received restart command");
                                    continue;
                                }
                                Ok(ListenerCommand::Shutdown) => {
                                    info!("Received shutdown command");
                                    break;
                                }
                                Err(e) => {
                                    error!("Command receive error: {:?}", e);
                                    break;
                                }
                            }
                        }

                }
            }
        })
    }

    fn set_state(&mut self, state: ConnectionState) {
        self.state.replace(state.clone());
        self.event_tx
            .send(ListenerEvent::ConnectionStateChanged(state)).unwrap();
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
                });
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

        self.set_state(
                ConnectionState::Connected,
            );

        info!(topic = %&self.config.topic, "listening");
        while let Some(msg) = stream.next().await {
            let msg = msg?;
            dbg!(&msg);

            let min_msg = serde_json::from_str::<models::MinMessage>(&msg)
                .map_err(|e| Error::InvalidMinMessage(msg.to_string(), e))?;
            self.config.since = min_msg.time.max(self.config.since);

            let event = serde_json::from_str(&msg)
                .map_err(|e| Error::InvalidMessage(msg.to_string(), e))?;

            match event {
                ServerEvent::Message { message, .. } => {
                    debug!("message event");
                    self.event_tx.send(ListenerEvent::Message(message))?;
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
pub struct Listener {
    pub state: Rc<RefCell<ConnectionState>>,
    pub events: watch::Receiver<ListenerEvent>,
    pub config: ListenerConfig,
    pub commands: broadcast::Sender<ListenerCommand>,
    pub event_tracker: OutputTracker<ListenerEvent>,
    local_set: Rc<LocalSet>,
    connection_handler: Rc<RefCell<Option<ConnectionHandler>>>,
}

impl Listener {
    pub fn new(config: ListenerConfig) -> Self {
        let (tx, rx) = watch::channel(ListenerEvent::ConnectionStateChanged(
            ConnectionState::Unitialized,
        ));
        let (commands_tx, commands_rx) = broadcast::channel(1);

        let local_set = Rc::new(LocalSet::new());
        let connection_handler = ConnectionHandler::new(config.clone(), tx, commands_rx);
        let state = connection_handler.state.clone();

        let event_tracker = OutputTracker::default();
        // let event_tracker_clone = event_tracker.clone();
        // let mut rx_clone = rx.clone();
        // local_set.spawn_local(async move {
        //     rx_clone.changed().await.unwrap();
        //     event_tracker_clone.push(rx_clone.borrow().clone());
        // });

        Listener {
            state,
            events: rx,
            config,
            commands: commands_tx,
            local_set,
            event_tracker,
            connection_handler: Rc::new(RefCell::new(Some(connection_handler))),
        }
    }
    pub async fn run(&mut self) {
        let connection_handler = self.connection_handler.take().unwrap();

        let _ = self
            .local_set
            .run_until(async move {
                connection_handler.run().await.unwrap();
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use models::Subscription;
    use reqwest::ResponseBuilderExt;
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
    async fn test_listener_reconnects_on_http_status_400() {
        let local_set = LocalSet::new();
        local_set
            .run_until(async {
                let http_client = HttpClient::new_nullable({
                    let nullable = NullableClient::new();
                    let url = Subscription::build_url("http://localhost", "test", 0).unwrap();
                    nullable
                        .set_response(
                            url.as_str(),
                            reqwest::Response::from(
                                http::response::Builder::new()
                                    .status(500)
                                    .url(url.clone())
                                    .body("failed")
                                    .unwrap(),
                            ),
                        )
                        .await;
                    nullable.set_default_response(Box::new(move || {
                        reqwest::Response::from(
                            http::response::Builder::new()
                                .status(200)
                                .url(url.clone())
                                .body(r#"{"id":"SLiKI64DOt","time":1635528757,"event":"open","topic":"mytopic"}"#)
                                .unwrap(),
                        )})
                    ).await;
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

                let mut listener = Listener::new(config.clone());
                let events = listener.events.clone();
                let changes = WatchStream::new(events);
                spawn_local(async move { listener.run().await });
                let items: Vec<_> = changes.take(3).collect().await;
                

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
            })
            .await;
    }

    #[tokio::test]
    async fn test_listener_reconnects_on_invalid_message() {
        let local_set = LocalSet::new();
        local_set
            .run_until(async {
                let http_client = HttpClient::new_nullable({
                    let nullable = NullableClient::new();
                    let url = Subscription::build_url("http://localhost", "test", 0).unwrap();
                    nullable
                        .set_response(
                            url.as_str(),
                            reqwest::Response::from(
                                http::response::Builder::new()
                                    .status(200)
                                    .url(url.clone())
                                    .body("failed")
                                    .unwrap(),
                            ),
                        )
                        .await;
                    nullable.set_default_response(Box::new(move || {
                        reqwest::Response::from(
                            http::response::Builder::new()
                                .status(200)
                                .url(url.clone())
                                .body(r#"{"id":"SLiKI64DOt","time":1635528757,"event":"open","topic":"mytopic"}"#)
                                .unwrap(),
                        )
                    })).await;
                    
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

                let mut listener = Listener::new(config.clone());
                let events = listener.events.clone();
                let changes = WatchStream::new(events);
                spawn_local(async move { listener.run().await });
                let items: Vec<_> = changes.take(3).collect().await;

                dbg!(&items);
                assert!(matches!(
                    &items[..],
                    &[
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Unitialized),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Reconnecting { .. }),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Connected { .. }),
                    ]
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn integration_connects_sends_receives_simple() {
        let local_set = LocalSet::new();
        local_set
            .run_until(async {
                let http_client = HttpClient::new(reqwest::Client::new());
                let credentials = Credentials::new_nullable(vec![]).await.unwrap();

                let config = ListenerConfig {
                    http_client,
                    credentials,
                    endpoint: "http://localhost:8000".to_string(),
                    topic: "test".to_string(),
                    since: 0,
                };

                let mut listener = Listener::new(config.clone());

                // assert_event_matches!(listener, ListenerEvent::Connected { .. },);
            })
            .await;
    }
}
