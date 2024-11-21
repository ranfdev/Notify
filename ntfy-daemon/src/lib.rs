mod actor_utils;
pub mod credentials;
mod http_client;
mod listener;
pub mod message_repo;
pub mod models;
mod ntfy;
mod output_tracker;
pub mod retry;
mod subscription;

pub use listener::*;
pub use ntfy::start;
pub use ntfy::NtfyHandle;
use std::sync::Arc;
pub use subscription::SubscriptionHandle;

use http_client::HttpClient;

#[derive(Clone)]
pub struct SharedEnv {
    db: message_repo::Db,
    notifier: Arc<dyn models::NotificationProxy>,
    http_client: HttpClient,
    network_monitor: Arc<dyn models::NetworkMonitorProxy>,
    credentials: credentials::Credentials,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("topic {0} must not be empty and must contain only alphanumeric characters and _ (underscore)")]
    InvalidTopic(String),
    #[error("invalid server base url {0:?}")]
    InvalidServer(#[from] url::ParseError),
    #[error("multiple errors in subscription model: {0:?}")]
    InvalidSubscription(Vec<Error>),
    #[error("duplicate message")]
    DuplicateMessage,
    #[error("can't parse the minimum set of required fields from the message {0}")]
    InvalidMinMessage(String, #[source] serde_json::Error),
    #[error("can't parse the complete message {0}")]
    InvalidMessage(String, #[source] serde_json::Error),
    #[error("database error")]
    Db(#[from] rusqlite::Error),
    #[error("subscription not found while {0}")]
    SubscriptionNotFound(String),
}
