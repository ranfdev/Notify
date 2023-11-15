use std::cell::{Cell, OnceCell, RefCell};
use std::ops::ControlFlow;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, hash::Hash};

use capnp::capability::Promise;
use capnp_rpc::{pry, rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::future::join_all;
use futures::prelude::*;
use generational_arena::Arena;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::models::Message;
use crate::Error;
use crate::SharedEnv;
use crate::{
    message_repo::Db,
    models::{self, MinMessage},
    ntfy_capnp::{account, output_channel, subscription, system_notifier, watch_handle, Status},
    topic_listener::{build_client, TopicListener},
};

const MESSAGE_THROTTLE: Duration = Duration::from_millis(150);

pub struct NotifyForwarder {
    model: Rc<RefCell<models::Subscription>>,
    env: SharedEnv,
    watching: Weak<RefCell<Arena<output_channel::Client>>>,
    status: Rc<Cell<Status>>,
}
impl NotifyForwarder {
    pub fn new(
        model: Rc<RefCell<models::Subscription>>,
        env: SharedEnv,
        watching: Weak<RefCell<Arena<output_channel::Client>>>,
        status: Rc<Cell<Status>>,
    ) -> Self {
        Self {
            model,
            env,
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
        let message = pry!(pry!(request.get_message()).to_str());

        // Store in database
        let already_stored: bool = {
            // If this fails parsing, the message is not valid at all.
            // The server is probably misbehaving.
            let min_message: MinMessage = pry!(serde_json::from_str(message)
                .map_err(|e| Error::InvalidMinMessage(message.to_string(), e)));
            let model = self.model.borrow();
            match self.env.db.insert_message(&model.server, message) {
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
                let msg: Message = pry!(serde_json::from_str(message)
                    .map_err(|e| Error::InvalidMessage(message.to_string(), e)));
                let np = self.env.proxy.clone();

                let title = { msg.notification_title(&self.model.borrow()) };

                let n = models::Notification {
                    title,
                    body: msg.display_message().as_deref().unwrap_or("").to_string(),
                    actions: msg.actions,
                };

                info!("Showing notification");
                np.send(n).unwrap();
            }

            // Forward
            if let Some(watching) = self.watching.upgrade() {
                let watching = watching.borrow();
                let futs = watching.iter().map(|(_id, w)| {
                    let mut req = w.send_message_request();
                    req.get().set_message(message.into());
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
    env: SharedEnv,
    watchers: Rc<RefCell<Arena<output_channel::Client>>>,
    status: Rc<Cell<Status>>,
    topic_listener: mpsc::Sender<ControlFlow<()>>,
}

impl Drop for SubscriptionImpl {
    fn drop(&mut self) {
        let t = self.topic_listener.clone();
        tokio::task::spawn_local(async move {
            t.send(ControlFlow::Break(())).await.unwrap();
        });
    }
}

impl SubscriptionImpl {
    fn new(model: models::Subscription, env: SharedEnv) -> Self {
        let status = Rc::new(Cell::new(Status::Down));
        let watchers = Default::default();
        let rc_model = Rc::new(RefCell::new(model.clone()));
        let output_channel = NotifyForwarder::new(
            rc_model.clone(),
            env.clone(),
            Rc::downgrade(&watchers),
            status.clone(),
        );
        let topic_listener = TopicListener::new(
            env.clone(),
            model.server.clone(),
            model.topic.clone(),
            model.read_until,
            capnp_rpc::new_client(output_channel),
        );
        Self {
            model: rc_model,
            env,
            watchers,
            status,
            topic_listener,
        }
    }

    fn _publish<'a>(&'a mut self, msg: &'a str) -> impl Future<Output = Result<(), capnp::Error>> {
        let msg = msg.to_owned();
        let req = self.env.http.post(&self.model.borrow().server).body(msg);

        async move {
            info!("sending message");
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
                .env
                .db
                .list_messages(&model.server, &model.topic, since)
                .map_err(Error::Db))
        };

        let futs = msgs.into_iter().map(move |msg| {
            let mut req = watcher.send_message_request();
            req.get().set_message(msg.as_str().into());
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
        let msg = pry!(pry!(pry!(params.get()).get_message()).to_str());
        let fut = self._publish(msg);

        Promise::from_future(async move {
            fut.await?;
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
        res.set_server(model.server.as_str().into());
        res.set_display_name(model.display_name.as_str().into());
        res.set_topic(model.topic.as_str().into());
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
        model.display_name = pry!(pry!(info.get_display_name()).to_string());
        model.muted = info.get_muted();
        model.read_until = info.get_read_until();
        pry!(self.env.db.update_subscription(model.clone()));
        Promise::ok(())
    }
    fn clear_notifications(
        &mut self,
        _params: subscription::ClearNotificationsParams,
        _results: subscription::ClearNotificationsResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let model = self.model.borrow_mut();
        pry!(self.env.db.delete_messages(&model.server, &model.topic));
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
            .env
            .db
            .update_read_until(&model.server, &model.topic, value));
        model.read_until = value;
        Promise::ok(())
    }
    fn refresh(
        &mut self,
        _: subscription::RefreshParams,
        _: subscription::RefreshResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let sender = self.topic_listener.clone();
        Promise::from_future(async move {
            sender
                .send(ControlFlow::Continue(()))
                .await
                .map_err(|e| capnp::Error::failed(format!("{:?}", e)))?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct WatchKey {
    server: String,
    topic: String,
}
pub struct SystemNotifier {
    watching: Rc<RefCell<HashMap<WatchKey, subscription::Client>>>,
    env: SharedEnv,
}

impl SystemNotifier {
    pub fn new(
        dbpath: &str,
        notification_proxy: Arc<dyn models::NotificationProxy>,
        network: Arc<dyn models::NetworkMonitorProxy>,
        credentials: crate::credentials::Credentials,
    ) -> Self {
        Self {
            watching: Rc::new(RefCell::new(HashMap::new())),
            env: SharedEnv {
                db: Db::connect(dbpath).unwrap(),
                proxy: notification_proxy,
                http: build_client().unwrap(),
                network,
                credentials,
            },
        }
    }
    fn watch(&mut self, sub: models::Subscription) -> Promise<subscription::Client, capnp::Error> {
        let subscription = SubscriptionImpl::new(sub.clone(), self.env.clone());

        let watching = self.watching.clone();
        let subc: subscription::Client = capnp_rpc::new_client(subscription);

        Promise::from_future(async move {
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
        let f: Vec<_> = pry!(self.env.db.list_subscriptions())
            .into_iter()
            .map(|m| self.watch(m))
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
    pub fn refresh_all(&mut self) -> Promise<(), capnp::Error> {
        let watching = self.watching.clone();
        Promise::from_future(async move {
            let reqs: Vec<_> = watching
                .borrow()
                .values()
                .map(|w| w.refresh_request())
                .collect();
            join_all(reqs.into_iter().map(|x| x.send().promise)).await;
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
        let topic = pry!(pry!(pry!(params.get()).get_topic()).to_str());
        let server: &str = pry!(pry!(pry!(params.get()).get_server()).to_str());

        let subscription = pry!(models::Subscription::builder(topic.to_owned())
            .server(server.to_string())
            .build()
            .map_err(|e| capnp::Error::failed(format!("{:?}", e))));
        let sub: Promise<subscription::Client, capnp::Error> = self.watch(subscription.clone());

        let mut db = self.env.db.clone();
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
        let topic = pry!(pry!(pry!(params.get()).get_topic()).to_str());
        let server = pry!(pry!(pry!(params.get()).get_server()).to_str());
        {
            self.watching.borrow_mut().remove(&WatchKey {
                server: server.to_string(),
                topic: topic.to_string(),
            });
            pry!(self
                .env
                .db
                .remove_subscription(server, topic)
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
    fn list_accounts(
        &mut self,
        _: system_notifier::ListAccountsParams,
        mut results: system_notifier::ListAccountsResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let values = self.env.credentials.list_all();

        Promise::from_future(async move {
            let mut list = results.get().init_list(values.len() as u32);
            for (i, item) in values.into_iter().enumerate() {
                let mut acc = list.reborrow().get(i as u32);
                acc.set_server(item.0[..].into());
                acc.set_username(item.1.username[..].into());
            }
            Ok(())
        })
    }
    fn add_account(
        &mut self,
        params: system_notifier::AddAccountParams,
        _: system_notifier::AddAccountResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let credentials = self.env.credentials.clone();
        let http = self.env.http.clone();
        let refresh = self.refresh_all();
        Promise::from_future(async move {
            let account = params.get()?.get_account()?;
            let username = account.get_username()?.to_str()?;
            let server = account.get_server()?.to_str()?;
            let password = params.get()?.get_password()?.to_str()?;

            info!("validating account");
            let url = models::Subscription::build_auth_url(server, "stats")?;

            http.get(url)
                .basic_auth(username, Some(password))
                .send()
                .await
                .map_err(|e| capnp::Error::failed(e.to_string()))?
                .error_for_status()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;

            credentials
                .insert(server, username, password)
                .await
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            refresh.await?;

            info!(server = %server, username = %username, "added account");

            Ok(())
        })
    }
    fn remove_account(
        &mut self,
        params: system_notifier::RemoveAccountParams,
        _: system_notifier::RemoveAccountResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let credentials = self.env.credentials.clone();
        Promise::from_future(async move {
            let account = params.get()?.get_account()?;
            let username = account.get_username()?.to_str()?;
            let server = account.get_server()?.to_str()?;

            credentials
                .delete(server)
                .await
                .map_err(|e| capnp::Error::failed(e.to_string()))?;

            info!(server = %server, username = %username, "removed account");

            Ok(())
        })
    }
}

pub fn start(
    socket_path: std::path::PathBuf,
    dbpath: &str,
    notification_proxy: Arc<dyn models::NotificationProxy>,
    network_proxy: Arc<dyn models::NetworkMonitorProxy>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let listener = rt.block_on(async move {
        let _ = std::fs::remove_file(&socket_path);
        UnixListener::bind(&socket_path).unwrap()
    });

    let dbpath = dbpath.to_owned();
    let f = move || {
        let credentials =
            rt.block_on(async { crate::credentials::Credentials::new().await.unwrap() });
        let local = tokio::task::LocalSet::new();
        let mut system_notifier =
            SystemNotifier::new(&dbpath, notification_proxy, network_proxy, credentials);
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
