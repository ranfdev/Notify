use crate::listener::{ListenerEvent, ListenerHandle};
use crate::models::{self, ReceivedMessage};
use crate::{Error, SharedEnv};
use tokio::select;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::spawn_local;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug)]
enum SubscriptionCommand {
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

#[derive(Clone)]
pub struct SubscriptionHandle {
    command_tx: mpsc::Sender<SubscriptionCommand>,
    listener: ListenerHandle,
}

impl SubscriptionHandle {
    pub fn new(listener: ListenerHandle, model: models::Subscription, env: &SharedEnv) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);
        let broadcast_tx = broadcast::channel(8).0;
        let actor = SubscriptionActor {
            listener: listener.clone(),
            model,
            command_rx,
            env: env.clone(),
            broadcast_tx: broadcast_tx.clone(),
        };
        spawn_local(actor.run());
        Self {
            command_tx,
            listener,
        }
    }

    pub async fn model(&self) -> models::Subscription {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.command_tx
            .send(SubscriptionCommand::GetModel { resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn update_info(&self, new_model: models::Subscription) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.command_tx
            .send(SubscriptionCommand::UpdateInfo { new_model, resp_tx })
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
        self.command_tx
            .send(SubscriptionCommand::Attach { resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn publish(&self, msg: String) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.command_tx
            .send(SubscriptionCommand::Publish { msg, resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn clear_notifications(&self) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.command_tx
            .send(SubscriptionCommand::ClearNotifications { resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }

    pub async fn update_read_until(&self, timestamp: u64) -> anyhow::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.command_tx
            .send(SubscriptionCommand::UpdateReadUntil { timestamp, resp_tx })
            .await
            .unwrap();
        resp_rx.await.unwrap()
    }
}

struct SubscriptionActor {
    listener: ListenerHandle,
    model: models::Subscription,
    command_rx: mpsc::Receiver<SubscriptionCommand>,
    env: SharedEnv,
    broadcast_tx: broadcast::Sender<ListenerEvent>,
}

impl SubscriptionActor {
    async fn run(mut self) {
        loop {
            select! {
                Ok(event) = self.listener.events.recv() => {
                    debug!(?event, "received listener event");
                    match event {
                        ListenerEvent::Message(msg) => self.handle_msg_event(msg),
                        other => {
                            let _ = self.broadcast_tx.send(other);
                        }
                    }
                }
                Some(command) = self.command_rx.recv() => {
                    trace!(?command, "processing subscription command");
                    match command {
                        SubscriptionCommand::GetModel { resp_tx } => {
                            debug!("getting subscription model");
                            let _ = resp_tx.send(self.model.clone());
                        }
                        SubscriptionCommand::UpdateInfo {
                            mut new_model,
                            resp_tx,
                        } => {
                            debug!(server=?new_model.server, topic=?new_model.topic, "updating subscription info");
                            new_model.server = self.model.server.clone();
                            new_model.topic = self.model.topic.clone();
                            let res = self.env.db.update_subscription(new_model.clone());
                            if let Ok(_) = res {
                                self.model = new_model;
                            }
                            let _ = resp_tx.send(res.map_err(|e| e.into()));
                        }
                        SubscriptionCommand::Publish {msg, resp_tx} => {
                            debug!(topic=?self.model.topic, "publishing message");
                            let _ = resp_tx.send(self.publish(msg).await);
                        }
                        SubscriptionCommand::Attach { resp_tx } => {
                            debug!(topic=?self.model.topic, "attaching new listener");
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
                            previous_events.push(ListenerEvent::ConnectionStateChanged(self.listener.state().await));
                            let _ = resp_tx.send((previous_events, self.broadcast_tx.subscribe()));
                        }
                        SubscriptionCommand::ClearNotifications {resp_tx} => {
                            debug!(topic=?self.model.topic, "clearing notifications");
                            let _ = resp_tx.send(self.env.db.delete_messages(&self.model.server, &self.model.topic).map_err(|e| anyhow::anyhow!(e)));
                        }
                        SubscriptionCommand::UpdateReadUntil { timestamp, resp_tx } => {
                            debug!(topic=?self.model.topic, timestamp=timestamp, "updating read until timestamp");
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
        debug!(server=?server, "preparing to publish message");
        let creds = self.env.credentials.get(server);
        let mut req = self.env.http_client.post(server);
        if let Some(creds) = creds {
            req = req.basic_auth(creds.username, Some(creds.password));
        }

        info!(server=?server, "sending message");
        let res = req.body(msg).send().await?;
        res.error_for_status()?;
        debug!(server=?server, "message published successfully");
        Ok(())
    }
    fn handle_msg_event(&mut self, msg: ReceivedMessage) {
        debug!(topic=?self.model.topic, "handling new message");
        // Store in database
        let already_stored: bool = {
            let json_ev = &serde_json::to_string(&msg).unwrap();
            match self.env.db.insert_message(&self.model.server, json_ev) {
                Err(Error::DuplicateMessage) => {
                    warn!(topic=?self.model.topic, "received duplicate message");
                    true
                }
                Err(e) => {
                    error!(error=?e, topic=?self.model.topic, "can't store the message");
                    false
                }
                _ => {
                    debug!(topic=?self.model.topic, "message stored successfully");
                    false
                }
            }
        };

        if !already_stored {
            debug!(topic=?self.model.topic, muted=?self.model.muted, "checking if notification should be shown");
            // Show notification. If this fails, panic
            if !{ self.model.muted } {
                let notifier = self.env.notifier.clone();

                let title = { msg.notification_title(&self.model) };

                let n = models::Notification {
                    title,
                    body: msg.display_message().as_deref().unwrap_or("").to_string(),
                    actions: msg.actions.clone(),
                };

                debug!(topic=?self.model.topic, "sending notification through proxy");
                notifier.send(n).unwrap();
            } else {
                debug!(topic=?self.model.topic, "notification muted, skipping");
            }

            // Forward to app
            debug!(topic=?self.model.topic, "forwarding message to app");
            let _ = self.broadcast_tx.send(ListenerEvent::Message(msg));
        }
    }
}
