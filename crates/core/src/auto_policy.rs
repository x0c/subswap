//! 自动切换策略：给定一个 Provider 的账号 + 额度快照，决定要不要切、切到谁。
//!
//! 设计要点：
//! - **纯函数**：不读环境、不发网络、不写文件。所有 IO 在调用方完成；这里只决策。
//! - 不跨 Provider 决策（自动切换默认不跨 Provider；用户可能并非两边都付费）。
//! - 决策结果显式区分 [`PolicyDecision::NoOp`] / [`PolicyDecision::Swap`] / [`PolicyDecision::Degraded`]，
//!   `Degraded` 是显式终态：调用方必须提示用户手动 `subswap swap`，不能盲切。
//!
//! 规则细节见 docs/design/AUTO_SWAP_DESIGN.md。

use chrono::{DateTime, Utc};

use crate::model::{Account, AccountId, Quota, QuotaStatus};
use crate::settings;

#[derive(Debug, Clone, Copy)]
pub struct PolicyConfig {
    /// 触发阈值，0.0~1.0。默认值来自当前生效的配置（`config.toml > auto_swap.threshold`）。
    pub threshold: f64,
    /// 是否允许把 status=Unknown 的账号作为候选。默认 false（保守）。
    pub allow_unknown: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            threshold: settings::current().auto_swap.threshold,
            allow_unknown: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountWithQuotas {
    pub account: Account,
    pub quotas: Vec<Quota>,
    /// 拉取状态。CLI 渐进刷新时可能把 [`QuotaFetchState::Loading`] 传入决策；
    /// active 仍在 loading 且已有明确可用候选时，允许先切走。
    pub fetch_state: QuotaFetchState,
}

/// 单次 `query_quota` 的状态机。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum QuotaFetchState {
    /// CLI 首屏渲染骨架时的占位；尚未发起或尚未返回。
    Loading,
    /// 拉取完成（`quotas` 是结果，允许为空）。
    #[default]
    Ready,
    /// 拉取失败，附带错误描述。
    Failed(String),
    /// 实时查询失败，但存在未过期的缓存数据（由 `QuotaCache` 回填）。
    /// 对应账号的 `AccountWithQuotas.quotas` 存放缓存快照。
    /// 自动切换策略将其等同于 `Ready` 处理。
    Stale {
        cached_at: chrono::DateTime<chrono::Utc>,
        error: String,
    },
}

