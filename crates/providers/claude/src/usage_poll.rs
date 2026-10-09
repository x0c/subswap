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
    min_interval_ms: u64,
}

fn after(now: DateTime<Utc>, ms: u64) -> DateTime<Utc> {
    now.checked_add_signed(Duration::milliseconds(ms.min(i64::MAX as u64) as i64))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

pub fn begin(root: &Path, id: &str, cfg: &Settings) -> Result<Begin> {
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
    if record.next_attempt_at.is_some_and(|d| d > now) {
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
    let lease = Lease {
        _lock: lock,
        path,
        record,
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
            }
            Err(error) => {
                self.record.failures = self.record.failures.saturating_add(1);
                let generic = self
                    .min_interval_ms
                    .saturating_mul(1_u64 << self.record.failures.saturating_sub(1).min(16))
                    .min(cfg.quota.failure_backoff_max_ms.max(self.min_interval_ms));
                let text = error.to_string();
                let uncertain_native = text.contains("native client returned no limits");
                let (delay, server_deadline) = match error {
                    Error::QuotaRateLimited { retry_at, .. } => (
                        generic.max(cfg.claude.usage_rate_limit_backoff_ms),
                        *retry_at,
                    ),
                    _ if uncertain_native => {
                        (generic.max(cfg.claude.usage_rate_limit_backoff_ms), None)
                    }
                    _ => (generic, None),
                };
                let deadline = after(now, delay).max(server_deadline.unwrap_or(now));
                self.record.next_attempt_at = Some(deadline);
                self.record.error = Some(format!("{text}; retry after {}", deadline.to_rfc3339()));
                self.record.quotas.clear();
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
        let Begin::Fetch(lease) = begin(temp.path(), "account", &cfg).unwrap() else {
            panic!()
        };
        assert!(begin(temp.path(), "account", &cfg).is_err());
        drop(lease);
        assert!(begin(temp.path(), "account", &cfg)
            .unwrap_err()
            .to_string()
            .contains("pending"));
    }

    #[test]
    fn server_deadline_outlives_generic_cap_and_token_changes() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Settings::default();
        let Begin::Fetch(lease) = begin(temp.path(), "account", &cfg).unwrap() else {
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
        let error = begin(temp.path(), "account", &cfg).unwrap_err().to_string();
        assert!(error.contains(&deadline.to_rfc3339()));
    }
}
