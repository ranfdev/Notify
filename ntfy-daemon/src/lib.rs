pub mod credentials;
pub mod message_repo;
pub mod models;
pub mod retry;
pub mod system_client;
pub mod topic_listener;
mod http_client;
mod output_tracker;
mod listener;
mod ntfy;
mod subscription;

pub use subscription::SubscriptionHandle;
pub use listener::*;
pub use ntfy::NtfyHandle;
pub use ntfy::start;

pub mod ntfy_capnp {
    include!(concat!(env!("OUT_DIR"), "/src/ntfy_capnp.rs"));
}

use std::sync::Arc;

use http_client::HttpClient;

#[derive(Clone)]
pub struct SharedEnv {
    db: message_repo::Db,
    proxy: Arc<dyn models::NotificationProxy>,
    http: reqwest::Client,
    nullable_http: HttpClient,
    network: Arc<dyn models::NetworkMonitorProxy>,
    credentials: credentials::Credentials,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("topic {0} must not be empty and must contain only alphanumeric characters and _ (underscore)")]
    InvalidTopic(String),
    #[error("invalid server base url {0:?}")]
    InvalidServer(#[from] url::ParseError),
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

impl From<Error> for capnp::Error {
    fn from(value: Error) -> Self {
        capnp::Error::failed(format!("{:?}", value))
    }
}
