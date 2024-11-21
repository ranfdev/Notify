use crate::models::NullNetworkMonitor;
use crate::models::NullNotifier;
use anyhow::{anyhow, Context};
use futures::future::join_all;
use std::{collections::HashMap, future::Future, sync::Arc};
use tokio::{
    sync::{broadcast, mpsc, oneshot, RwLock},
    task::{spawn_local, LocalSet},
};
use tracing::{error, info};

use crate::{
    http_client::HttpClient,
    message_repo::Db,
    models::{self, Account},
    ListenerActor, ListenerCommand, ListenerConfig, ListenerHandle, SharedEnv, SubscriptionHandle,
};

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240); // 4 minutes

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

// Message types for the actor
#[derive()]
pub enum NtfyMessage {
    Subscribe {
        server: String,
        topic: String,
        respond_to: oneshot::Sender<Result<SubscriptionHandle, Vec<anyhow::Error>>>,
    },
    Unsubscribe {
        server: String,
        topic: String,
        respond_to: oneshot::Sender<anyhow::Result<()>>,
    },
    RefreshAll {
        respond_to: oneshot::Sender<anyhow::Result<()>>,
    },
    ListSubscriptions {
        respond_to: oneshot::Sender<anyhow::Result<Vec<SubscriptionHandle>>>,
    },
    ListAccounts {
        respond_to: oneshot::Sender<anyhow::Result<Vec<Account>>>,
    },
    WatchSubscribed {
        respond_to: oneshot::Sender<anyhow::Result<()>>,
    },
    AddAccount {
        server: String,
        username: String,
        password: String,
        respond_to: oneshot::Sender<anyhow::Result<()>>,
    },
    RemoveAccount {
        server: String,
        respond_to: oneshot::Sender<anyhow::Result<()>>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct WatchKey {
    server: String,
    topic: String,
}

pub struct NtfyActor {
    listener_handles: Arc<RwLock<HashMap<WatchKey, SubscriptionHandle>>>,
    env: SharedEnv,
    command_rx: mpsc::Receiver<NtfyMessage>,
}

#[derive(Clone)]
pub struct NtfyHandle {
    command_tx: mpsc::Sender<NtfyMessage>,
}

impl NtfyActor {
    pub fn new(env: SharedEnv) -> (Self, NtfyHandle) {
        let (command_tx, command_rx) = mpsc::channel(32);

        let actor = Self {
            listener_handles: Default::default(),
            env,
            command_rx,
        };

        let handle = NtfyHandle { command_tx };

        (actor, handle)
    }

    async fn handle_subscribe(
        &self,
        server: String,
        topic: String,
    ) -> Result<SubscriptionHandle, Vec<anyhow::Error>> {
        let subscription = models::Subscription::builder(topic.clone())
            .server(server.clone())
            .build()
            .map_err(|e| e.into_iter().map(|e| anyhow!(e)).collect::<Vec<_>>())?;

        let mut db = self.env.db.clone();
        db.insert_subscription(subscription.clone())
            .map_err(|e| vec![anyhow!(e)])?;

        self.listen(subscription)
            .await
            .map_err(|e| vec![anyhow!(e)])
    }

    async fn handle_unsubscribe(&mut self, server: String, topic: String) -> anyhow::Result<()> {
        let subscription = self.listener_handles.write().await.remove(&WatchKey {
            server: server.clone(),
            topic: topic.clone(),
        });

        if let Some(sub) = subscription {
            sub.shutdown().await?;
        }

        self.env.db.remove_subscription(&server, &topic)?;
        info!(server, topic, "Unsubscribed");
        Ok(())
    }

    pub async fn run(&mut self) {
        while let Some(msg) = self.command_rx.recv().await {
            match msg {
                NtfyMessage::Subscribe {
                    server,
                    topic,
                    respond_to,
                } => {
                    let result = self.handle_subscribe(server, topic).await;
                    let _ = respond_to.send(result);
                }

                NtfyMessage::Unsubscribe {
                    server,
                    topic,
                    respond_to,
                } => {
                    let result = self.handle_unsubscribe(server, topic).await;
                    let _ = respond_to.send(result);
                }

                NtfyMessage::RefreshAll { respond_to } => {
                    let mut res = Ok(());
                    for sub in self.listener_handles.read().await.values() {
                        res = sub.restart().await;
                        if res.is_err() {
                            break;
                        }
                    }
                    let _ = respond_to.send(res);
                }

                NtfyMessage::ListSubscriptions { respond_to } => {
                    let subs = self
                        .listener_handles
                        .read()
                        .await
                        .values()
                        .cloned()
                        .collect();
                    let _ = respond_to.send(Ok(subs));
                }

                NtfyMessage::ListAccounts { respond_to } => {
                    let accounts = self
                        .env
                        .credentials
                        .list_all()
                        .into_iter()
                        .map(|(server, credential)| Account {
                            server,
                            username: credential.username,
                        })
                        .collect();
                    let _ = respond_to.send(Ok(accounts));
                }

                NtfyMessage::WatchSubscribed { respond_to } => {
                    let result = self.handle_watch_subscribed().await;
                    let _ = respond_to.send(result);
                }

                NtfyMessage::AddAccount {
                    server,
                    username,
                    password,
                    respond_to,
                } => {
                    let result = self
                        .env
                        .credentials
                        .insert(&server, &username, &password)
                        .await;
                    let _ = respond_to.send(result);
                }

                NtfyMessage::RemoveAccount { server, respond_to } => {
                    let result = self.env.credentials.delete(&server).await;
                    let _ = respond_to.send(result);
                }

                NtfyMessage::Shutdown => break,
            }
        }
    }

    async fn handle_watch_subscribed(&mut self) -> anyhow::Result<()> {
        let f: Vec<_> = self
            .env
            .db
            .list_subscriptions()?
            .into_iter()
            .map(|m| self.listen(m))
            .collect();

        join_all(f.into_iter().map(|x| async move {
            if let Err(e) = x.await {
                error!(error = ?e, "Can't rewatch subscribed topic");
            }
        }))
        .await;

        Ok(())
    }

    fn listen(
        &self,
        sub: models::Subscription,
    ) -> impl Future<Output = anyhow::Result<SubscriptionHandle>> {
        let server = sub.server.clone();
        let topic = sub.topic.clone();
        let listener = ListenerHandle::new(ListenerConfig {
            http_client: self.env.http_client.clone(),
            credentials: self.env.credentials.clone(),
            endpoint: server.clone(),
            topic: topic.clone(),
            since: sub.read_until,
        });
        let listener_handles = self.listener_handles.clone();
        let sub = SubscriptionHandle::new(listener.clone(), sub, &self.env);

        async move {
            listener_handles
                .write()
                .await
                .insert(WatchKey { server, topic }, sub.clone());
            Ok(sub)
        }
    }
}

impl NtfyHandle {
    pub async fn subscribe(
        &self,
        server: &str,
        topic: &str,
    ) -> Result<SubscriptionHandle, Vec<anyhow::Error>> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::Subscribe {
                server: server.to_string(),
                topic: topic.to_string(),
                respond_to: tx,
            })
            .await
            .map_err(|_| vec![anyhow!("Actor mailbox error")])?;

        rx.await
            .map_err(|_| vec![anyhow!("Actor response error")])?
    }

