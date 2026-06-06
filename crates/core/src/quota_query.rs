//! quota 查询的通用重试包装。
//!
//! Provider 只负责「怎么查」；CLI / daemon 共享这里的超时与重试策略，避免各处各写一套。

use std::time::Duration;

use crate::error::{Error, Result};
use crate::model::{AccountId, Quota};
use crate::provider::Provider;
use crate::settings::{self, Quota as QuotaSettings};

/// 按当前配置查询 quota，失败或单次超时后做保守重试。
pub async fn query_quota_with_retry(provider: &dyn Provider, id: &AccountId) -> Result<Vec<Quota>> {
    let cfg = settings::current().quota.clone();
    query_quota_with_retry_config(provider, id, &cfg).await
}

async fn query_quota_with_retry_config(
    provider: &dyn Provider,
    id: &AccountId,
    cfg: &QuotaSettings,
) -> Result<Vec<Quota>> {
    let max_attempts = cfg.fetch_retries.saturating_add(1);
    let timeout = Duration::from_millis(cfg.fetch_timeout_ms);
    let retry_delay = Duration::from_millis(cfg.fetch_retry_delay_ms);

    for attempt in 1..=max_attempts {
        let result = match tokio::time::timeout(timeout, provider.query_quota(id)).await {
            Ok(inner) => inner,
            Err(_) => Err(Error::QuotaFetch("quota fetch timeout".into())),
        };

        match result {
            Ok(quotas) => return Ok(quotas),
            Err(e) if attempt < max_attempts => {
                tracing::debug!(
                    provider = provider.id(),
                    account = %id,
                    attempt,
                    max_attempts,
                    err = %e,
                    "quota fetch failed; retrying"
                );
                if !retry_delay.is_zero() {
                    tokio::time::sleep(retry_delay).await;
                }
            }
            Err(e) => return Err(error_with_attempts(e, max_attempts)),
        }
    }

    Err(Error::QuotaFetch("quota fetch failed".into()))
}

fn error_with_attempts(error: Error, max_attempts: u32) -> Error {
    if max_attempts <= 1 {
        return error;
    }
    match error {
        Error::QuotaFetch(message) => {
            Error::QuotaFetch(format!("{message} after {max_attempts} attempts"))
        }
        other => Error::QuotaFetch(format!("{other} after {max_attempts} attempts")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::model::{Account, ClientTarget};

    struct FlakyProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        fn id(&self) -> &'static str {
            "test"
        }

        fn display_name(&self) -> &'static str {
            "Test"
        }

        fn client_targets(&self) -> Vec<ClientTarget> {
            Vec::new()
        }

        async fn list_accounts(&self) -> Result<Vec<Account>> {
            Ok(Vec::new())
        }

        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Err(Error::QuotaFetch("temporary failure".into()))
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn retries_once_after_failure() {
        let provider = FlakyProvider {
            calls: AtomicUsize::new(0),
        };
        let cfg = QuotaSettings {
            fetch_retries: 1,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let result =
            query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg).await;

        assert!(result.is_ok());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    struct AlwaysTimeoutProvider;

    #[async_trait]
    impl Provider for AlwaysTimeoutProvider {
        fn id(&self) -> &'static str {
            "test"
        }

        fn display_name(&self) -> &'static str {
            "Test"
        }

        fn client_targets(&self) -> Vec<ClientTarget> {
            Vec::new()
        }

        async fn list_accounts(&self) -> Result<Vec<Account>> {
            Ok(Vec::new())
        }

        async fn activate(&self, _id: &AccountId) -> Result<()> {
            Ok(())
        }

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn final_timeout_reports_attempt_count() {
        let cfg = QuotaSettings {
            fetch_timeout_ms: 1,
            fetch_retries: 1,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let err =
            query_quota_with_retry_config(&AlwaysTimeoutProvider, &AccountId("alice".into()), &cfg)
                .await
                .unwrap_err()
                .to_string();

        assert!(err.contains("after 2 attempts"), "{err}");
    }
}
