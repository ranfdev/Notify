use anyhow::{anyhow, Context};
use futures::future::join_all;
use std::{collections::HashMap, future::Future, sync::Arc};
use tokio::{
    sync::{broadcast, mpsc, RwLock},
    task::LocalSet,
};
use tracing::{error, info};

use crate::{
    credentials::{self, Credential},
    http_client::HttpClient,
    listener::{Listener, ListenerCommand, ListenerConfig, ListenerEvent},
    message_repo::Db,
    models::{self, Account},
    topic_listener::build_client,
    SharedEnv,
};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct WatchKey {
    server: String,
    topic: String,
}

#[derive(Clone)]
pub struct Ntfy {
    listener_handles: Arc<RwLock<HashMap<WatchKey, Listener>>>,
    env: SharedEnv,
}

impl Ntfy {
    pub fn new(env: SharedEnv) -> Self {
        Self {
            listener_handles: Default::default(),
            env,
        }
    }
    pub async fn subscribe(
        &self,
        server: &str,
        topic: &str,
    ) -> Result<Listener, Vec<anyhow::Error>> {
        let subscription = models::Subscription::builder(topic.to_owned())
            .server(server.to_string())
            .build()
            .map_err(|e| e.into_iter().map(|e| anyhow!(e)).collect::<Vec<_>>())?;

        let mut db = self.env.db.clone();
        db.insert_subscription(subscription.clone())
            .map_err(|e| vec![anyhow!(e)])?;

        let listener = self.listen(subscription).await;
        listener.map_err(|e| vec![anyhow!(e)])
    }

    pub async fn unsubscribe(&mut self, server: &str, topic: &str) -> anyhow::Result<()> {
        let listener = self.listener_handles.write().await.remove(&WatchKey {
            server: server.to_string(),
            topic: topic.to_string(),
        });
        if let Some(listener) = listener {
            listener.commands.send(ListenerCommand::Shutdown)?;
        }

        self.env.db.remove_subscription(server, topic)?;
        info!(server, topic, "Unsubscribed");
        Ok(())
    }

    // TODO rename reconnect_all
    pub async fn refresh_all(&mut self) -> anyhow::Result<()> {
        for listener in self.listener_handles.read().await.values() {
            listener.commands.send(ListenerCommand::Restart)?;
        }
        Ok(())
    }

    pub async fn list_subscriptions(&mut self) -> anyhow::Result<Vec<Listener>> {
        let values = self
            .listener_handles
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();

        Ok(values)
    }

    pub async fn list_accounts(&mut self) -> anyhow::Result<Vec<Account>> {
        let values = self.env.credentials.list_all();
        let res = values
            .into_iter()
            .map(|(server, credential)| Account {
                server,
                username: credential.username,
            })
            .collect();

        Ok(res)
    }

    pub fn listen(
        &self,
        sub: models::Subscription,
    ) -> impl Future<Output = anyhow::Result<Listener>> {
        let server = sub.server.clone();
        let topic = sub.topic.clone();
        let listener = Listener::new(ListenerConfig {
            http_client: self.env.nullable_http.clone(),
            credentials: self.env.credentials.clone(),
            endpoint: server.clone(),
            topic: topic.clone(),
            since: sub.read_until,
        });
        let listener_handles = self.listener_handles.clone();
        async move {
            listener_handles
                .write()
                .await
                .insert(WatchKey { server, topic }, listener.clone());
            Ok(listener)
        }
    }

    pub async fn watch_subscribed(&mut self) -> anyhow::Result<()> {
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

    fn add_account(&mut self) {}
    fn remove_account(&mut self) {}


pub fn start(
    socket_path: std::path::PathBuf,
    dbpath: &str,
    notification_proxy: Arc<dyn models::NotificationProxy>,
    network_proxy: Arc<dyn models::NetworkMonitorProxy>,
) -> anyhow::Result<Ntfy> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let dbpath = dbpath.to_owned();
    let credentials = rt.block_on(async { crate::credentials::Credentials::new().await.unwrap() });
    let local = tokio::task::LocalSet::new();

    let env = SharedEnv {
        db: Db::connect(&dbpath).unwrap(),
        proxy: notification_proxy,
        http: build_client().unwrap(),
        nullable_http: HttpClient::new(build_client().unwrap()),
        network: network_proxy,
        credentials,
    };
    let ntfy = Ntfy::new(env);
    let mut ntfy_clone = ntfy.clone();
    local.spawn_local(async move {
        ntfy_clone.watch_subscribed().await.unwrap();
    });

    Ok(ntfy)
}

}