impl QuotaFetchState {
    /// 拉取失败（且无可用缓存）时返回错误文本；其他状态返回 `None`。
    pub fn failed(&self) -> Option<&str> {
        match self {
            Self::Failed(e) => Some(e.as_str()),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderSnapshot {
    pub provider: String,
    pub accounts: Vec<AccountWithQuotas>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// 当前激活账号还在阈值内，不动。
    NoOp { reason: String },
    /// 决定切换。`from` 为空表示当前没有激活账号。
    Swap {
        from: Option<AccountId>,
        to: AccountId,
        reason: String,
    },
    /// 自动切换不可用，需要人工介入（手动 swap）。
    Degraded { reason: String },
}

pub fn decide(snapshot: &ProviderSnapshot, config: &PolicyConfig) -> PolicyDecision {
    if snapshot.accounts.is_empty() {
        return PolicyDecision::Degraded {
            reason: format!("provider {} has no accounts", snapshot.provider),
        };
    }

    let active = snapshot.accounts.iter().find(|a| a.account.active);
    let active_id = active.map(|a| a.account.id.clone());

    // manual_only 账号只允许用户显式切换。激活后自动切换完全停用。
    if let Some(active) = active.filter(|active| active.account.manual_only()) {
        return PolicyDecision::NoOp {
            reason: format!("{} is manual-only", active.account.id),
        };
    }

    // 1. active 账号自身额度尚未可用时，若有额度明确可用的其他账号则切走。
    // 没有明确可用候选时才降级，避免从未知切到未知。
    if let Some(a) = active {
        if a.fetch_state.is_loading() {
            if let Some(best) = best_known_available_candidate(snapshot, &a.account.id, config) {
                return PolicyDecision::Swap {
                    from: Some(a.account.id.clone()),
                    to: best.account.id.clone(),
                    reason: format!(
                        "active account {} quota still loading; pick {} (known available)",
                        a.account.id, best.account.id
                    ),
                };
            }
        }
        if let Some(err) = a.fetch_state.failed() {
            if let Some(best) = best_known_available_candidate(snapshot, &a.account.id, config) {
                return PolicyDecision::Swap {
                    from: Some(a.account.id.clone()),
                    to: best.account.id.clone(),
                    reason: format!(
                        "active account {} quota fetch failed ({}); pick {} (known available)",
                        a.account.id, err, best.account.id
                    ),
                };
            }
            return PolicyDecision::Degraded {
                reason: format!(
                    "active account {} quota fetch failed ({}); cannot decide",
                    a.account.id, err
                ),
            };
        }
    }

    // 2. 判断当前 active 是否需要切走。
    let needs_swap = match active {
        Some(a) => account_needs_swap(a, config.threshold),
        None => true, // 没有 active 时主动选一个激活
    };

    if !needs_swap {
        let id = active.map(|a| a.account.id.to_string()).unwrap_or_default();
        return PolicyDecision::NoOp {
            reason: format!("{} within threshold", id),
        };
    }

    // 3. 筛候选：排除当前激活，优先选择当前可承接流量的账号。
    let candidates: Vec<&AccountWithQuotas> = snapshot
        .accounts
        .iter()
        .filter(|a| Some(&a.account.id) != active_id.as_ref())
        .filter(|a| !a.account.manual_only())
        .filter(|a| is_viable_candidate(a, config.threshold, config.allow_unknown))
        .collect();

    if let Some(best) = candidates
        .into_iter()
        .min_by(|a, b| compare_candidates(a, b))
    {
        let reason = match active {
            Some(a) => format!(
                "{} above {:.0}% threshold; pick {} (most headroom)",
                a.account.id,
                config.threshold * 100.0,
                best.account.id
            ),
            None => format!("no active account; activate {}", best.account.id),
        };

        return PolicyDecision::Swap {
            from: active_id,
            to: best.account.id.clone(),
            reason,
        };
    }

    // 4. 当前账号已明确耗尽时，quota 查询失败的其他账号仍可作为逃生候选。
    // 查询失败不代表账号不可用；此时继续留在已耗尽账号一定无法承接流量。
    if let Some(active) = active {
        let failed_candidates: Vec<&AccountWithQuotas> = snapshot
            .accounts
            .iter()
            .filter(|a| Some(&a.account.id) != active_id.as_ref())
            .filter(|a| !a.account.manual_only())
            .filter(|a| a.fetch_state.failed().is_some())
            .collect();

        if let Some(best) = failed_candidates
            .into_iter()
            .min_by(|a, b| compare_unknown_candidates(a, b))
        {
            return PolicyDecision::Swap {
                from: active_id,
                to: best.account.id.clone(),
                reason: format!(
                    "{} above {:.0}% threshold; pick {} (quota unavailable fallback)",
                    active.account.id,
                    config.threshold * 100.0,
                    best.account.id
                ),
            };
        }
    }

    // 5. 没有当前可用号时，允许切到「最早恢复可用」的账号。
    // 对多窗口账号取所有阻塞窗口 reset_at 的最大值，确保切过去后不会被另一个窗口继续卡住。
    let reset_candidates: Vec<(&AccountWithQuotas, DateTime<Utc>)> = snapshot
        .accounts
        .iter()
        .filter(|a| !a.account.manual_only())
        .filter_map(|a| reset_ready_at(a, config.threshold, config.allow_unknown).map(|t| (a, t)))
        .collect();

    let Some((best, ready_at)) = reset_candidates
        .into_iter()
        .min_by(|(a, a_ready), (b, b_ready)| compare_reset_candidates(a, *a_ready, b, *b_ready))
    else {
        return PolicyDecision::Degraded {
            reason: "no swap candidate (others exhausted / fetch failed / unknown status)".into(),
        };
    };

    if Some(&best.account.id) == active_id.as_ref() {
        return PolicyDecision::NoOp {
            reason: format!(
                "{} waiting for soonest reset at {}",
                best.account.id,
                ready_at.to_rfc3339()
            ),
        };
    }

    let reason = match active {
        Some(a) => format!(
            "{} above {:.0}% threshold; pick {} (soonest reset at {})",
            a.account.id,
            config.threshold * 100.0,
            best.account.id,
            ready_at.to_rfc3339()
        ),
        None => format!(
            "no active account; activate {} (soonest reset at {})",
            best.account.id,
            ready_at.to_rfc3339()
        ),
    };

    PolicyDecision::Swap {
        from: active_id,
        to: best.account.id.clone(),
        reason,
    }
}

fn account_needs_swap(a: &AccountWithQuotas, threshold: f64) -> bool {
    if a.quotas.is_empty() {
        return false; // 无窗口数据时不主动切（保守）
    }
    // 只看「已耗尽」或「达到 threshold」。Provider 自己的 Warn 标记仅作
    // 展示用途的着色不耦合到自动切换决策——否则改默认 threshold 时
    // 还得连同 Provider 内 Warn 阈值一起调。
    a.quotas
        .iter()
        .any(|q| matches!(q.status, QuotaStatus::Exhausted) || q.is_above(threshold))
}

fn best_known_available_candidate<'a>(
    snapshot: &'a ProviderSnapshot,
    active_id: &AccountId,
    config: &PolicyConfig,
) -> Option<&'a AccountWithQuotas> {
    snapshot
        .accounts
        .iter()
        .filter(|candidate| candidate.account.id != *active_id)
        .filter(|candidate| is_viable_candidate(candidate, config.threshold, config.allow_unknown))
        .min_by(|left, right| compare_candidates(left, right))
}

fn is_viable_candidate(a: &AccountWithQuotas, threshold: f64, allow_unknown: bool) -> bool {
    if a.account.manual_only() {
        return false;
    }
    if a.fetch_state.failed().is_some() {
        return allow_unknown;
    }
    if a.quotas.is_empty() {
        return allow_unknown;
    }
    // 候选不能有任何窗口达到/超过 threshold，否则切过去仍无法正常承接流量。
    let no_above_threshold = a.quotas.iter().all(|q| !q.is_above(threshold));
    let no_exhausted = a
        .quotas
        .iter()
        .all(|q| !matches!(q.status, QuotaStatus::Exhausted));
    if allow_unknown {
        no_above_threshold && no_exhausted
    } else {
        let any_ok = a.quotas.iter().any(|q| matches!(q.status, QuotaStatus::Ok));
        any_ok && no_above_threshold && no_exhausted
    }
}

fn reset_ready_at(
    a: &AccountWithQuotas,
    threshold: f64,
    allow_unknown: bool,
) -> Option<DateTime<Utc>> {
    if a.fetch_state.failed().is_some() || a.quotas.is_empty() {
        return None;
    }

    let mut ready_at: Option<DateTime<Utc>> = None;
    let mut has_blocking_window = false;
    let mut has_known_ok_after_reset = false;

    for q in &a.quotas {
        let blocking = quota_blocks_candidate(q, threshold);
        has_blocking_window |= blocking;
        has_known_ok_after_reset |= blocking || matches!(q.status, QuotaStatus::Ok);

        if blocking {
            let reset_at = q.reset_at?;
            ready_at = Some(ready_at.map_or(reset_at, |current| current.max(reset_at)));
        }
    }

    if !has_blocking_window {
        return None;
    }
    if !allow_unknown && !has_known_ok_after_reset {
        return None;
    }

    ready_at
}

fn quota_blocks_candidate(q: &Quota, threshold: f64) -> bool {
    matches!(q.status, QuotaStatus::Exhausted) || q.is_above(threshold)
}

fn compare_candidates(a: &AccountWithQuotas, b: &AccountWithQuotas) -> std::cmp::Ordering {
    // 「最忙窗口」used 升序（剩余多的优先）
    let a_busiest = busiest_used(&a.quotas);
    let b_busiest = busiest_used(&b.quotas);
    a_busiest
        .cmp(&b_busiest)
        .then(a.account.priority.cmp(&b.account.priority))
        .then(a.account.id.0.cmp(&b.account.id.0))
}

fn compare_reset_candidates(
    a: &AccountWithQuotas,
    a_ready: DateTime<Utc>,
    b: &AccountWithQuotas,
    b_ready: DateTime<Utc>,
) -> std::cmp::Ordering {
    a_ready
        .cmp(&b_ready)
        .then(a.account.priority.cmp(&b.account.priority))
        .then(a.account.id.0.cmp(&b.account.id.0))
}

fn compare_unknown_candidates(a: &AccountWithQuotas, b: &AccountWithQuotas) -> std::cmp::Ordering {
    a.account
        .priority
        .cmp(&b.account.priority)
        .then(a.account.id.0.cmp(&b.account.id.0))
}

fn busiest_used(quotas: &[Quota]) -> u64 {
    quotas.iter().map(|q| q.used).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountId, Quota, QuotaStatus, QuotaWindow};

