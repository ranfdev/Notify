use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::rc::{Rc, Weak};
use std::time::Duration;

use ashpd::desktop::network_monitor::NetworkMonitor;
use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::future::RemoteHandle;
use futures::prelude::*;
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::LinesStream;
use tracing::{debug, error, info, instrument, Instrument};

use crate::{
    models,
    ntfy_capnp::{ntfy_proxy, output_channel, watch_handle, Status},
    Error,
};

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240); // 4 minutes

static GLOBAL_MONITOR: tokio::sync::OnceCell<NetworkMonitor> = tokio::sync::OnceCell::const_new();

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

fn build_client() -> anyhow::Result<reqwest::Client> {
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
fn topic_request(endpoint: &str, topic: &str, since: u64) -> anyhow::Result<reqwest::Request> {
    let url = models::Subscription::build_url(endpoint, topic, since)?;
    let mut req = reqwest::Request::new(reqwest::Method::GET, url);
    let headers = req.headers_mut();
    headers.append(
        "Content-Type",
        HeaderValue::from_static("application/x-ndjson"),
    );
    headers.append("Transfer-Encoding", HeaderValue::from_static("chunked"));
    Ok(req)
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

struct TopicListener {
    endpoint: String,
    topic: String,
    status: Status,
    output_channel: output_channel::Client,
    since: u64,
    client: reqwest::Client,
}

impl TopicListener {
    fn new(
        client: reqwest::Client,
        endpoint: String,
        topic: String,
        since: u64,
        output_channel: output_channel::Client,
    ) -> anyhow::Result<mpsc::Sender<ControlFlow<()>>> {
        let (tx, mut rx) = mpsc::channel(8);
        let mut this = Self {
            endpoint,
            topic,
            status: Status::Down,
            output_channel,
            since,
            client,
        };

        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    _ = this.run_supervised_loop().instrument(tracing::debug_span!("run_supervised_loop")) => {},
                    res = rx.recv() => match res {
                        Some(ControlFlow::Continue(_)) => {}
                        None | Some(ControlFlow::Break(_)) => {
                            break;
                        }
                    }
                }
            }
        });
        Ok(tx)
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
        let req = topic_request(&self.endpoint, &self.topic, self.since)?;
        let res = self.client.execute(req).await?;
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
                    req.get().set_message(&msg);
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
                .max(Duration::from_secs(60 * 10))
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

struct WatcherImpl {
    topic: String,
    all_topics: Weak<RefCell<HashMap<String, mpsc::Sender<ControlFlow<()>>>>>,
}
impl Drop for WatcherImpl {
    fn drop(&mut self) {
        if let Some(m) = self.all_topics.upgrade() {
            debug!("Dropped WatcherImpl");
            let mut m = m.borrow_mut();
            let tx = m[&self.topic].clone();
            tokio::task::spawn_local(async move {
                tx.send(ControlFlow::Break(())).await.unwrap();
            });
            m.remove(&self.topic);
        }
    }
}

impl watch_handle::Server for WatcherImpl {}

// This is a proxy to the actual ntfy server. After a network issue, this will reconnect to the
// server and re-establish all watches.
pub struct NtfyProxyImpl {
    endpoint: String,
    watching: Rc<RefCell<HashMap<String, mpsc::Sender<ControlFlow<()>>>>>,
    client: reqwest::Client,
    _monitor_task: RemoteHandle<()>,
}

impl NtfyProxyImpl {
    pub fn new(endpoint: String) -> NtfyProxyImpl {
        let watching = Rc::new(RefCell::new(
            HashMap::<String, mpsc::Sender<ControlFlow<()>>>::new(),
        ));
        let watching_clone = Rc::downgrade(&watching);

        let (f, handle) = async move {
            let mut prev_available = false;

            let monitor = GLOBAL_MONITOR
                .get_or_init(|| async move { NetworkMonitor::new().await.unwrap() })
                .await;
            while let Ok(_) = monitor.receive_changed().await {
                let available = monitor.is_available().await.unwrap();
                if available && !prev_available {
                    info!("Refreshed");
                    if let Some(ws) = watching_clone.upgrade() {
                        for (_, w) in ws.borrow().iter() {
                            w.send(ControlFlow::Continue(())).await.unwrap();
                        }
                    }
                }
                prev_available = available;
            }
        }
        .remote_handle();
        tokio::task::spawn_local(f);
        NtfyProxyImpl {
            endpoint,
            watching: watching.clone(),
            client: build_client().unwrap(),
            _monitor_task: handle,
        }
    }

    fn _watch(
        &mut self,
        topic: String,
        watcher: output_channel::Client,
        since: u64,
    ) -> anyhow::Result<watch_handle::Client> {
        if !{ self.watching.borrow().contains_key(&topic) } {
            self.watching.borrow_mut().insert(
                topic.clone(),
                TopicListener::new(
                    self.client.clone(),
                    self.endpoint.clone(),
                    topic.clone(),
                    since,
                    watcher,
                )?,
            );
        }
        Ok(capnp_rpc::new_client(WatcherImpl {
            topic,
            all_topics: Rc::downgrade(&self.watching),
        }))
    }
    fn _send_msg<'a>(
        &'a mut self,
        msg: &'a models::Message,
    ) -> impl Future<Output = Result<(), capnp::Error>> {
        let client = reqwest::Client::new();

        let json = serde_json::to_string(&msg).unwrap();
        let req = client.post(&self.endpoint).body(json.clone());

        async move {
            info!(json = ?json, "sending message");
            let res = req.send().await;
            match res {
                Err(e) => Err(capnp::Error::failed(e.to_string())),
                Ok(res) => {
                    res.error_for_status()
                        .map_err(|e| capnp::Error::failed(e.to_string()))?;
                    Ok(())
                }
            }
        }
    }
}
impl ntfy_proxy::Server for NtfyProxyImpl {
    fn publish(
        &mut self,
        params: ntfy_proxy::PublishParams,
        _results: ntfy_proxy::PublishResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let params = params.get();
        let message = pry!(pry!(params).get_message());
        let message: models::Message = serde_json::from_str(message).unwrap();
        let res = self._send_msg(&message);
        Promise::from_future(async move {
            res.await.map_err(|e| capnp::Error::failed(e.to_string()))?;
            Ok(())
        })
    }
    fn watch(
        &mut self,
        params: ntfy_proxy::WatchParams,
        mut results: ntfy_proxy::WatchResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let topic = pry!(pry!(params.get()).get_topic());
        let watcher = pry!(pry!(params.get()).get_watcher());
        let since = pry!(params.get()).get_since();
        let handle = pry!(self
            ._watch(topic.to_owned(), watcher, since.to_owned())
            .map_err(|e| capnp::Error::failed(e.to_string())));
        results.get().set_handle(handle);
        Promise::ok(())
    }
}
