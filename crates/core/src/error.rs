//! 统一错误类型。Provider 实现可以用 `Error::provider` / `Error::Other(anyhow)` 包装内部错误。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unknown provider: {0}")]
    ProviderNotFound(String),

    #[error("account not found: provider={provider} id={id}")]
    AccountNotFound { provider: String, id: String },

    #[error("credential store: {0}")]
    Credential(String),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("toml deserialize: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("quota fetch: {0}")]
    QuotaFetch(String),

    #[error("provider: {0}")]
    Provider(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