    pub async fn unsubscribe(&self, server: &str, topic: &str) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::Unsubscribe {
                server: server.to_string(),
                topic: topic.to_string(),
                respond_to: tx,
            })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }

    pub async fn refresh_all(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::RefreshAll { respond_to: tx })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }

    pub async fn list_subscriptions(&self) -> anyhow::Result<Vec<SubscriptionHandle>> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::ListSubscriptions { respond_to: tx })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }

    pub async fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::ListAccounts { respond_to: tx })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }

    pub async fn watch_subscribed(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::WatchSubscribed { respond_to: tx })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }

    pub async fn add_account(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::AddAccount {
                server: server.to_string(),
                username: username.to_string(),
                password: password.to_string(),
                respond_to: tx,
            })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }

    pub async fn remove_account(&self, server: &str) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NtfyMessage::RemoveAccount {
                server: server.to_string(),
                respond_to: tx,
            })
            .await
            .map_err(|_| anyhow!("Actor mailbox error"))?;

        rx.await.map_err(|_| anyhow!("Actor response error"))?
    }
}

pub fn start(
    dbpath: &str,
    notification_proxy: Arc<dyn models::NotificationProxy>,
    network_proxy: Arc<dyn models::NetworkMonitorProxy>,
) -> anyhow::Result<NtfyHandle> {
    let dbpath = dbpath.to_owned();

    // Create a channel to receive the handle from the spawned thread
    let (handle_tx, handle_rx) = oneshot::channel();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Create everything inside the new thread's runtime
        let credentials =
            rt.block_on(async move { crate::credentials::Credentials::new().await.unwrap() });

        let env = SharedEnv {
            db: Db::connect(&dbpath).unwrap(),
            notifier: notification_proxy,
            http_client: HttpClient::new(build_client().unwrap()),
            network_monitor: network_proxy,
            credentials,
        };

        let (mut actor, handle) = NtfyActor::new(env);
        let handle_clone = handle.clone();

        // Send the handle back to the calling thread
        handle_tx.send(handle.clone());

        rt.block_on({
            let local_set = LocalSet::new();
            // Spawn the watch_subscribed task
            local_set.spawn_local(async move {
                if let Err(e) = handle_clone.watch_subscribed().await {
                    error!(error = ?e, "Failed to watch subscribed topics");
                }
            });

            // Run the actor
            local_set.spawn_local(async move {
                actor.run().await;
            });
            local_set
        })
    });

    // Wait for the handle from the spawned thread
    Ok(handle_rx
        .blocking_recv()
        .map_err(|_| anyhow!("Failed to receive actor handle"))?)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use models::Message;
    use tokio::time::sleep;

    use crate::ListenerEvent;

    use super::*;

    #[test]
    fn test_subscribe_and_publish() {
        let notification_proxy = Arc::new(NullNotifier::new());
        let network_proxy = Arc::new(NullNetworkMonitor::new());
        let dbpath = ":memory:";

        let handle = start(dbpath, notification_proxy, network_proxy).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let server = "http://localhost:8000";
            let topic = "test_topic";

            // Subscribe to the topic
            let subscription_handle = handle.subscribe(server, topic).await.unwrap();

            // Publish a message
            let message = serde_json::to_string(&Message {
                topic: topic.to_string(),
                ..Default::default()
            })
            .unwrap();
            let result = subscription_handle.publish(message).await;
            assert!(result.is_ok());

            sleep(Duration::from_millis(250)).await;

            // Attach to the subscription and check if the message is received and stored
            let (events, receiver) = subscription_handle.attach().await;
            dbg!(&events);
            assert!(events.iter().any(|event| match event {
                ListenerEvent::Message(msg) => msg.topic == topic,
                _ => false,
            }));
        });
    }
}
