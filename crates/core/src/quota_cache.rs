//! quota 查询结果的磁盘缓存。
//!
//! 存储在 `cache_dir/quota_cache.json`，按单个 quota 窗口过滤：
//! - 有 `reset_at` 的窗口：reset_at 已过则该窗口数据失效，不返回。
//! - 无 `reset_at` 的窗口：按窗口类型兜底 TTL（5h/7d/30d），超出则失效。
//! - 所有窗口都失效 → 整条 entry 不返回（等同缓存未命中）。
//!
//! 缓存是可丢弃数据，读写失败静默忽略。

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Quota, QuotaWindow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub quotas: Vec<Quota>,
    pub cached_at: DateTime<Utc>,
}

/// `get()` 返回的有效缓存快照；quotas 已过滤掉过期窗口。
pub struct ValidEntry {
    pub quotas: Vec<Quota>,
    pub cached_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuotaCache {
    entries: HashMap<String, CachedEntry>,
}

impl QuotaCache {
    /// 从文件加载缓存；文件不存在或解析失败返回空缓存。
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 将缓存写入文件；失败静默忽略（缓存是可丢弃数据）。
    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// 获取仍有效的缓存快照，已过期的窗口被过滤。
    /// 若所有窗口都过期或无缓存，返回 None。
    pub fn get(&self, provider: &str, account_id: &str) -> Option<ValidEntry> {
        let entry = self.entries.get(&cache_key(provider, account_id))?;
        let now = Utc::now();
        let valid_quotas: Vec<Quota> = entry
            .quotas
            .iter()
            .filter(|q| !quota_expired(q, entry.cached_at, now))
            .cloned()
            .collect();
        if valid_quotas.is_empty() {
            return None;
        }
        Some(ValidEntry {
            quotas: valid_quotas,
            cached_at: entry.cached_at,
        })
    }

    /// 若缓存足够新(`cached_at` 距今 < `max_age`)且仍有有效窗口，返回快照；否则 None。
    /// 用于「缓存节流」：够新就直接复用、跳过真实 quota 查询，避免高频打 usage 端点。
    pub fn fresh(
        &self,
        provider: &str,
        account_id: &str,
        max_age: std::time::Duration,
    ) -> Option<ValidEntry> {
        let entry = self.entries.get(&cache_key(provider, account_id))?;
        let age = Utc::now() - entry.cached_at;
        // age 为负(时钟回拨/未来时间戳)时视为「不新鲜」,保守地重新查询。
        if age < Duration::zero() || age >= Duration::from_std(max_age).ok()? {
            return None;
        }
        self.get(provider, account_id)
    }

    /// 更新或插入缓存条目。
    pub fn set(&mut self, provider: &str, account_id: &str, quotas: Vec<Quota>) {
        self.entries.insert(
            cache_key(provider, account_id),
            CachedEntry {
                quotas,
                cached_at: Utc::now(),
            },
        );
    }
}

fn cache_key(provider: &str, account_id: &str) -> String {
    format!("{provider}::{account_id}")
}

/// 判断单个 quota 窗口是否已失效。
fn quota_expired(q: &Quota, cached_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    if let Some(reset_at) = q.reset_at {
        // 窗口已重置 → 数据失效
        return reset_at <= now;
    }
    // 没有 reset_at：按窗口类型兜底 TTL
    cached_at + window_ttl(q.window) <= now
}

fn window_ttl(window: QuotaWindow) -> Duration {
    match window {
        QuotaWindow::FiveHour | QuotaWindow::Custom => Duration::hours(5),
        QuotaWindow::SevenDay => Duration::days(7),
        QuotaWindow::Month => Duration::days(30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountId, QuotaStatus};

    fn sample_quota() -> Quota {
        Quota {
            provider: "claude".into(),
            account_id: AccountId("a@x.com".into()),
            window: QuotaWindow::SevenDay,
            used: 10,
            limit: 100,
            reset_at: Some(Utc::now() + Duration::days(3)),
            status: QuotaStatus::Ok,
            note: None,
        }
    }

    #[test]
    fn fresh_returns_only_within_window() {
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        // 刚写入 → 90s 窗口内算新鲜。
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(90))
            .is_some());
        // 0 窗口 → 任何缓存都视为不新鲜,强制重新查询。
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(0))
            .is_none());
        // 未知账号 → None。
        assert!(cache
            .fresh("claude", "b@x.com", std::time::Duration::from_secs(90))
            .is_none());
    }

    #[test]
    fn fresh_rejects_stale_entry() {
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        // 手动把 cached_at 拨到 5 分钟前 → 超过 90s 窗口,不新鲜。
        if let Some(entry) = cache.entries.get_mut("claude::a@x.com") {
            entry.cached_at = Utc::now() - Duration::minutes(5);
        }
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(90))
            .is_none());
    }
}
