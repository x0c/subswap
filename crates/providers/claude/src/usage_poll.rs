//! Claude 账号维度的跨进程查询预约与持久结果，独立于整表额度缓存的并发写入。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subswap_core::{
    error::{Error, Result},
    settings::Settings,
    Quota,
};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    next_attempt_at: Option<DateTime<Utc>>,
    observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    quotas: Vec<Quota>,
    error: Option<String>,
    /// 失败时所用 access token 的指纹；换了凭证后非限流失败不再挡查询。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failed_credential: Option<String>,
    /// 429 退避按账号生效，与凭证无关。
    #[serde(default)]
    rate_limited: bool,
}

#[derive(Debug)]
pub enum Begin {
    Cached(Vec<Quota>),
    Fetch(Lease),
}

#[derive(Debug)]
pub struct Lease {
    _lock: File,
    path: PathBuf,
    record: Record,
    credential: String,
    min_interval_ms: u64,
}

/// 停用号凭据轮换与查询共用同一账号边界；不适用于 active 账号。
pub fn refresh_lock(root: &Path, id: &str) -> Result<File> {
    std::fs::create_dir_all(root)?;
    let key = format!("{:x}", Sha256::digest(id.as_bytes()));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!("{key}.refresh.lock")))?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| {
        Error::QuotaFetch("Claude parked credential refresh already in progress".into())
    })?;
    Ok(lock)
}

fn credential_fingerprint(access_token: &str) -> String {
    format!("{:x}", Sha256::digest(access_token.as_bytes()))[..16].to_string()
}

fn after(now: DateTime<Utc>, ms: u64) -> DateTime<Utc> {
    now.checked_add_signed(Duration::milliseconds(ms.min(i64::MAX as u64) as i64))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// 预约一次账号 usage 查询。`access_token` 只用于比对上次失败所用凭证，不落盘原文。
pub fn begin(root: &Path, id: &str, access_token: &str, cfg: &Settings) -> Result<Begin> {
    std::fs::create_dir_all(root)?;
    let key = format!("{:x}", Sha256::digest(id.as_bytes()));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!("{key}.lock")))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| Error::QuotaFetch("Claude quota query already in progress".into()))?;
    let path = root.join(format!("{key}.json"));
    let mut record: Record = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| Error::QuotaFetch("Claude quota coordination state unreadable".into()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::default(),
        Err(e) => return Err(e.into()),
    };
    let now = Utc::now();
    let min_interval_ms = cfg
        .claude
        .usage_min_refresh_interval_ms
        .max(cfg.quota.min_refresh_interval_ms);
    let credential = credential_fingerprint(access_token);
    // 过期 / 401 这类失败只针对当时那枚凭证：Claude Code 或 subswap 续期后立即可查。
    let failed_with_other_credential = !record.rate_limited
        && record
            .failed_credential
            .as_deref()
            .is_some_and(|failed| failed != credential);
    if record.next_attempt_at.is_some_and(|d| d > now) && !failed_with_other_credential {
        if let Some(error) = record.error {
            return Err(Error::QuotaFetch(error));
        }
        let quotas = valid_quotas(&record, now, min_interval_ms);
        if !quotas.is_empty() {
            return Ok(Begin::Cached(quotas));
        }
        return Err(Error::QuotaFetch(
            "Claude quota query cooling down; no current reading".into(),
        ));
    }
    record.next_attempt_at = Some(after(now, min_interval_ms));
    record.error = Some("Claude quota query pending; waiting for refresh interval".into());
    record.failed_credential = None;
    record.rate_limited = false;
    let lease = Lease {
        _lock: lock,
        path,
        record,
        credential,
        min_interval_ms,
    };
    lease.save()?; // 请求前落盘：超时、取消、进程退出都不能立即再打一次。
    Ok(Begin::Fetch(lease))
}

