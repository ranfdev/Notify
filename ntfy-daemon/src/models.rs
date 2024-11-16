use std::collections::HashMap;
use std::pin::Pin;
use std::sync::OnceLock;

use futures::stream::Stream;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::Error;

pub const DEFAULT_SERVER: &str = "https://ntfy.sh";
static EMOJI_MAP: OnceLock<HashMap<String, String>> = OnceLock::new();

fn emoji_map() -> &'static HashMap<String, String> {
    EMOJI_MAP.get_or_init(move || {
        serde_json::from_str(include_str!("../data/mailer_emoji_map.json")).unwrap()
    })
}

pub fn validate_topic(topic: &str) -> Result<&str, Error> {
    let re = Regex::new(r"^[\w\-]{1,64}$").unwrap();
    if re.is_match(topic) {
        Ok(topic)
    } else {
        Err(Error::InvalidTopic(topic.to_string()))
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub topic: String,
    pub message: Option<String>,
    #[serde(default = "Default::default")]
    pub time: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub attachment: Option<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

impl Message {
    fn extend_with_emojis(&self, text: &mut String) {
        // Add emojis
        for t in &self.tags {
            if let Some(emoji) = emoji_map().get(t) {
                text.push_str(emoji);
            }
        }
    }
    pub fn display_title(&self) -> Option<String> {
        self.title.as_ref().map(|title| {
            let mut title_text = String::new();
            self.extend_with_emojis(&mut title_text);

            if !title_text.is_empty() {
                title_text.push(' ');
            }

            title_text.push_str(title);
            title_text
        })
    }
    pub fn notification_title(&self, subscription: &Subscription) -> String {
        self.display_title()
            .or(if subscription.display_name.is_empty() {
                None
            } else {
                Some(subscription.display_name.to_string())
            })
            .unwrap_or(self.topic.to_string())
    }

    pub fn display_message(&self) -> Option<String> {
        self.message.as_ref().map(|message| {
            let mut out = String::new();
            if self.title.is_none() {
                self.extend_with_emojis(&mut out);
            }
            if !out.is_empty() {
                out.push(' ');
            }

            out.push_str(message);
            out
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MinMessage {
    pub id: String,
    pub topic: String,
    pub time: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub url: url::Url,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<usize>,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        let Some(ext) = self.name.split('.').last() else {
            return false;
        };
        ["jpeg", "jpg", "png", "webp", "gif"].contains(&ext)
    }
}

#[derive(Clone, Debug)]
pub struct Subscription {
    pub server: String,
    pub topic: String,
    pub display_name: String,
    pub muted: bool,
    pub archived: bool,
    pub reserved: bool,
    pub symbolic_icon: Option<String>,
    pub read_until: u64,
}

impl Subscription {
    pub fn build_url(server: &str, topic: &str, since: u64) -> Result<url::Url, crate::Error> {
        let mut url = url::Url::parse(server)?;
        url.path_segments_mut()
            .map_err(|_| url::ParseError::RelativeUrlWithCannotBeABaseBase)?
            .push(topic)
            .push("json");
        url.query_pairs_mut()
            .append_pair("since", &since.to_string());
        Ok(url)
    }
    pub fn build_auth_url(server: &str, topic: &str) -> Result<url::Url, crate::Error> {
        let mut url = url::Url::parse(server)?;
        url.path_segments_mut()
            .map_err(|_| url::ParseError::RelativeUrlWithCannotBeABaseBase)?
            .push(topic)
            .push("auth");
        Ok(url)
    }
    pub fn validate(self) -> Result<Self, Vec<crate::Error>> {
        let mut errs = vec![];
        if let Err(e) = validate_topic(&self.topic) {
            errs.push(e);
        };
        if let Err(e) = Self::build_url(&self.server, &self.topic, 0) {
            errs.push(e);
        };
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(self)
    }
    pub fn builder(topic: String) -> SubscriptionBuilder {
        SubscriptionBuilder::new(topic)
    }
}

#[derive(Clone)]
pub struct SubscriptionBuilder {
    server: String,
    topic: String,
    muted: bool,
    archived: bool,
    reserved: bool,
    symbolic_icon: Option<String>,
    display_name: String,
}

impl SubscriptionBuilder {
    pub fn new(topic: String) -> Self {
        Self {
            server: DEFAULT_SERVER.to_string(),
            topic,
            muted: false,
            archived: false,
            reserved: false,
            symbolic_icon: None,
            display_name: String::new(),
        }
    }

    pub fn server(mut self, server: String) -> Self {
        self.server = server;
        self
    }

    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    pub fn archived(mut self, archived: bool) -> Self {
        self.archived = archived;
        self
    }

    pub fn reserved(mut self, reserved: bool) -> Self {
        self.reserved = reserved;
        self
    }

    pub fn symbolic_icon(mut self, symbolic_icon: Option<String>) -> Self {
        self.symbolic_icon = symbolic_icon;
        self
    }

    pub fn display_name(mut self, display_name: String) -> Self {
        self.display_name = display_name;
        self
    }

    pub fn build(self) -> Result<Subscription, Vec<Error>> {
        let res = Subscription {
            server: self.server,
            topic: self.topic,
            muted: self.muted,
            archived: self.archived,
            reserved: self.reserved,
            symbolic_icon: self.symbolic_icon,
            display_name: self.display_name,
            read_until: 0,
        };
        res.validate()
    }
}

fn default_method() -> String {
    "POST".to_string()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum Action {
    #[serde(rename = "view")]
    View {
        label: String,
        url: String,
        #[serde(default)]
        clear: bool,
    },
    #[serde(rename = "http")]
    Http {
        label: String,
        url: String,
        #[serde(default = "default_method")]
        method: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        clear: bool,
    },
    #[serde(rename = "broadcast")]
    Broadcast {
        label: String,
        intent: Option<String>,
        #[serde(default)]
        extras: HashMap<String, String>,
        #[serde(default)]
        clear: bool,
    },
}

#[derive(Debug, PartialEq, Copy, Clone, Default)]
pub enum Status {
    #[default]
    Down,
    Degraded,
    Up,
}

impl From<u8> for Status {
    fn from(item: u8) -> Self {
        match item {
            0 => Status::Down,
            1 => Status::Degraded,
            2 => Status::Up,
            _ => Status::Down,
        }
    }
}

impl From<Status> for u8 {
    fn from(item: Status) -> Self {
        match item {
            Status::Down => 0,
            Status::Degraded => 1,
            Status::Up => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub server: String,
    pub username: String
}

pub struct Notification {
    pub title: String,
    pub body: String,
    pub actions: Vec<Action>,
}

pub trait NotificationProxy: Sync + Send {
    fn send(&self, n: Notification) -> anyhow::Result<()>;
}

pub trait NetworkMonitorProxy: Sync + Send {
    fn listen(&self) -> Pin<Box<dyn Stream<Item = ()>>>;
}
