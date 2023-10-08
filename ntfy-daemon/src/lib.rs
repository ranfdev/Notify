pub mod message_repo;
pub mod models;
pub mod ntfy_proxy;
pub mod retry;
pub mod system_client;
pub mod ntfy_capnp {
    include!(concat!(env!("OUT_DIR"), "/src/ntfy_capnp.rs"));
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("topic {0} must not be empty and must contain only alphanumeric characters and _ (underscore)")]
    InvalidTopic(String),
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