fn valid_quotas(record: &Record, now: DateTime<Utc>, max_age_ms: u64) -> Vec<Quota> {
    let Some(observed) = record.observed_at else {
        return Vec::new();
    };
    if now < observed || now >= after(observed, max_age_ms) {
        return Vec::new();
    }
    record
        .quotas
        .iter()
        .filter(|q| q.reset_at.map_or(true, |d| d > now))
        .cloned()
        .collect()
}

impl Lease {
    pub fn finish(mut self, result: &Result<Vec<Quota>>, cfg: &Settings) -> Result<()> {
        let now = Utc::now();
        match result {
            Ok(quotas) => {
                self.record.observed_at = Some(now);
                self.record.next_attempt_at = Some(after(now, self.min_interval_ms));
                self.record.quotas = quotas.clone();
                self.record.error = None;
                self.record.failures = 0;
                self.record.failed_credential = None;
                self.record.rate_limited = false;
            }
            Err(error) => {
                self.record.failures = self.record.failures.saturating_add(1);
                let generic = self
                    .min_interval_ms
                    .saturating_mul(1_u64 << self.record.failures.saturating_sub(1).min(16))
                    .min(cfg.quota.failure_backoff_max_ms.max(self.min_interval_ms));
                let text = error.to_string();
                let (delay, server_deadline) = match error {
                    Error::QuotaRateLimited { retry_at, .. } => (
                        generic.max(cfg.claude.usage_rate_limit_backoff_ms),
                        *retry_at,
                    ),
                    _ => (generic, None),
                };
                let deadline = after(now, delay).max(server_deadline.unwrap_or(now));
                self.record.next_attempt_at = Some(deadline);
                self.record.error = Some(format!("{text}; retry after {}", deadline.to_rfc3339()));
                self.record.quotas.clear();
                self.record.failed_credential = Some(self.credential.clone());
                self.record.rate_limited = matches!(error, Error::QuotaRateLimited { .. });
            }
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let temp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_vec(&self.record)?)?;
        std::fs::rename(temp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_survives_cancel_and_coordinates_separate_handles() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Settings::default();
        let Begin::Fetch(lease) = begin(temp.path(), "account", "token-a", &cfg).unwrap() else {
            panic!()
        };
        assert!(begin(temp.path(), "account", "token-a", &cfg).is_err());
        drop(lease);
        assert!(begin(temp.path(), "account", "token-a", &cfg)
            .unwrap_err()
            .to_string()
            .contains("pending"));
    }

    #[test]
    fn server_deadline_outlives_generic_cap_and_token_changes() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Settings::default();
        let Begin::Fetch(lease) = begin(temp.path(), "account", "token-a", &cfg).unwrap() else {
            panic!()
        };
        let deadline = Utc::now() + Duration::hours(2);
        lease
            .finish(
                &Err(Error::QuotaRateLimited {
                    message: "test".into(),
                    retry_at: Some(deadline),
                }),
                &cfg,
            )
            .unwrap();
        let error = begin(temp.path(), "account", "token-b", &cfg)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&deadline.to_rfc3339()));
    }

    #[test]
    fn non_rate_limited_failure_only_blocks_the_same_credential() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Settings::default();
        let Begin::Fetch(lease) = begin(temp.path(), "account", "token-a", &cfg).unwrap() else {
            panic!()
        };
        lease
            .finish(
                &Err(Error::QuotaFetch(
                    "Claude credential expired; open Claude Code to renew".into(),
                )),
                &cfg,
            )
            .unwrap();
        let error = begin(temp.path(), "account", "token-a", &cfg)
            .unwrap_err()
            .to_string();
        assert!(error.contains("expired"), "{error}");
        assert!(matches!(
            begin(temp.path(), "account", "token-b", &cfg).unwrap(),
            Begin::Fetch(_)
        ));
        // 新凭证的预约生效后，旧凭证也不得绕过这次预约连打。
        assert!(begin(temp.path(), "account", "token-a", &cfg)
            .unwrap_err()
            .to_string()
            .contains("pending"));
    }
}
