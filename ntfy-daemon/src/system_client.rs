use std::cell::OnceCell;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;
use std::{collections::HashMap, hash::Hash};

use ashpd::desktop::notification::{Notification, NotificationProxy};
use capnp::capability::Promise;
use capnp_rpc::{pry, rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::future::join_all;
use futures::prelude::*;
use generational_arena::Arena;
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use crate::models::Message;
use crate::Error;
use crate::{
    message_repo::Db,
    models::{self, MinMessage},
    ntfy_capnp::ntfy_proxy,
    ntfy_capnp::{output_channel, subscription, system_notifier, watch_handle, Status},
    ntfy_proxy::NtfyProxyImpl,
};

const MESSAGE_THROTTLE: Duration = Duration::from_millis(150);

impl From<Error> for capnp::Error {
    fn from(value: Error) -> Self {
        capnp::Error::failed(format!("{:?}", value))
    }
}

pub struct NotifyForwarder {
    model: Rc<RefCell<models::Subscription>>,
    db: Db,
    watching: Weak<RefCell<Arena<output_channel::Client>>>,
    status: Rc<Cell<Status>>,
}
impl NotifyForwarder {
    pub fn new(
        model: Rc<RefCell<models::Subscription>>,
        db: Db,
        watching: Weak<RefCell<Arena<output_channel::Client>>>,
        status: Rc<Cell<Status>>,
    ) -> Self {
        Self {
            model,
            db,
            watching,
            status,
        }
    }
}

impl output_channel::Server for NotifyForwarder {
    // Stores the message, sends a system notification, forwards the message to watching clients
    fn send_message(
        &mut self,
        params: output_channel::SendMessageParams,
        _results: output_channel::SendMessageResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let request = pry!(params.get());
        let message = pry!(request.get_message());

        // Store in database
        let already_stored: bool = {
            // If this fails parsing, the message is not valid at all.
            // The server is probably misbehaving.
            let min_message: MinMessage = pry!(serde_json::from_str(&message)
                .map_err(|e| Error::InvalidMinMessage(message.to_string(), e)));
            let model = self.model.borrow();
            match self.db.insert_message(&model.server, message) {
                Err(Error::DuplicateMessage) => {
                    warn!(min_message = ?min_message, "Received duplicate message");
                    true
                }
                Err(e) => {
                    error!(min_message = ?min_message, error = ?e, "Can't store the message");
                    false
                }
                _ => false,
            }
        };

        if !already_stored {
            // Show notification
            // Our priority is to show notifications. If anything fails, panic.
            if !{ self.model.borrow().muted } {
                let msg: Message = pry!(serde_json::from_str(&message)
                    .map_err(|e| Error::InvalidMessage(message.to_string(), e)));
                tokio::task::spawn_local(async move {
                    let proxy = match NotificationProxy::new().await {
                        Ok(p) => p,
                        Err(e) => {
                            panic!("Can't show notification: {:?}", e);
                        }
                    };

                    let title = msg.display_title();
                    let title = title.as_ref().map(|x| x.as_str()).unwrap_or(&msg.topic);

                    let n = Notification::new(&title).body(
                        msg.display_message()
                            .as_ref()
                            .map(|x| x.as_str())
                            .unwrap_or(""),
                    );

                    let notification_id = "com.ranfdev.Notify";
                    info!("Showing notification");
                    proxy.add_notification(notification_id, n).await.unwrap();
                });
            }

            // Forward
            if let Some(watching) = self.watching.upgrade() {
                let watching = watching.borrow();
                let futs = watching.iter().map(|(_id, w)| {
                    let mut req = w.send_message_request();
                    req.get().set_message(message);
                    async move {
                        if let Err(e) = req.send().promise.await {
                            error!(error = ?e, "Error forwarding");
                        }
                    }
                });
                tokio::task::spawn_local(join_all(futs));
            }
        }

        Promise::from_future(async move {
            // some backpressure
            tokio::time::sleep(MESSAGE_THROTTLE).await;
            Ok(())
        })
    }

    fn send_status(
        &mut self,
        params: output_channel::SendStatusParams,
        _: output_channel::SendStatusResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let status = pry!(pry!(params.get()).get_status());
        if let Some(watching) = self.watching.upgrade() {
            for (_, w) in watching.borrow().iter() {
                let mut req = w.send_status_request();
                req.get().set_status(status);
                tokio::task::spawn_local(async move {
                    req.send().promise.await.unwrap();
                });
            }
        }
        self.status.set(status);
        Promise::ok(())
    }
}

struct WatcherImpl {
    id: generational_arena::Index,
    watchers: Weak<RefCell<Arena<output_channel::Client>>>,
}

impl watch_handle::Server for WatcherImpl {}

impl Drop for WatcherImpl {
    fn drop(&mut self) {
        if let Some(w) = self.watchers.upgrade() {
            w.borrow_mut().remove(self.id);
        }
    }
}

pub struct SubscriptionImpl {
    model: Rc<RefCell<models::Subscription>>,
    db: Db,
    server: ntfy_proxy::Client,
    server_watch_handle: OnceCell<watch_handle::Client>,
    watchers: Rc<RefCell<Arena<output_channel::Client>>>,
    status: Rc<Cell<Status>>,
}

impl SubscriptionImpl {
    fn new(model: models::Subscription, server: ntfy_proxy::Client, db: Db) -> Self {
        Self {
            model: Rc::new(RefCell::new(model)),
            server,
            db,
            watchers: Default::default(),
            server_watch_handle: Default::default(),
            status: Rc::new(Cell::new(Status::Down)),
        }
    }

    fn output_channel(&self) -> NotifyForwarder {
        NotifyForwarder::new(
            self.model.clone(),
            self.db.clone(),
            Rc::downgrade(&self.watchers),
            self.status.clone(),
        )
    }
}

impl subscription::Server for SubscriptionImpl {
    fn watch(
        &mut self,
        params: subscription::WatchParams,
        mut results: subscription::WatchResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let watcher = pry!(pry!(params.get()).get_watcher());
        let since = pry!(params.get()).get_since();

        // Send old messages
        let msgs = {
            let model = self.model.borrow();
            pry!(self
                .db
                .list_messages(&model.server, &model.topic, since)
                .map_err(Error::Db))
        };

        let futs = msgs.into_iter().map(move |msg| {
            let mut req = watcher.send_message_request();
            req.get().set_message(&msg);
            req.send().promise
        });

        let watcher = pry!(pry!(params.get()).get_watcher());
        let mut req = watcher.send_status_request();
        req.get().set_status(self.status.get());

        let id = { self.watchers.borrow_mut().insert(watcher) };

        results.get().set_handle(capnp_rpc::new_client(WatcherImpl {
            id,
            watchers: Rc::downgrade(&self.watchers),
        }));

        Promise::from_future(async move {
            futures::future::try_join_all(futs).await?;
            req.send().promise.await?;
            Ok(())
        })
    }
    fn publish(
        &mut self,
        params: subscription::PublishParams,
        _results: subscription::PublishResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let msg = pry!(pry!(params.get()).get_message());

        let mut req = self.server.publish_request();
        req.get().set_message(msg);

        Promise::from_future(async move {
            req.send().promise.await?;
            Ok(())
        })
    }
    fn get_info(
        &mut self,
        _: subscription::GetInfoParams,
        mut results: subscription::GetInfoResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let mut res = results.get();
        let model = self.model.borrow();
        res.set_server(&model.server);
        res.set_display_name(&model.display_name);
        res.set_topic(&model.topic);
        res.set_muted(model.muted);
        res.set_read_until(model.read_until);
        Promise::ok(())
    }
    fn update_info(
        &mut self,
        params: subscription::UpdateInfoParams,
        _results: subscription::UpdateInfoResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let info = pry!(pry!(params.get()).get_value());
        let mut model = self.model.borrow_mut();
        model.display_name = pry!(info.get_display_name()).to_string();
        model.muted = info.get_muted();
        model.read_until = info.get_read_until();
        pry!(self.db.update_subscription(model.clone()));
        Promise::ok(())
    }
    fn clear_notifications(
        &mut self,
        _params: subscription::ClearNotificationsParams,
        _results: subscription::ClearNotificationsResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let model = self.model.borrow_mut();
        pry!(self.db.delete_messages(&model.server, &model.topic));
        Promise::ok(())
    }

    fn update_read_until(
        &mut self,
        params: subscription::UpdateReadUntilParams,
        _: subscription::UpdateReadUntilResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let value = pry!(params.get()).get_value();
        let mut model = self.model.borrow_mut();
        pry!(self
            .db
            .update_read_until(&model.server, &model.topic, value));
        model.read_until = value;
        Promise::ok(())
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct WatchKey {
    server: String,
    topic: String,
}
pub struct SystemNotifier {
    servers: HashMap<String, ntfy_proxy::Client>,
    watching: Rc<RefCell<HashMap<WatchKey, subscription::Client>>>,
    db: Db,
}

impl SystemNotifier {
    pub fn new(dbpath: &str) -> Self {
        Self {
            servers: HashMap::new(),
            watching: Rc::new(RefCell::new(HashMap::new())),
            db: Db::connect(dbpath).unwrap(),
        }
    }
    fn watch(&mut self, sub: models::Subscription) -> Promise<subscription::Client, capnp::Error> {
        let ntfy = self
            .servers
            .entry(sub.server.to_owned())
            .or_insert_with(|| capnp_rpc::new_client(NtfyProxyImpl::new(sub.server.to_owned())));

        let subscription = SubscriptionImpl::new(sub.clone(), ntfy.clone(), self.db.clone());

        let mut req = ntfy.watch_request();
        req.get().set_topic(&sub.topic);
        req.get()
            .set_watcher(capnp_rpc::new_client(subscription.output_channel()));
        let res = req.send();
        let handle = res.pipeline.get_handle();
        subscription
            .server_watch_handle
            .set(handle)
            .map_err(|_| "already set")
            .unwrap();

        let watching = self.watching.clone();
        let subc: subscription::Client = capnp_rpc::new_client(subscription);

        Promise::from_future(async move {
            res.promise
                .await
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            watching.borrow_mut().insert(
                WatchKey {
                    server: sub.server.to_owned(),
                    topic: sub.topic.to_owned(),
                },
                subc.clone(),
            );
            Ok(subc)
        })
    }
    pub fn watch_subscribed(&mut self) -> Promise<(), capnp::Error> {
        let f: Vec<_> = pry!(self.db.list_subscriptions())
            .into_iter()
            .map(|m| self.watch(m.clone()))
            .collect();
        Promise::from_future(async move {
            join_all(f.into_iter().map(|x| async move {
                if let Err(e) = x.await {
                    error!(error = ?e, "Can't rewatch subscribed topic");
                }
            }))
            .await;
            Ok(())
        })
    }
}

impl system_notifier::Server for SystemNotifier {
    fn subscribe(
        &mut self,
        params: system_notifier::SubscribeParams,
        mut results: system_notifier::SubscribeResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let topic = pry!(pry!(params.get()).get_topic());
        let server: &str = pry!(pry!(params.get()).get_server());
        let server = if server.is_empty() {
            "https://ntfy.sh"
        } else {
            ""
        };

        let subscription = pry!(
            models::Subscription::builder(server.to_owned(), topic.to_owned())
                .build()
                .map_err(|e| capnp::Error::failed(e.to_string()))
        );
        let sub: Promise<subscription::Client, capnp::Error> = self.watch(subscription.clone());

        let mut db = self.db.clone();
        Promise::from_future(async move {
            results.get().set_subscription(sub.await?);

            db.insert_subscription(subscription).map_err(|e| {
                capnp::Error::failed(format!("could not insert subscription: {}", e))
            })?;
            Ok(())
        })
    }
    fn unsubscribe(
        &mut self,
        params: system_notifier::UnsubscribeParams,
        _results: system_notifier::UnsubscribeResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let topic = pry!(pry!(params.get()).get_topic());
        let server = pry!(pry!(params.get()).get_server());
        {
            self.watching.borrow_mut().remove(&WatchKey {
                server: server.to_string(),
                topic: topic.to_string(),
            });
            pry!(self
                .db
                .remove_subscription(&server, &topic)
                .map_err(|e| capnp::Error::failed(e.to_string())));
            info!(server, topic, "Unsubscribed");
        }
        Promise::ok(())
    }
    fn list_subscriptions(
        &mut self,
        _: system_notifier::ListSubscriptionsParams,
        mut results: system_notifier::ListSubscriptionsResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let req = results.get();
        let values = self.watching.borrow().values().cloned().collect::<Vec<_>>();
        let mut list = req.init_list(values.len() as u32);

        for (i, v) in values.iter().enumerate() {
            use capnp::capability::FromClientHook;
            list.set(i as u32, v.clone().clone().into_client_hook());
        }

        Promise::ok(())
    }
}

pub fn start(socket_path: std::path::PathBuf, dbpath: &str) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let listener = rt.block_on(async move {
        let _ = std::fs::remove_file(&socket_path);
        UnixListener::bind(&socket_path).unwrap()
    });

    let dbpath = dbpath.to_owned();
    let f = move || {
        let local = tokio::task::LocalSet::new();
        let mut system_notifier = SystemNotifier::new(&dbpath);
        local.spawn_local(async move {
            system_notifier.watch_subscribed().await.unwrap();
            let system_client: system_notifier::Client = capnp_rpc::new_client(system_notifier);

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        info!("client connected");
                        let (reader, writer) =
                            tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
                        let network = twoparty::VatNetwork::new(
                            reader,
                            writer,
                            rpc_twoparty_capnp::Side::Server,
                            Default::default(),
                        );

                        let rpc_system =
                            RpcSystem::new(Box::new(network), Some(system_client.clone().client));

                        tokio::task::spawn_local(rpc_system);
                    }
                    Err(e) => {
                        error!(error=%e);
                    }
                }
            }
        });
        rt.block_on(local);
    };
    std::thread::spawn(move || {
        f();
    });

    Ok(())
}