    fn mk_account(id: &str, active: bool) -> Account {
        Account {
            provider: "claude".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    fn mk_quota(used: u64, status: QuotaStatus) -> Quota {
        mk_quota_with_reset(used, status, None)
    }

    fn mk_quota_with_reset(
        used: u64,
        status: QuotaStatus,
        reset_at: Option<chrono::DateTime<Utc>>,
    ) -> Quota {
        Quota {
            provider: "claude".into(),
            account_id: AccountId("x".into()),
            window: QuotaWindow::FiveHour,
            used,
            limit: 100,
            reset_at,
            status,
            note: None,
        }
    }

    fn mk_awq(id: &str, active: bool, used: u64, status: QuotaStatus) -> AccountWithQuotas {
        AccountWithQuotas {
            account: mk_account(id, active),
            quotas: vec![mk_quota(used, status)],
            fetch_state: QuotaFetchState::Ready,
        }
    }

    #[test]
    fn noop_when_active_below_threshold() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![
                mk_awq("a", true, 50, QuotaStatus::Ok),
                mk_awq("b", false, 0, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::NoOp { .. }));
    }

    #[test]
    fn swap_when_active_above_threshold() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![
                mk_awq("a", true, 99, QuotaStatus::Warn),
                mk_awq("b", false, 10, QuotaStatus::Ok),
                mk_awq("c", false, 30, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &PolicyConfig::default());
        match d {
            PolicyDecision::Swap { from, to, .. } => {
                assert_eq!(from.unwrap().0, "a");
                // b 剩余最多
                assert_eq!(to.0, "b");
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn degraded_when_all_candidates_exhausted() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![
                mk_awq("a", true, 100, QuotaStatus::Exhausted),
                mk_awq("b", false, 100, QuotaStatus::Exhausted),
            ],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn active_quota_fetch_failure_swaps_to_known_available_candidate() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.fetch_state = QuotaFetchState::Failed("timeout".into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::Swap { to, .. } if to.0 == "b"));
    }

    #[test]
    fn active_quota_loading_swaps_to_known_available_candidate() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.quotas.clear();
        a.fetch_state = QuotaFetchState::Loading;
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::Swap { to, .. } if to.0 == "b"));
    }

    #[test]
    fn active_manual_only_account_disables_auto_swap_while_loading() {
        let mut api = mk_awq("api", true, 0, QuotaStatus::Unknown);
        api.account.extra.insert("manual_only".into(), true.into());
        api.quotas.clear();
        api.fetch_state = QuotaFetchState::Loading;
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![api, mk_awq("oauth", false, 0, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::NoOp { .. }));
    }

    #[test]
    fn manual_only_account_is_never_an_auto_swap_candidate() {
        let mut api = mk_awq("api", false, 0, QuotaStatus::Ok);
        api.account.extra.insert("manual_only".into(), true.into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![mk_awq("oauth", true, 100, QuotaStatus::Exhausted), api],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn degraded_when_active_quota_fetch_fails_without_known_candidate() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.fetch_state = QuotaFetchState::Failed("timeout".into());
        let mut b = mk_awq("b", false, 0, QuotaStatus::Unknown);
        b.fetch_state = QuotaFetchState::Failed("429".into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![a, b],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn activates_when_no_active_account() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![
                mk_awq("a", false, 20, QuotaStatus::Ok),
                mk_awq("b", false, 5, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &PolicyConfig::default());
        match d {
            PolicyDecision::Swap { from, to, .. } => {
                assert!(from.is_none());
                assert_eq!(to.0, "b"); // 用得少的优先
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn candidate_also_above_threshold_yields_degraded_not_churn() {
        // 用户实际场景：两个号都接近耗尽时不应该硬切。
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            accounts: vec![
                mk_awq("a", true, 100, QuotaStatus::Exhausted),
                mk_awq("b", false, 99, QuotaStatus::Warn),
            ],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::Degraded { .. }), "got {d:?}");
    }

    #[test]
    fn exhausted_active_swaps_to_failed_quota_candidate() {
        let mut candidate = mk_awq("candidate", false, 0, QuotaStatus::Unknown);
        candidate.quotas.clear();
        candidate.fetch_state = QuotaFetchState::Failed("429 rate limited".into());

        let d = decide(
            &ProviderSnapshot {
                provider: "claude".into(),
                accounts: vec![
                    mk_awq("active", true, 100, QuotaStatus::Exhausted),
                    candidate,
                ],
            },
            &PolicyConfig::default(),
        );
        assert!(matches!(d, PolicyDecision::Swap { to, .. } if to.0 == "candidate"));
    }

    #[test]
    fn priority_breaks_tie_when_usage_equal() {
        let mut b = mk_awq("b", false, 10, QuotaStatus::Ok);
        let mut c = mk_awq("c", false, 10, QuotaStatus::Ok);
        b.account.priority = 50;
        c.account.priority = 10;
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![mk_awq("a", true, 99, QuotaStatus::Warn), b, c],
        };
        let d = decide(&snap, &PolicyConfig::default());
        match d {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "c"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn picks_soonest_reset_when_no_candidate_has_headroom() {
        let now = Utc::now();
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![
            mk_quota_with_reset(
                100,
                QuotaStatus::Exhausted,
                Some(now + chrono::Duration::hours(5)),
            ),
            mk_quota_with_reset(80, QuotaStatus::Ok, Some(now + chrono::Duration::hours(46))),
        ];

        let mut sooner = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        sooner.quotas = vec![
            mk_quota_with_reset(
                100,
                QuotaStatus::Exhausted,
                Some(now + chrono::Duration::minutes(3)),
            ),
            mk_quota_with_reset(72, QuotaStatus::Ok, Some(now + chrono::Duration::days(4))),
        ];

        let mut later = mk_awq("c", false, 100, QuotaStatus::Exhausted);
        later.quotas = vec![mk_quota_with_reset(
            100,
            QuotaStatus::Exhausted,
            Some(now + chrono::Duration::hours(1)),
        )];

        let snap = ProviderSnapshot {
            provider: "codex".into(),
            accounts: vec![active, later, sooner],
        };
        let d = decide(&snap, &PolicyConfig::default());
        match d {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "b"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn waits_when_active_account_has_soonest_reset() {
        let now = Utc::now();
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![mk_quota_with_reset(
            100,
            QuotaStatus::Exhausted,
            Some(now + chrono::Duration::minutes(3)),
        )];

        let mut later = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        later.quotas = vec![mk_quota_with_reset(
            100,
            QuotaStatus::Exhausted,
            Some(now + chrono::Duration::hours(1)),
        )];

        let snap = ProviderSnapshot {
            provider: "codex".into(),
            accounts: vec![active, later],
        };
        let d = decide(&snap, &PolicyConfig::default());
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }
}
