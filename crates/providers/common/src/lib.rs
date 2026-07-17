//! 文件型 OAuth 账号切换共享引擎：Codex / Kimi 等「一个 JSON blob + 文件切换 + OAuth 刷新 + usage」
//! 形态的 provider 共用的机制。差异点由 [`FileBlobRuntime`] adapter 表达。

pub mod engine;
pub mod json;
pub mod runtime;

pub use engine::FileBlobProvider;
pub use json::{extract_access_token, extract_refresh_token, extract_token};
pub use runtime::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};
