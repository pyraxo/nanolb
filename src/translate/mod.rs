pub(crate) mod anthropic;
pub(crate) mod chat;

use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Deserialize)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    delta: Option<Text<'a>>,
    #[serde(borrow)]
    item_id: Option<Text<'a>>,
}

impl Event<'_> {
    fn delta(&self) -> Option<&str> {
        self.delta.as_ref().map(|t| &*t.0)
    }

    fn item_id(&self) -> &str {
        self.item_id.as_ref().map_or("", |t| &t.0)
    }
}

fn write_json(b: &mut Vec<u8>, v: &impl Serialize) {
    let _ = serde_json::to_writer(b, v);
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TranslateError {
    #[error("messages is required and must be a non-empty array")]
    MissingMessages,
    #[error("model is required")]
    MissingModel,
    #[error("messages must be an array")]
    MessagesNotArray,
    #[error("invalid message role: {0:?}")]
    InvalidRole(String),
    #[error("message content must be a string or an array")]
    InvalidContent,
}
