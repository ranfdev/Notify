use crate::listener::{ListenerEvent, ListenerHandle};
use crate::message_repo::Db;
use crate::models::{self, Message, NotificationProxy};
use crate::{Error, ServerEvent, SharedEnv};
use std::future::Future;
use std::sync::Arc;
use tokio::select;
use tokio::sync::{broadcast, mpsc, oneshot, watch, RwLock};
use tokio::task::spawn_local;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct SubscriptionHandle {
    sender: mpsc::Sender<SubscriptionRequest>,
    listener: ListenerHandle,
}

impl SubscriptionHandle {
    pub fn new(listener: ListenerHandle, model: models::Subscription, env: &SharedEnv) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let broadcast_tx = broadcast::channel(8).0;
        let actor = SubscriptionActor {
            listener: listener.clone(),
            model,
            receiver,
            env: env.clone(),
            broadcast_tx: broadcast_tx.clone(),
        };
        spawn_local(actor.run());
        Self { sender, listener }
    }

    pub async fn model(&self) -> models::Subscription {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.sender
            .send(SubscriptionRequest::GetModel { resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn update_info(&self, new_model: models::Subscription) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.sender
            .send(SubscriptionRequest::UpdateInfo { new_model, resp_tx })
            .await?;
        resp_rx.await.unwrap()
    }

    pub async fn restart(&self) -> anyhow::Result<()> {
        self.listener
            .commands
            .send(crate::ListenerCommand::Restart)
            .await?;
        Ok(())
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.listener
            .commands
            .send(crate::ListenerCommand::Shutdown)
            .await?;
        Ok(())
    }

    // returns a vector containing all the past messages stored in the database and the current connection state.
    // The first vector is useful to get a summary of what happened before.
    // The `ListenerHandle` is returned to receive new events.
    pub async fn attach(&self) -> (Vec<ListenerEvent>, broadcast::Receiver<ListenerEvent>) {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.sender
            .send(SubscriptionRequest::Attach { resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn publish(&self, msg: String) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.sender
            .send(SubscriptionRequest::Publish { msg, resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn clear_notifications(&self) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.sender
            .send(SubscriptionRequest::ClearNotifications { resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn update_read_until(&self, timestamp: u64) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.sender
            .send(SubscriptionRequest::UpdateReadUntil { timestamp, resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }
}

struct SubscriptionActor {
    listener: ListenerHandle,
    model: models::Subscription,
    receiver: mpsc::Receiver<SubscriptionRequest>,
    env: SharedEnv,
    broadcast_tx: broadcast::Sender<ListenerEvent>,
}

impl SubscriptionActor {
    async fn run(mut self) {
        loop {
            select! {
                Ok(event) = self.listener.events.recv() => {
                    match event {
                        ListenerEvent::Message(msg) => self.handle_msg_event(msg),
                        other => {
                            let _ = self.broadcast_tx.send(other);
                        }
                    }
                }
                Some(request) = self.receiver.recv() => {
                    match request {
                        SubscriptionRequest::GetModel { resp_tx } => {
                            let _ = resp_tx.send(self.model.clone());
                        }
                        SubscriptionRequest::UpdateInfo {
                            mut new_model,
                            resp_tx,
                        } => {
                            new_model.server = self.model.server.clone();
                            new_model.topic = self.model.topic.clone();
                            let res = self.env.db.update_subscription(new_model.clone());
                            if let Ok(_) = res {
                                self.model = new_model;
                            }
                            resp_tx.send(res.map_err(|e| e.into()));
                        }
                        SubscriptionRequest::Publish {msg, resp_tx} => {
                            let _ = resp_tx.send(self.publish(msg).await);
                        }
                        SubscriptionRequest::Attach { resp_tx } => {
                            let messages = self
                            .env
                                .db
                                .list_messages(&self.model.server, &self.model.topic, 0)
                                .unwrap_or_default();
                            let mut previous_events: Vec<ListenerEvent> = messages
                                .into_iter()
                                .filter_map(|msg| {
                                    let msg = serde_json::from_str(&msg);
                                    match msg {
                                        Err(e) => {
                                            error!(error = ?e, "error parsing stored message");
                                            None
                                        }
                                        Ok(msg) => Some(msg),
                                    }
                                })
                                .map(ListenerEvent::Message)
                                .collect();
                            previous_events.push(ListenerEvent::ConnectionStateChanged(self.listener.request_state().await));
                            let _ = resp_tx.send((previous_events, self.broadcast_tx.subscribe()));
                        }
                        SubscriptionRequest::ClearNotifications {resp_tx} => {
                            let _ = resp_tx.send(self.env.db.delete_messages(&self.model.server, &self.model.topic).map_err(|e| anyhow::anyhow!(e)));
                        }
                        SubscriptionRequest::UpdateReadUntil { timestamp, resp_tx } => {
                            let res = self.env.db.update_read_until(&self.model.server, &self.model.topic, timestamp);
                            let _ = resp_tx.send(res.map_err(|e| anyhow::anyhow!(e)));
                        }
                    }
                }
            }
        }
    }

    async fn publish(&self, msg: String) -> anyhow::Result<()> {
        let server = &self.model.server;
        let creds = self.env.credentials.get(server);
        let mut req = self.env.http_client.post(server);
        if let Some(creds) = creds {
            req = req.basic_auth(creds.username, Some(creds.password));
        }

        info!("sending message");
        let res = req.body(msg).send().await?;
        res.error_for_status()?;
        Ok(())
    }
    fn handle_msg_event(&mut self, msg: Message) {
        // Store in database
        let already_stored: bool = {
            let json_ev = &serde_json::to_string(&msg).unwrap();
            match self.env.db.insert_message(&self.model.server, json_ev) {
                Err(Error::DuplicateMessage) => {
                    warn!("Received duplicate message");
                    true
                }
                Err(e) => {
                    error!(error = ?e, "Can't store the message");
                    false
                }
                _ => false,
            }
        };

        if !already_stored {
            // Show notification. If this fails, panic
            if !{ self.model.muted } {
                let notifier = self.env.notifier.clone();

                let title = { msg.notification_title(&self.model) };

                let n = models::Notification {
                    title,
                    body: msg.display_message().as_deref().unwrap_or("").to_string(),
                    actions: msg.actions.clone(),
                };

                info!("Showing notification");
                notifier.send(n).unwrap();
            }

            // Forward to app
            let _ = self.broadcast_tx.send(ListenerEvent::Message(msg));
        }
    }
}

enum SubscriptionRequest {
    GetModel {
        resp_tx: oneshot::Sender<models::Subscription>,
    },
    UpdateInfo {
        new_model: models::Subscription,
        resp_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    Attach {
        resp_tx: oneshot::Sender<(Vec<ListenerEvent>, broadcast::Receiver<ListenerEvent>)>,
    },
    Publish {
        msg: String,
        resp_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    ClearNotifications {
        resp_tx: oneshot::Sender<anyhow::Result<()>>,
    },
    UpdateReadUntil {
        timestamp: u64,
        resp_tx: oneshot::Sender<anyhow::Result<()>>,
    },
}
