pub mod anthropic;
pub mod chat;

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
