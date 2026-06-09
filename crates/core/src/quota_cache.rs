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
