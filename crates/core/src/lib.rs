//! subswap 核心：数据模型 + Provider 抽象 + 凭证仓库 + 路径与配置。

pub mod account_registry;
pub mod audit;
pub mod auto_policy;
pub mod checkout;
pub mod defaults;
pub mod error;
pub mod model;
pub mod paths;
pub mod provider;
pub mod quota_cache;
pub mod quota_query;
pub mod registry;
pub mod settings;
pub mod store;
pub mod swap;
pub mod time;

pub use account_registry::AccountRegistry;
pub use audit::{AuditEvent, AuditLog};
pub use auto_policy::{
    decide as auto_decide, AccountWithQuotas, PolicyConfig, PolicyDecision, ProviderSnapshot,
    QuotaFetchState,
};
pub use error::{Error, Result};
pub use model::{Account, AccountId, ClientTarget, Quota, QuotaStatus, QuotaWindow};
pub use provider::Provider;
pub use quota_cache::{CachedEntry, QuotaCache, ValidEntry};
pub use quota_query::query_quota_with_retry;
pub use registry::ProviderRegistry;
pub use store::{CredentialStore, FileStore, KeyringStore};
