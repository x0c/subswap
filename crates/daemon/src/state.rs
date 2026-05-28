//! Daemon 内存态:冷却 / flap 检测 / Degraded 窗口。
//!
//! 一切重启即重置。理由(对齐 AUTO_SWAP_DESIGN.md §3):重启意味着上一轮诊断信息不可
//! 信赖,继续盲守历史计数风险大于收益。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use subswap_core::AccountId;

/// 5min 内最多允许的自动 swap 次数;超过进入 Degraded。
const MAX_FLAP_PER_5MIN: usize = 3;
/// Degraded 窗口长度:30min(对齐 docs/design/AUTO_SWAP_DESIGN.md §3)。
const DEGRADED_WINDOW: Duration = Duration::from_secs(30 * 60);
/// Flap 检测的滑动窗口长度。
const FLAP_WINDOW: Duration = Duration::from_secs(5 * 60);
/// 单账号冷却期:刚切走 / 切到的账号短期内不再被选中。
const COOLDOWN: Duration = Duration::from_secs(5 * 60);

pub struct DaemonState {
    /// 每个 (provider, account) 上次被切的时间。
    last_swap_at: HashMap<(String, AccountId), Instant>,
    /// 每个 provider 最近 5min 内的 swap 时间戳(滑动窗口)。
    swap_history: HashMap<String, Vec<Instant>>,
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

    /// 候选账号是否在冷却期。
    pub fn in_cooldown(&self, provider: &str, id: &AccountId) -> bool {
        match self.last_swap_at.get(&(provider.to_string(), id.clone())) {
            Some(t) => t.elapsed() < COOLDOWN,
            None => false,
        }
    }

    /// 记录一次成功 swap:更新冷却 + 滑动窗口。
    pub fn record_swap(&mut self, provider: &str, id: &AccountId) {
        let now = Instant::now();
        self.last_swap_at
            .insert((provider.to_string(), id.clone()), now);
        let history = self.swap_history.entry(provider.to_string()).or_default();
        history.push(now);
        // 清旧:只保留 FLAP_WINDOW 内的。
        history.retain(|t| now.duration_since(*t) <= FLAP_WINDOW);
    }

    /// 该 provider 最近 5min 内是否已经超过 flap 阈值。
    pub fn detect_flap(&self, provider: &str) -> bool {
        self.swap_history
            .get(provider)
            .map(|h| h.len() >= MAX_FLAP_PER_5MIN)
            .unwrap_or(false)
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
    fn mark_degraded_blocks_further_decisions_until_window_ends() {
        let mut s = DaemonState::new();
        s.mark_degraded("claude");
        assert!(s.is_degraded("claude"));
    }
}
