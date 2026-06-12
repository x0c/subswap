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
    let max_retries = cfg.fetch_retries.min(5);
    let max_attempts = max_retries.saturating_add(1);
    let timeout = Duration::from_millis(cfg.fetch_timeout_ms);

    for attempt in 1..=max_attempts {
        let result = match tokio::time::timeout(timeout, provider.query_quota(id)).await {
            Ok(inner) => inner,
            Err(_) => Err(Error::QuotaFetch("quota fetch timeout".into())),
        };

        match result {
            Ok(quotas) => return Ok(quotas),
            Err(e) if attempt < max_attempts && !is_non_retryable_http_error(&e) => {
                let retry_delay = retry_delay(cfg.fetch_retry_delay_ms, attempt);
                tracing::debug!(
                    provider = provider.id(),
                    account = %id,
                    attempt,
                    max_attempts,
                    retry_delay_ms = retry_delay.as_millis(),
                    err = %e,
                    "quota fetch failed; retrying"
                );
                if !retry_delay.is_zero() {
                    tokio::time::sleep(retry_delay).await;
                }
            }
            Err(e) if is_non_retryable_http_error(&e) => return Err(e),
            Err(e) => return Err(error_with_attempts(e, max_attempts)),
        }
    }

    Err(Error::QuotaFetch("quota fetch failed".into()))
}

fn retry_delay(base_ms: u64, failed_attempt: u32) -> Duration {
    let exponent = failed_attempt.saturating_sub(1).min(63);
    Duration::from_millis(base_ms.saturating_mul(1_u64 << exponent))
}

fn is_non_retryable_http_error(error: &Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("401")
        || text.contains("403")
        || text.contains("429")
        || text.contains("unauthorized")
        || text.contains("forbidden")
        || text.contains("too many requests")
        || text.contains("rate_limit")
        || text.contains("rate limited")
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

    struct AlwaysErrorProvider {
        calls: AtomicUsize,
        message: &'static str,
    }

    #[async_trait]
    impl Provider for AlwaysErrorProvider {
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::QuotaFetch(self.message.into()))
        }
    }

    #[tokio::test]
    async fn retries_are_capped_at_five() {
        let provider = AlwaysErrorProvider {
            calls: AtomicUsize::new(0),
            message: "temporary failure",
        };
        let cfg = QuotaSettings {
            fetch_retries: 99,
            fetch_retry_delay_ms: 0,
            ..QuotaSettings::default()
        };

        let err = query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg)
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(provider.calls.load(Ordering::SeqCst), 6);
        assert!(err.contains("after 6 attempts"), "{err}");
    }

    #[tokio::test]
    async fn non_retryable_http_failures_are_not_retried() {
        for message in [
            "usage returned 401 Unauthorized",
            "usage returned 403 Forbidden",
            "usage returned 429 Too Many Requests",
            "usage returned rate_limit_error",
        ] {
            let provider = AlwaysErrorProvider {
                calls: AtomicUsize::new(0),
                message,
            };
            let cfg = QuotaSettings {
                fetch_retries: 5,
                fetch_retry_delay_ms: 0,
                ..QuotaSettings::default()
            };

            let err = query_quota_with_retry_config(&provider, &AccountId("alice".into()), &cfg)
                .await
                .unwrap_err()
                .to_string();

            assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "{message}");
            assert!(!err.contains("after"), "{err}");
        }
    }

    #[test]
    fn retry_delay_uses_exponential_backoff() {
        let delays: Vec<u128> = (1..=5)
            .map(|attempt| retry_delay(500, attempt).as_millis())
            .collect();
        assert_eq!(delays, vec![500, 1_000, 2_000, 4_000, 8_000]);
    }
}
