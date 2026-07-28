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

/// 连续查询失败的记录，用于失败退避。只存错误摘要，不含任何凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    /// 最近一次失败时间。
    pub failed_at: DateTime<Utc>,
    /// 连续失败次数（成功一次即清零）。
    pub consecutive: u32,
    /// 最近一次失败的错误文本，退避期间原样复用给展示层。
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuotaCache {
    entries: HashMap<String, CachedEntry>,
    /// 失败退避表。老版本缓存文件没有该字段，`default` 保证可平滑升级。
    #[serde(default)]
    failures: HashMap<String, FailureEntry>,
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

    /// 更新或插入缓存条目；同时清掉该账号的失败退避记录。
    pub fn set(&mut self, provider: &str, account_id: &str, quotas: Vec<Quota>) {
        let key = cache_key(provider, account_id);
        self.failures.remove(&key);
        self.entries.insert(
            key,
            CachedEntry {
                quotas,
                cached_at: Utc::now(),
            },
        );
    }

    /// 记一次查询失败，累加连续失败次数。
    ///
    /// 失败结果不进 `entries`（不能拿失败当数据用），单独记在这里只为算退避窗口。
    pub fn record_failure(&mut self, provider: &str, account_id: &str, error: &str) {
        let key = cache_key(provider, account_id);
        let consecutive = self
            .failures
            .get(&key)
            .map(|f| f.consecutive.saturating_add(1))
            .unwrap_or(1);
        self.failures.insert(
            key,
            FailureEntry {
                failed_at: Utc::now(),
                consecutive,
                error: error.to_string(),
            },
        );
    }

    /// 该账号是否仍处于失败退避窗口内；是则返回最近一次失败记录，调用方应跳过真实查询。
    ///
    /// 退避时长 = `base * 2^(consecutive - 1)`，封顶 `cap`。
    pub fn in_failure_backoff(
        &self,
        provider: &str,
        account_id: &str,
        base: std::time::Duration,
        cap: std::time::Duration,
    ) -> Option<&FailureEntry> {
        let entry = self.failures.get(&cache_key(provider, account_id))?;
        // 401/403 往往会在原生客户端刚刷新凭据后立刻恢复。仍保留一个基础窗口避免请求风暴，
        // 但不把旧鉴权失败指数退避到 15 分钟，否则用户明明已经恢复登录仍会长期看到旧错误。
        let effective_cap = if is_authentication_failure(&entry.error) {
            base
        } else {
            cap
        };
        let backoff = failure_backoff(base, effective_cap, entry.consecutive);
        let age = Utc::now() - entry.failed_at;
        // age 为负(时钟回拨)时视为已过窗口，保守地允许重查。
        if age < Duration::zero() || age >= Duration::from_std(backoff).ok()? {
            return None;
        }
        Some(entry)
    }
}

/// 连续失败 n 次后的退避时长：`base * 2^(n-1)`，封顶 `cap`。
fn failure_backoff(
    base: std::time::Duration,
    cap: std::time::Duration,
    consecutive: u32,
) -> std::time::Duration {
    let shift = consecutive.saturating_sub(1).min(16);
    base.saturating_mul(1_u32 << shift).min(cap.max(base))
}

fn cache_key(provider: &str, account_id: &str) -> String {
    format!("{provider}::{account_id}")
}

fn is_authentication_failure(error: &str) -> bool {
    error.contains("401 Unauthorized") || error.contains("403 Forbidden")
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
        QuotaWindow::Month | QuotaWindow::FirstPartyModels | QuotaWindow::Api => Duration::days(30),
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
    fn failure_backoff_grows_and_clears_on_success() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let mut cache = QuotaCache::default();

        // 首次失败 → 退避 base，窗口内跳过查询。
        cache.record_failure("claude", "a@x.com", "429 rate limited");
        let hit = cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .unwrap();
        assert_eq!(hit.consecutive, 1);
        assert_eq!(hit.error, "429 rate limited");

        // 第 3 次失败 → 退避 4*base；把 failed_at 拨到 3*base 之前仍在窗口内。
        cache.record_failure("claude", "a@x.com", "429 rate limited");
        cache.record_failure("claude", "a@x.com", "429 rate limited");
        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(270);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_some());

        // 超过封顶时长 → 放行重查。
        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(1000);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_none());

        // 查成功一次即清零，不再退避。
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_none());
    }

    #[test]
    fn authentication_failure_backoff_is_capped_at_base_interval() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let mut cache = QuotaCache::default();

        for _ in 0..5 {
            cache.record_failure(
                "claude",
                "a@x.com",
                "quota fetch: usage returned 401 Unauthorized",
            );
        }
        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(89);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_some());

        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(91);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_none());
    }

    #[test]
    fn failure_backoff_is_capped() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        assert_eq!(failure_backoff(base, cap, 1), base);
        assert_eq!(failure_backoff(base, cap, 3), base * 4);
        // 2^(20-1) * 90s 远超封顶,必须被 cap 住。
        assert_eq!(failure_backoff(base, cap, 20), cap);
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
