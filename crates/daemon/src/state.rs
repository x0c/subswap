//! Daemon 内存态:冷却 / flap 检测 / Degraded 窗口。
//!
//! 一切重启即重置。理由(对齐 AUTO_SWAP_DESIGN.md §3):重启意味着上一轮诊断信息不可
//! 信赖,继续盲守历史计数风险大于收益。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use subswap_core::{settings, AccountId};

/// 5min 内最多允许的自动 swap 次数;超过进入 Degraded。
const MAX_FLAP_PER_5MIN: usize = 3;
/// Degraded 窗口长度:30min(对齐 docs/design/AUTO_SWAP_DESIGN.md §3)。
const DEGRADED_WINDOW: Duration = Duration::from_secs(30 * 60);
/// 快速 flap 检测的滑动窗口长度。
const FLAP_WINDOW: Duration = Duration::from_secs(5 * 60);
/// 振荡(A→B→A 回切)检测窗口。**必须明显大于 cooldown(5min)**:否则像
/// caoozc↔achesjeremy 这种被 cooldown 卡到刚好 5min 一跳的振荡,会卡在 FLAP_WINDOW 边界外、
/// 永远数不到 3 次而逃过刹车(实测 bug)。取 15min,确保同一目标在两个振荡周期内重复出现即可识别。
const OSCILLATION_WINDOW: Duration = Duration::from_secs(15 * 60);

fn cooldown() -> Duration {
    let ms = settings::current().auto_swap.cooldown_ms.max(0) as u64;
    Duration::from_millis(ms)
}

pub struct DaemonState {
    /// 每个 (provider, account) 上次被切的时间。
    last_swap_at: HashMap<(String, AccountId), Instant>,
    /// 每个 provider 最近 [`OSCILLATION_WINDOW`] 内的 swap 历史(目标账号 + 时间)。
    /// 记目标账号以识别 A→B→A 回切式振荡,不只是计数。
    swap_history: HashMap<String, Vec<(AccountId, Instant)>>,
    /// 每个 provider 进入 Degraded 的时间;None 表示未 degraded。
    degraded_until: HashMap<String, Instant>,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            last_swap_at: HashMap::new(),
            swap_history: HashMap::new(),
            degraded_until: HashMap::new(),
        }
    }

    /// 候选账号是否在冷却期。窗口长度从 [`settings`] 读取（`auto_swap.cooldown_ms`）。
    pub fn in_cooldown(&self, provider: &str, id: &AccountId) -> bool {
        match self.last_swap_at.get(&(provider.to_string(), id.clone())) {
            Some(t) => t.elapsed() < cooldown(),
            None => false,
        }
    }

    /// 记录一次成功 swap:更新冷却 + 滑动窗口。
    pub fn record_swap(&mut self, provider: &str, id: &AccountId) {
        let now = Instant::now();
        self.last_swap_at
            .insert((provider.to_string(), id.clone()), now);
        let history = self.swap_history.entry(provider.to_string()).or_default();
        history.push((id.clone(), now));
        // 清旧:保留较大的 OSCILLATION_WINDOW 内的(振荡检测需要更长回看)。
        history.retain(|(_, t)| now.duration_since(*t) <= OSCILLATION_WINDOW);
    }

    /// 该 provider 是否在抖动。两种信号取其一:
    /// 1. **快速 flap**:[`FLAP_WINDOW`](5min)内 swap 次数 ≥ [`MAX_FLAP_PER_5MIN`]。
    /// 2. **振荡回切**:[`OSCILLATION_WINDOW`](15min)内同一目标账号被切回 ≥2 次——
    ///    专治被 cooldown 卡到刚好躲过计数的 A→B→A 来回跳。
    pub fn detect_flap(&self, provider: &str) -> bool {
        let Some(history) = self.swap_history.get(provider) else {
            return false;
        };
        let now = Instant::now();
        let rapid = history
            .iter()
            .filter(|(_, t)| now.duration_since(*t) <= FLAP_WINDOW)
            .count()
            >= MAX_FLAP_PER_5MIN;
        let oscillating = history
            .iter()
            .any(|(id, _)| history.iter().filter(|(other, _)| other == id).count() >= 2);
        rapid || oscillating
    }

    /// 标记该 provider 进入 Degraded 窗口。
    pub fn mark_degraded(&mut self, provider: &str) {
        self.degraded_until
            .insert(provider.to_string(), Instant::now() + DEGRADED_WINDOW);
        // Degraded 后清空历史,退出 Degraded 后从干净状态开始。
        self.swap_history.remove(provider);
    }

    /// 是否仍处在 Degraded 窗口内。窗口过期会自动清掉。
    pub fn is_degraded(&self, provider: &str) -> bool {
        match self.degraded_until.get(provider) {
            Some(until) => *until > Instant::now(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AccountId {
        AccountId(s.into())
    }

    #[test]
    fn fresh_state_has_no_cooldown_and_no_flap() {
        let s = DaemonState::new();
        assert!(!s.in_cooldown("claude", &id("a@x")));
        assert!(!s.detect_flap("claude"));
        assert!(!s.is_degraded("claude"));
    }

    #[test]
    fn cooldown_engages_after_swap() {
        let mut s = DaemonState::new();
        s.record_swap("claude", &id("a@x"));
        assert!(s.in_cooldown("claude", &id("a@x")));
        // 别的账号不受影响。
        assert!(!s.in_cooldown("claude", &id("b@x")));
    }

    #[test]
    fn flap_kicks_in_after_three_swaps_in_window() {
        let mut s = DaemonState::new();
        s.record_swap("claude", &id("a@x"));
        s.record_swap("claude", &id("b@x"));
        assert!(!s.detect_flap("claude"));
        s.record_swap("claude", &id("c@x"));
        assert!(s.detect_flap("claude"));
        // 其他 provider 隔离。
        assert!(!s.detect_flap("codex"));
    }

    #[test]
    fn oscillation_detected_even_below_rapid_count() {
        // A→B→A 回切:同一目标(a)被切回 2 次,即使总数没到「5min 内 3 次」也算抖动。
        // 复现 caoozc↔achesjeremy 被 cooldown 卡到躲过计数的真实场景。
        let mut s = DaemonState::new();
        s.record_swap("codex", &id("a@x"));
        s.record_swap("codex", &id("b@x"));
        // 此刻 2 个不同目标、各 1 次 → 不算抖动。
        assert!(!s.detect_flap("codex"));
        // 切回 a → a 出现 2 次 → 振荡。
        s.record_swap("codex", &id("a@x"));
        assert!(s.detect_flap("codex"));
    }

    #[test]
    fn two_distinct_swaps_are_not_a_flap() {
        let mut s = DaemonState::new();
        s.record_swap("codex", &id("a@x"));
        s.record_swap("codex", &id("b@x"));
        assert!(!s.detect_flap("codex"));
    }

    #[test]
    fn mark_degraded_blocks_further_decisions_until_window_ends() {
        let mut s = DaemonState::new();
        s.mark_degraded("claude");
        assert!(s.is_degraded("claude"));
    }
}
