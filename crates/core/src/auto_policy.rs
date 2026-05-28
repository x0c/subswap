//! 自动切换策略：给定一个 Provider 的账号 + 额度快照，决定要不要切、切到谁。
//!
//! 设计要点：
//! - **纯函数**：不读环境、不发网络、不写文件。所有 IO 在调用方完成；这里只决策。
//! - 不跨 Provider 决策（自动切换默认不跨 Provider；用户可能并非两边都付费）。
//! - 决策结果显式区分 [`PolicyDecision::NoOp`] / [`PolicyDecision::Swap`] / [`PolicyDecision::Degraded`]，
//!   `Degraded` 是显式终态：调用方必须提示用户手动 `subswap swap`，不能盲切。
//!
//! 规则细节见 docs/design/AUTO_SWAP_DESIGN.md。

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
    /// 拉取状态。决策时只看 [`QuotaFetchState::Failed`]；
    /// [`QuotaFetchState::Loading`] 是 CLI 首屏占位，不应进入 [`decide`]。
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
}

impl QuotaFetchState {
    /// 拉取失败时返回错误文本；其他状态返回 `None`。
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

    // 1. active 账号自身额度查询失败 → 不知道是否真超额 → 降级。
    if let Some(a) = active {
        if let Some(err) = a.fetch_state.failed() {
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

    // 3. 筛候选：排除当前激活，排除耗尽/已超阈值的（切过去就是抖动），按 allow_unknown 决定是否包含 fetch_error/Unknown。
    let candidates: Vec<&AccountWithQuotas> = snapshot
        .accounts
        .iter()
        .filter(|a| Some(&a.account.id) != active_id.as_ref())
        .filter(|a| is_viable_candidate(a, config.threshold, config.allow_unknown))
        .collect();

    if candidates.is_empty() {
        return PolicyDecision::Degraded {
            reason: "no swap candidate (others exhausted / fetch failed / unknown status)".into(),
        };
    }

    // 4. 选最优：「最忙窗口」剩余最多 → priority 小 → id 字典序。
    let best = candidates
        .into_iter()
        .min_by(|a, b| compare_candidates(a, b))
        .expect("non-empty");

    let reason = match active {
        Some(a) => format!(
            "{} above {:.0}% threshold; pick {} (most headroom)",
            a.account.id,
            config.threshold * 100.0,
            best.account.id
        ),
        None => format!("no active account; activate {}", best.account.id),
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

fn is_viable_candidate(a: &AccountWithQuotas, threshold: f64, allow_unknown: bool) -> bool {
    if a.fetch_state.failed().is_some() {
        return allow_unknown;
    }
    if a.quotas.is_empty() {
        return allow_unknown;
    }
    // 关键：候选不能在任何窗口达到/超过 threshold，否则切过去也是马上又得切，纯抖动。
    // 同时所有窗口都不能是 Exhausted（防御性，理论上 used>=100 已包含在 is_above 里）。
    let no_above_threshold = a.quotas.iter().all(|q| !q.is_above(threshold));
    let no_exhausted = a
        .quotas
        .iter()
        .all(|q| !matches!(q.status, QuotaStatus::Exhausted));
    if allow_unknown {
        no_above_threshold && no_exhausted
    } else {
        // 默认模式下还要求至少有一个明确 Ok 窗口（拒收纯 Unknown 候选）。
        let any_ok = a.quotas.iter().any(|q| matches!(q.status, QuotaStatus::Ok));
        any_ok && no_above_threshold && no_exhausted
    }
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
        Quota {
            provider: "claude".into(),
            account_id: AccountId("x".into()),
            window: QuotaWindow::FiveHour,
            used,
            limit: 100,
            reset_at: None,
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
    fn degraded_when_active_quota_fetch_fails() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.fetch_state = QuotaFetchState::Failed("timeout".into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
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
}
