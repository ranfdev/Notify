use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::LinesStream;
use tracing::warn;
use tracing::{debug, error, info, instrument, Instrument};

use crate::{
    models,
    ntfy_capnp::{output_channel, Status},
    Error, SharedEnv,
};

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240); // 4 minutes

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
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

pub fn build_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(TIMEOUT)
        // rustls is used because HTTP 2 isn't discovered with native-tls.
        // HTTP 2 is required to multiplex multiple requests over a single connection.
        // You can check that the app is using a single connection to a server by doing
        // ```
        // ping ntfy.sh # to get the ip address
        // netstat | grep $ip
        // ```
        .use_rustls_tls()
        .build()?)
}

fn topic_request(
    client: &reqwest::Client,
    endpoint: &str,
    topic: &str,
    since: u64,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<reqwest::Request> {
    let url = models::Subscription::build_url(endpoint, topic, since)?;
    let mut req = client
        .get(url)
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

pub enum BroadcasterEvent {
    Stop,
    Restart,
}

pub struct TopicListener {
    env: crate::SharedEnv,
    endpoint: String,
    topic: String,
    status: Status,
    output_channel: output_channel::Client,
    since: u64,
}

impl TopicListener {
    pub fn new(
        env: SharedEnv,
        endpoint: String,
        topic: String,
        since: u64,
        output_channel: output_channel::Client,
    ) -> mpsc::Sender<ControlFlow<()>> {
        let (tx, mut rx) = mpsc::channel(8);
        let network = env.network.clone();
        let mut this = Self {
            env,
            endpoint,
            topic,
            status: Status::Down,
            output_channel,
            since,
        };

        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    _ = this.run_supervised_loop().instrument(tracing::debug_span!("run_supervised_loop")) => {},
                    res = rx.recv() => match res {
                        Some(ControlFlow::Continue(_)) => {
                            info!("Refreshed");
                        }
                        None | Some(ControlFlow::Break(_)) => {
                            break;
                        }
                    }
                }
            }
        });

        let tx_clone = tx.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = Self::reload_on_network_change(network, tx_clone.clone()).await {
                warn!(error = %e, "watching network failed")
            }
        });

        tx
    }

    async fn reload_on_network_change(
        monitor: Arc<dyn models::NetworkMonitorProxy>,
        tx: mpsc::Sender<ControlFlow<()>>,
    ) -> anyhow::Result<()> {
        let mut m = monitor.listen();
        while let Some(_) = m.next().await {
            tx.send(ControlFlow::Continue(())).await?;
        }
        Ok(())
    }

    fn send_current_status(&mut self) -> impl Future<Output = anyhow::Result<()>> {
        let mut req = self.output_channel.send_status_request();
        req.get().set_status(self.status);
        async move {
            req.send().promise.await?;
            Ok(())
        }
    }

    #[instrument(skip_all)]
    async fn recv_and_forward(&mut self) -> anyhow::Result<()> {
        let creds = self.env.credentials.get(&self.endpoint);
        let req = topic_request(
            &self.env.http,
            &self.endpoint,
            &self.topic,
            self.since,
            creds.as_ref().map(|x| x.username.as_str()),
            creds.as_ref().map(|x| x.password.as_str()),
        );
        let res = self.env.http.execute(req?).await?;
        let reader = tokio_util::io::StreamReader::new(
            res.bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string())),
        );
        let stream = response_lines(reader).await?;
        tokio::pin!(stream);
        self.status = Status::Up;
        self.send_current_status().await.unwrap();
        info!(topic = %&self.topic, "listening");
        while let Some(msg) = stream.next().await {
            let msg = msg?;

            let min_msg = serde_json::from_str::<models::MinMessage>(&msg)
                .map_err(|e| Error::InvalidMinMessage(msg.to_string(), e))?;
            self.since = min_msg.time.max(self.since);

            let event = serde_json::from_str(&msg)
                .map_err(|e| Error::InvalidMessage(msg.to_string(), e))?;

            match event {
                Event::Message { .. } => {
                    debug!("message event");
                    let mut req = self.output_channel.send_message_request();
                    req.get().set_message(msg.as_str().into());
                    req.send().promise.await?;
                }
                Event::KeepAlive { .. } => {
                    debug!("keepalive event");
                }
                Event::Open { .. } => {
                    debug!("open event");
                }
            }
        }

        Ok(())
    }
    async fn run_supervised_loop(&mut self) {
        let retrier = || {
            crate::retry::WaitExponentialRandom::builder()
                .min(Duration::from_secs(1))
                .max(Duration::from_secs(5 * 60))
                .build()
        };
        let mut retry = retrier();
        loop {
            let start_time = std::time::Instant::now();
            if let Err(e) = self.recv_and_forward().await {
                let uptime = std::time::Instant::now().duration_since(start_time);
                // Reset retry delay to minimum if uptime was decent enough
                if uptime > Duration::from_secs(60 * 4) {
                    retry = retrier();
                }
                error!(error = ?e);
                self.status = Status::Degraded;
                self.send_current_status().await.unwrap();
                info!(delay = ?retry.next_delay(), "restarting");
                retry.wait().await;
            } else {
                break;
            }
        }
    }
}
