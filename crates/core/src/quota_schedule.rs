//! 自适应后台额度采集调度（纯函数、无 IO）。
//!
//! 设计要点：
//! - 调度只决定「什么时候值得发起一次采集」。真实请求下限、跨进程去重与合并、
//!   Provider 专属退避（Claude 三分钟地板 / Retry-After 等）由
//!   [`crate::quota_query::collect_quota`] 的协调器负责；调度发出的采集请求永远
//!   不能绕过协调器。
//! - 所有函数接收显式 `now` 与配置，便于单元测试，不读环境、不发网络、不写文件。
//! - 超过 `decision_max_age` 的数据只能展示为 [`QuotaFetchState::Stale`]；不能因为
//!   「本轮调度没查」就把旧数字升格成 `Ready`，更不能把计时器到期当成 100% 余额。
//!
//! 语义细节见 docs/CONFIG.md 与 docs/design/AUTO_SWAP_DESIGN.md。

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;

use crate::auto_policy::QuotaFetchState;
use crate::model::{Account, AccountId, Quota, QuotaPoolSemantics, QuotaStatus, QuotaWindow};
use crate::settings;

/// 调度把「等待下一轮」的旧数据标成 stale 时使用的解释性错误。
///
/// 它不是鉴权失败，也不代表账号不可用；只表示这份数字已经超过自动决策可接受的年龄。
pub const SCHEDULED_STALE_REASON: &str =
    "quota data is older than the decision window; waiting for the next scheduled refresh";

/// 自适应调度的运行时参数（从 [`settings::Quota`] 提取，避免调用方散落硬编码）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleConfig {
    /// 激活账号的基础刷新间隔。
    pub active_interval: ChronoDuration,
    /// 停放账号的基础刷新间隔。
    pub parked_interval: ChronoDuration,
    /// 已确认耗尽账号的基础刷新间隔。
    pub exhausted_interval: ChronoDuration,
    /// 自动决策可接受的最大数据年龄。
    pub decision_max_age: ChronoDuration,
    /// 激活账号接近阈值多少个百分点时提前确认候选。
    pub candidate_margin_pct: f64,
    /// 接近阈值时最多提前刷新的候选数。
    pub candidate_limit: usize,
    /// 正向抖动上限（百分比 0~100）。
    pub jitter_pct: f64,
    /// 已知 reset 后的显式刷新延迟。
    pub reset_delay: ChronoDuration,
    /// 同一账号两次真实采集的通用下限（Provider 还可能更严）。
    pub min_refresh: ChronoDuration,
    /// 同一进程并发采集上限。
    pub max_concurrent_fetches: usize,
    /// 同一 Provider 并发采集上限。
    pub max_concurrent_per_provider: usize,
}

impl ScheduleConfig {
    /// 从 `[quota]` 配置构造；所有间隔都不会早于 `min_refresh_interval_ms`。
    pub fn from_settings(quota: &settings::Quota) -> Self {
        Self {
            active_interval: millis(quota.active_refresh_interval_ms),
            parked_interval: millis(quota.parked_refresh_interval_ms),
            exhausted_interval: millis(quota.exhausted_refresh_interval_ms),
            decision_max_age: ChronoDuration::milliseconds(quota.decision_max_age_ms),
            candidate_margin_pct: quota.candidate_refresh_margin_pct.clamp(0.0, 100.0),
            candidate_limit: quota.candidate_refresh_limit,
            jitter_pct: quota.refresh_jitter_pct.clamp(0.0, 100.0),
            reset_delay: millis(quota.reset_refresh_delay_ms),
            min_refresh: millis(quota.min_refresh_interval_ms),
            max_concurrent_fetches: quota.max_concurrent_fetches.max(1),
            max_concurrent_per_provider: quota.max_concurrent_per_provider.max(1),
        }
    }

    /// 读取当前生效的全局配置快照。daemon 每轮重新调用，CLI 启动时一次即可。
    pub fn current() -> Self {
        Self::from_settings(&settings::current().quota)
    }

    /// 某类账号的基础间隔（已应用通用下限）。
    pub fn interval_for(&self, class: RefreshClass) -> ChronoDuration {
        let base = match class {
            RefreshClass::Active => self.active_interval,
            RefreshClass::Parked => self.parked_interval,
            RefreshClass::Exhausted => self.exhausted_interval,
        };
        base.max(self.min_refresh)
    }
}

/// 账号在后台调度中的节奏分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshClass {
    /// 当前激活账号：更频繁，保证自动切换及时。
    Active,
    /// 非激活、未确认耗尽：正常候选池节奏。
    Parked,
    /// 已确认耗尽：低频检查，只用于发现提前发放。
    Exhausted,
}

impl RefreshClass {
    /// 稳定字符串，供日志与测试断言。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Parked => "parked",
            Self::Exhausted => "exhausted",
        }
    }
}

/// 调度视角下的一个账号观测：账号元数据 + 最近一次确认额度 + 原始观测时间。
///
/// `observed_at = None` 表示本机没有任何可展示的确认数据。
#[derive(Debug, Clone)]
pub struct ObservedAccount {
    pub account: Account,
    pub quotas: Vec<Quota>,
    /// 最近一次**真实返回**的观测时间；缓存复用必须保留它，不能改写成 now。
    pub observed_at: Option<DateTime<Utc>>,
}

impl ObservedAccount {
    pub fn new(account: Account, quotas: Vec<Quota>, observed_at: Option<DateTime<Utc>>) -> Self {
        Self {
            account,
            quotas,
            observed_at,
        }
    }
}

/// 账号是否已确认耗尽（不可用）。
///
/// 叠加池：任一 gating 窗口耗尽即不可用；并行池：全部 gating 池耗尽才不可用。
/// 只认 `limit > 0` 且状态明确 `Exhausted` 的窗口，未知/空窗口不能当耗尽证据。
pub fn confirmed_exhausted(quotas: &[Quota], semantics: QuotaPoolSemantics) -> bool {
    let gating: Vec<&Quota> = quotas.iter().filter(|q| gates_auto_swap(q)).collect();
    if gating.is_empty() {
        return false;
    }
    if parallel_pools(semantics, &gating) {
        gating
            .iter()
            .all(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted))
    } else {
        gating
            .iter()
            .any(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted))
    }
}

/// 当前账号的调度节奏：激活优先；其次才是耗尽 / 停放。
pub fn refresh_class(account: &ObservedAccount, semantics: QuotaPoolSemantics) -> RefreshClass {
    if account.account.active {
        return RefreshClass::Active;
    }
    if confirmed_exhausted(&account.quotas, semantics) {
        RefreshClass::Exhausted
    } else {
        RefreshClass::Parked
    }
}

/// 给基础间隔加确定性的**正向**抖动，避免多账号同一秒打向同一端点。
///
/// 抖动只会在 `[1ms, interval * jitter_pct]` 内增加等待，不会提前；同一账号每轮
/// 得到相同偏移，便于用户理解和测试。`jitter_pct <= 0` 时原样返回。
pub fn jittered_interval(
    provider: &str,
    account_id: &str,
    interval: ChronoDuration,
    jitter_pct: f64,
) -> ChronoDuration {
    let interval = interval.max(ChronoDuration::zero());
    let pct = jitter_pct.clamp(0.0, 100.0);
    if pct <= 0.0 {
        return interval;
    }
    let base_ms = interval.num_milliseconds();
    if base_ms <= 0 {
        return interval;
    }
    let max_extra_ms = ((base_ms as f64) * pct / 100.0).floor() as i64;
    if max_extra_ms <= 0 {
        return interval;
    }
    let extra_ms = 1 + (stable_hash(provider, account_id) % max_extra_ms as u64) as i64;
    interval + ChronoDuration::milliseconds(extra_ms)
}

/// 某账号下一次后台采集的绝对时间。
///
/// - 有观测：`observed_at + 间隔 + 抖动`。
/// - 无观测：立即到期。
/// - 间隔已按通用下限钳制，且抖动只增不减；Provider 更严的下限仍由协调器执行。
pub fn next_due_at(
    provider: &str,
    account_id: &str,
    class: RefreshClass,
    observed_at: Option<DateTime<Utc>>,
    config: &ScheduleConfig,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    next_due_at_with_deadline(provider, account_id, class, observed_at, None, config, now)
}

/// 同 [`next_due_at`]，但额外尊重协调器落盘的 `next_attempt_at`（请求地板 / 服务端
/// Retry-After）。截止时间更晚时以截止时间为准，调度器永远不能提前绕过它。
pub fn next_due_at_with_deadline(
    provider: &str,
    account_id: &str,
    class: RefreshClass,
    observed_at: Option<DateTime<Utc>>,
    not_before: Option<DateTime<Utc>>,
    config: &ScheduleConfig,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let scheduled = match observed_at {
        None => now,
        Some(observed_at) => {
            let interval = config.interval_for(class);
            observed_at + jittered_interval(provider, account_id, interval, config.jitter_pct)
        }
    };
    match not_before {
        Some(deadline) => scheduled.max(deadline),
        None => scheduled,
    }
}

/// 当前所有账号里最早的一次到期时间；无账号时返回 `None`。
pub fn next_due(
    accounts: &[ObservedAccount],
    semantics: QuotaPoolSemantics,
    config: &ScheduleConfig,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    accounts
        .iter()
        .map(|account| {
            next_due_at(
                &account.account.provider,
                &account.account.id.0,
                refresh_class(account, semantics),
                account.observed_at,
                config,
                now,
            )
        })
        .min()
}

/// 本节次真正到期、需要后台采集的账号（保持输入顺序）。
pub fn due_accounts(
    accounts: &[ObservedAccount],
    semantics: QuotaPoolSemantics,
    config: &ScheduleConfig,
    now: DateTime<Utc>,
) -> Vec<(String, AccountId)> {
    accounts
        .iter()
        .filter(|account| {
            next_due_at(
                &account.account.provider,
                &account.account.id.0,
                refresh_class(account, semantics),
                account.observed_at,
                config,
                now,
            ) <= now
        })
        .map(|account| (account.account.provider.clone(), account.account.id.clone()))
        .collect()
}

/// 自动决策是否可以使用这份数据。
///
/// 只接受「不晚于 `decision_max_age`」的真实观测；未来时间戳（时钟回拨/伪造）
/// 一律视为不新鲜，宁可显示 stale 也不拿它做切换依据。
pub fn fresh_for_decision(
    observed_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &ScheduleConfig,
) -> bool {
    let Some(observed_at) = observed_at else {
        return false;
    };
    let age = now - observed_at;
    if age < ChronoDuration::zero() {
        return false;
    }
    config.decision_max_age >= ChronoDuration::zero() && age <= config.decision_max_age
}

/// 把观测时间映射成展示/决策使用的 fetch 状态。
///
/// 无观测 → `Loading`（还没有任何确认结果）；超过决策年龄 → `Stale` 并保留原始
/// `cached_at`。调度等待本身不会产生 `Ready`，也不会伪造 100% 额度。
pub fn fetch_state_for_observation(
    observed_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &ScheduleConfig,
) -> QuotaFetchState {
    let Some(observed_at) = observed_at else {
        return QuotaFetchState::Loading;
    };
    if fresh_for_decision(Some(observed_at), now, config) {
        QuotaFetchState::Ready
    } else {
        QuotaFetchState::Stale {
            cached_at: observed_at,
            error: SCHEDULED_STALE_REASON.to_string(),
        }
    }
}

/// 候选预热的执行计划：本轮实际刷新谁、以及因上限被推迟的候选。
///
/// `deferred` 仅用于可观测性（记录 `deferred` 事件）；它们仍按普通停放节奏刷新，
/// 不会因为本轮没选中而失去额度跟踪。
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRefreshPlan {
    pub selected: Vec<AccountId>,
    pub deferred: Vec<AccountId>,
}

/// 激活账号接近/越过切换条件时，最多预热 `candidate_limit` 个候选。
///
/// 只挑「缺真正新鲜确认」的非 manual_only、非当前候选；已有新鲜
/// 数据的候选不再重复请求。这样阈值真正被越过时通常已有可确认目标，又不会整池刷新。
/// 候选旧数据即使显示耗尽也值得一次刷新——可能已提前恢复；上限保证不会整池重查。
/// 激活账号自身数据不新鲜时返回空：先确认当前号，再谈候选。
pub fn candidate_refresh_targets(
    accounts: &[ObservedAccount],
    threshold: f64,
    config: &ScheduleConfig,
    now: DateTime<Utc>,
) -> Vec<AccountId> {
    candidate_refresh_plan(accounts, threshold, config, now).selected
}

/// 同 [`candidate_refresh_targets`]，但同时返回因上限被推迟的候选。
pub fn candidate_refresh_plan(
    accounts: &[ObservedAccount],
    threshold: f64,
    config: &ScheduleConfig,
    now: DateTime<Utc>,
) -> CandidateRefreshPlan {
    let empty = CandidateRefreshPlan {
        selected: Vec::new(),
        deferred: Vec::new(),
    };
    let Some(active) = accounts.iter().find(|a| a.account.active) else {
        return empty;
    };
    if active.account.manual_only() {
        return empty;
    }
    if !fresh_for_decision(active.observed_at, now, config) {
        return empty;
    }
    if !active_needs_candidate_attention(active, threshold, config.candidate_margin_pct) {
        return empty;
    }

    let mut candidates: Vec<&ObservedAccount> = accounts
        .iter()
        .filter(|a| !a.account.active)
        .filter(|a| !a.account.manual_only())
        .filter(|a| !fresh_for_decision(a.observed_at, now, config))
        .collect();
    // 完全没有数据的候选优先，其次沿用账号优先级与稳定 ID，确保每轮选择可复现。
    candidates.sort_by(|a, b| {
        a.observed_at
            .is_some()
            .cmp(&b.observed_at.is_some())
            .then(a.account.priority.cmp(&b.account.priority))
            .then(a.account.id.0.cmp(&b.account.id.0))
    });
    let mut selected: Vec<AccountId> = candidates
        .iter()
        .take(config.candidate_limit)
        .map(|a| a.account.id.clone())
        .collect();
    let deferred: Vec<AccountId> = candidates
        .iter()
        .skip(config.candidate_limit)
        .map(|a| a.account.id.clone())
        .collect();
    selected.dedup();
    CandidateRefreshPlan { selected, deferred }
}

/// 激活账号是否已需要候选确认：至少一个 gating 窗口耗尽，或小时级窗口进入
/// 「阈值 - margin」区间（含越过阈值）。
fn active_needs_candidate_attention(
    active: &ObservedAccount,
    threshold: f64,
    margin_pct: f64,
) -> bool {
    let near = (threshold * 100.0 - margin_pct).max(0.0);
    active
        .quotas
        .iter()
        .filter(|q| gates_auto_swap(q))
        .any(|q| {
            (q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted))
                || (matches!(q.window, QuotaWindow::FiveHour)
                    && q.usage_ratio()
                        .map(|ratio| ratio * 100.0 >= near)
                        .unwrap_or(false))
        })
}

/// 账号从「不可用」恢复的预计时间；可用 / 缺信息时返回 `None`。
///
/// 叠加池取阻塞窗口中最晚的 reset（任一缺 reset 则未知）；并行池取最早者。
/// 与自动切换的资金池语义保持一致，避免调度器用错恢复时刻。
pub fn account_recovery_reset(
    quotas: &[Quota],
    semantics: QuotaPoolSemantics,
    threshold: f64,
) -> Option<DateTime<Utc>> {
    let gating: Vec<&Quota> = quotas.iter().filter(|q| gates_auto_swap(q)).collect();
    if gating.is_empty() {
        return None;
    }
    if parallel_pools(semantics, &gating) {
        let five_hour_over = gating.iter().any(|q| {
            matches!(q.window, QuotaWindow::FiveHour)
                && !matches!(q.status, QuotaStatus::Unknown)
                && q.is_above(threshold)
        });
        let all_exhausted = gating
            .iter()
            .all(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted));
        if !five_hour_over && !all_exhausted {
            return None;
        }
        // 并行池任一恢复即恢复，取最早的已知 reset。
        return gating.iter().filter_map(|q| q.reset_at).min();
    }

    let mut blocking: Vec<&Quota> = gating
        .into_iter()
        .filter(|q| is_blocking(q, threshold))
        .collect();
    if blocking.is_empty() {
        return None;
    }
    let mut latest: Option<DateTime<Utc>> = None;
    for quota in blocking.drain(..) {
        let reset = quota.reset_at?;
        latest = Some(latest.map_or(reset, |current| current.max(reset)));
    }
    latest
}

/// 从观测账号里提取「已知恢复时刻」候选（每个账号至多一个）。
///
/// 仅用于**恢复排序**（叠加池最晚、并行池最早）；**不**用于调度已知 reset 到期。
/// 展示用倒计时到期必须刷新 healthy 账号，见 [`reset_candidates_for_account`]。
pub fn reset_candidates(
    accounts: &[ObservedAccount],
    semantics: QuotaPoolSemantics,
    threshold: f64,
) -> Vec<ResetCandidate> {
    accounts
        .iter()
        .filter_map(|account| {
            account_recovery_reset(&account.quotas, semantics, threshold).map(|reset_at| {
                ResetCandidate {
                    provider: account.account.provider.clone(),
                    account_id: account.account.id.clone(),
                    reset_at,
                    not_before: None,
                }
            })
        })
        .collect()
}

/// 枚举账号当前可见的 gating 窗口 reset 时间（排除 `ResetCredits`）。
///
/// 这是「展示倒计时到期」的调度输入，与账号是否健康无关：健康 5h 用到 40% 也应在
/// reset 到期后刷新一次。同一账号多个窗口共享同一 reset 时间只保留一个。
pub fn gating_reset_times(quotas: &[Quota]) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = quotas
        .iter()
        .filter(|quota| gates_auto_swap(quota))
        .filter_map(|quota| quota.reset_at)
        .collect();
    times.sort_unstable();
    times.dedup();
    times
}

/// 把账号的每个已知 gating reset 时间转成调度候选。
///
/// 不同窗口不同到期时间会产生多个候选（后续窗口在第一次刷新后继续调度）；
/// 同一时间只产生一个候选，daemon 侧再按账号去重，保证每次到期最多查询该账号一次。
pub fn reset_candidates_for_account(
    provider: &str,
    account_id: &AccountId,
    quotas: &[Quota],
    not_before: Option<DateTime<Utc>>,
) -> Vec<ResetCandidate> {
    gating_reset_times(quotas)
        .into_iter()
        .map(|reset_at| ResetCandidate {
            provider: provider.to_string(),
            account_id: account_id.clone(),
            reset_at,
            not_before,
        })
        .collect()
}

/// 从 quota 缓存文件解析出的原始观测（用于 reset 唤醒种子）。
///
/// 升级前的缓存可能已有额度与 reset 时间，但还没有协调器记录；调度器必须能直接用这些
/// 观测排定 reset 唤醒，不能等到第一次真实采集之后。只读取 `entries` 的只读副本，
/// 不写回、不改缓存，也不把 raw 数据当展示值。
#[derive(Debug, Clone)]
pub struct CachedResetSeed {
    pub provider: String,
    pub account_id: AccountId,
    pub quotas: Vec<Quota>,
}

#[derive(Debug, Deserialize)]
struct CachedFileShape {
    #[serde(default)]
    entries: HashMap<String, CachedEntryShape>,
}

#[derive(Debug, Deserialize)]
struct CachedEntryShape {
    #[serde(default)]
    quotas: Vec<Quota>,
}

/// 解析 quota_cache.json 的原始 `entries`（兼容升级前已有文件）。
///
/// 缓存键格式固定为 `provider::account`；账号部分可包含 `::`，因此只按第一个分隔符切分。
/// 解析失败/结构不符返回空，绝不 panic，也不触碰文件。
pub fn parse_cached_reset_seeds(json: &str) -> Vec<CachedResetSeed> {
    let Ok(parsed) = serde_json::from_str::<CachedFileShape>(json) else {
        return Vec::new();
    };
    let mut seeds: Vec<CachedResetSeed> = parsed
        .entries
        .into_iter()
        .filter_map(|(key, entry)| {
            let (provider, account_id) = key.split_once("::")?;
            if provider.is_empty() || account_id.is_empty() || entry.quotas.is_empty() {
                return None;
            }
            Some(CachedResetSeed {
                provider: provider.to_string(),
                account_id: AccountId(account_id.to_string()),
                quotas: entry.quotas,
            })
        })
        .collect();
    seeds.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.account_id.0.cmp(&b.account_id.0))
    });
    seeds
}

/// 一个账号的已知恢复时刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetCandidate {
    pub provider: String,
    pub account_id: AccountId,
    pub reset_at: DateTime<Utc>,
    /// 协调器落盘的请求地板 / 服务端 Retry-After；存在时唤醒不得早于它。
    pub not_before: Option<DateTime<Utc>>,
}

impl ResetCandidate {
    /// 绑定协调器允许的最早请求时间（`next_attempt_at`）。
    pub fn with_not_before(mut self, not_before: Option<DateTime<Utc>>) -> Self {
        self.not_before = not_before;
        self
    }

    fn wake_at(&self, delay: ChronoDuration) -> DateTime<Utc> {
        let scheduled = self.reset_at + delay;
        match self.not_before {
            Some(deadline) => scheduled.max(deadline),
            None => scheduled,
        }
    }
}

/// 去重用的 reset 身份：同一账号同一 reset 只唤醒一次。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResetKey {
    pub provider: String,
    pub account_id: String,
    pub reset_at: DateTime<Utc>,
}

impl ResetKey {
    fn from_candidate(candidate: &ResetCandidate) -> Self {
        Self {
            provider: candidate.provider.clone(),
            account_id: candidate.account_id.0.clone(),
            reset_at: candidate.reset_at,
        }
    }
}

/// 一次显式 reset 唤醒：同一时刻到期的所有受影响账号合并成一轮。
#[derive(Debug, Clone, PartialEq)]
pub struct ResetWake {
    /// `reset_at + reset_delay`；可能已过去（表示应立刻刷新）。
    pub at: DateTime<Utc>,
    /// 本轮需要显式刷新的账号（同一账号只出现一次；keys 仍保留全部到期窗口）。
    pub targets: Vec<ResetCandidate>,
    /// 供 [`ResetTracker::mark_handled`] 记录的 key。
    pub keys: Vec<ResetKey>,
}

/// 已知 reset 唤醒的去重状态。
///
/// 只保存 key（provider/account/reset 时间），不含凭证。daemon 重启即清空；
/// 同一个 reset 被唤醒并查询后，即使服务端仍返回同一 reset 时间也不会重复查询，
/// 直到数据更新出新的 reset（或调用方 [`Self::prune`] 清掉已消失的 key）。
#[derive(Debug, Default)]
pub struct ResetTracker {
    handled: HashSet<ResetKey>,
}

impl ResetTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 本轮已经到期（`reset_at + delay <= now`）的唤醒；没有则为 `None`。
    ///
    /// 所有已到期的 reset 合并成一次唤醒，避免同一轮因多个窗口各查一次。
    pub fn due_wake(
        &self,
        now: DateTime<Utc>,
        candidates: &[ResetCandidate],
        delay: ChronoDuration,
    ) -> Option<ResetWake> {
        self.build_wake(now, candidates, delay, true)
    }

    /// 下一次（含已到期的）唤醒；用于计算 daemon 睡眠时间。
    pub fn next_wake(
        &self,
        now: DateTime<Utc>,
        candidates: &[ResetCandidate],
        delay: ChronoDuration,
    ) -> Option<ResetWake> {
        self.build_wake(now, candidates, delay, false)
    }

    /// 标记一次唤醒已处理。调用方应在真正发起采集前标记，失败也不会原地重查。
    pub fn mark_handled(&mut self, wake: &ResetWake) {
        self.handled.extend(wake.keys.iter().cloned());
    }

    /// 清理已经不在当前数据中的 key，限制内存并允许新窗口重新调度。
    pub fn prune(&mut self, candidates: &[ResetCandidate]) {
        let live: HashSet<ResetKey> = candidates.iter().map(ResetKey::from_candidate).collect();
        self.handled.retain(|key| live.contains(key));
    }

    /// 测试/诊断用：已记录的 reset key 数量。
    pub fn handled_len(&self) -> usize {
        self.handled.len()
    }

    fn build_wake(
        &self,
        now: DateTime<Utc>,
        candidates: &[ResetCandidate],
        delay: ChronoDuration,
        due_only: bool,
    ) -> Option<ResetWake> {
        let mut pending: Vec<(&ResetCandidate, DateTime<Utc>)> = candidates
            .iter()
            .filter(|candidate| !self.handled.contains(&ResetKey::from_candidate(candidate)))
            .map(|candidate| (candidate, candidate.wake_at(delay)))
            .filter(|(_, at)| !due_only || *at <= now)
            .collect();
        if pending.is_empty() {
            return None;
        }
        if due_only {
            // 所有到期 reset 合并：它们都已经过 reset+delay，集中刷新一次即可。
            let at = pending
                .iter()
                .map(|(_, at)| *at)
                .min()
                .unwrap_or(now)
                .min(now);
            let mut seen_keys = HashSet::new();
            let mut seen_targets: HashSet<(String, String)> = HashSet::new();
            let mut targets = Vec::new();
            let mut keys = Vec::new();
            for (candidate, _) in pending.drain(..) {
                let key = ResetKey::from_candidate(candidate);
                if seen_keys.insert(key.clone()) {
                    keys.push(key);
                }
                let target_key = (candidate.provider.clone(), candidate.account_id.0.clone());
                if seen_targets.insert(target_key) {
                    targets.push(candidate.clone());
                }
            }
            return Some(ResetWake { at, targets, keys });
        }

        let at = pending.iter().map(|(_, at)| *at).min()?;
        let mut seen_keys = HashSet::new();
        let mut seen_targets: HashSet<(String, String)> = HashSet::new();
        let mut targets = Vec::new();
        let mut keys = Vec::new();
        for (candidate, candidate_at) in pending.drain(..) {
            if candidate_at != at {
                continue;
            }
            let key = ResetKey::from_candidate(candidate);
            if seen_keys.insert(key.clone()) {
                keys.push(key);
            }
            let target_key = (candidate.provider.clone(), candidate.account_id.0.clone());
            if seen_targets.insert(target_key) {
                targets.push(candidate.clone());
            }
        }
        Some(ResetWake { at, targets, keys })
    }
}

/// 计算 daemon 下一轮睡眠时间。
///
/// 取以下三者的最小值，并保证严格大于零：
/// - `existing_wake`：既有活跃 / 空闲轮询间隔，必须保留；
/// - `next_due`：自适应账号到期时间（已到期时用 `overdue_retry`，避免零时长忙循环）；
/// - `next_reset`：已知 reset + 延迟的显式唤醒（已到期时只留 1ms 让本轮立即刷新）。
///
/// 传入的 `next_due = None` 时（空闲模式）不会把睡眠缩短到自适应节奏。
pub fn next_sleep(
    now: DateTime<Utc>,
    existing_wake: std::time::Duration,
    next_due: Option<DateTime<Utc>>,
    next_reset: Option<DateTime<Utc>>,
    overdue_retry: std::time::Duration,
) -> std::time::Duration {
    const MIN_RESET_WAKE: std::time::Duration = std::time::Duration::from_millis(1);

    let mut sleep = existing_wake.max(MIN_RESET_WAKE);
    if let Some(due) = next_due {
        let candidate = if due > now {
            (due - now).to_std().unwrap_or(overdue_retry)
        } else {
            overdue_retry
        };
        sleep = sleep.min(candidate.max(MIN_RESET_WAKE));
    }
    if let Some(reset) = next_reset {
        let candidate = if reset > now {
            (reset - now).to_std().unwrap_or(MIN_RESET_WAKE)
        } else {
            MIN_RESET_WAKE
        };
        sleep = sleep.min(candidate.max(MIN_RESET_WAKE));
    }
    sleep.max(MIN_RESET_WAKE)
}

/// 把毫秒配置转成 chrono 时长（负值/溢出保守归零/封顶）。
fn millis(value: u64) -> ChronoDuration {
    ChronoDuration::milliseconds(value.min(i64::MAX as u64) as i64)
}

/// FNV-1a：只用于稳定抖动，不是安全哈希。
fn stable_hash(provider: &str, account_id: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for byte in provider
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(account_id.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn gates_auto_swap(q: &Quota) -> bool {
    !matches!(q.window, QuotaWindow::ResetCredits)
}

fn is_blocking(q: &Quota, threshold: f64) -> bool {
    (q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted))
        || (matches!(q.window, QuotaWindow::FiveHour)
            && !matches!(q.status, QuotaStatus::Unknown)
            && q.is_above(threshold))
}

fn parallel_pools(semantics: QuotaPoolSemantics, quotas: &[&Quota]) -> bool {
    semantics == QuotaPoolSemantics::Parallel
        && quotas.iter().any(|q| {
            matches!(
                q.window,
                QuotaWindow::FirstPartyModels | QuotaWindow::Credits | QuotaWindow::Api
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap()
    }

    fn config() -> ScheduleConfig {
        ScheduleConfig::from_settings(&settings::Quota {
            refresh_jitter_pct: 0.0,
            ..settings::Quota::default()
        })
    }

    fn account(id: &str, active: bool) -> Account {
        Account {
            provider: "test".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: t0(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    fn quota(
        window: QuotaWindow,
        used: u64,
        limit: u64,
        status: QuotaStatus,
        reset_at: Option<DateTime<Utc>>,
    ) -> Quota {
        Quota {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            window,
            used,
            limit,
            reset_at,
            status,
            note: None,
        }
    }

    fn observed(
        id: &str,
        active: bool,
        quotas: Vec<Quota>,
        observed_at: Option<DateTime<Utc>>,
    ) -> ObservedAccount {
        ObservedAccount::new(account(id, active), quotas, observed_at)
    }

    #[test]
    fn classes_and_intervals_follow_state() {
        let cfg = config();
        assert_eq!(cfg.interval_for(RefreshClass::Active).num_seconds(), 180);
        assert_eq!(cfg.interval_for(RefreshClass::Parked).num_seconds(), 900);
        assert_eq!(cfg.interval_for(RefreshClass::Exhausted).num_seconds(), 900);

        let active = observed("active", true, Vec::new(), Some(t0()));
        assert_eq!(
            refresh_class(&active, QuotaPoolSemantics::Stacked),
            RefreshClass::Active
        );

        let exhausted = observed(
            "parked",
            false,
            vec![quota(
                QuotaWindow::FiveHour,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(t0()),
            )],
            Some(t0()),
        );
        assert_eq!(
            refresh_class(&exhausted, QuotaPoolSemantics::Stacked),
            RefreshClass::Exhausted
        );

        let parked = observed(
            "parked",
            false,
            vec![quota(QuotaWindow::FiveHour, 10, 100, QuotaStatus::Ok, None)],
            Some(t0()),
        );
        assert_eq!(
            refresh_class(&parked, QuotaPoolSemantics::Stacked),
            RefreshClass::Parked
        );
    }

    #[test]
    fn parallel_exhaustion_requires_every_pool() {
        let one_pool = vec![
            quota(
                QuotaWindow::FirstPartyModels,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(t0()),
            ),
            quota(QuotaWindow::Credits, 5, 100, QuotaStatus::Ok, None),
        ];
        assert!(!confirmed_exhausted(
            &one_pool,
            QuotaPoolSemantics::Parallel
        ));
        assert!(confirmed_exhausted(&one_pool, QuotaPoolSemantics::Stacked));

        let all_pools = vec![
            quota(
                QuotaWindow::FirstPartyModels,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(t0()),
            ),
            quota(
                QuotaWindow::Credits,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(t0()),
            ),
        ];
        assert!(confirmed_exhausted(
            &all_pools,
            QuotaPoolSemantics::Parallel
        ));
    }

    #[test]
    fn jitter_is_positive_bounded_and_stable() {
        let interval = ChronoDuration::minutes(3);
        let mut seen_different = false;
        for id in ["a@x", "b@x", "c@x", "d@x"] {
            let first = jittered_interval("test", id, interval, 10.0);
            let second = jittered_interval("test", id, interval, 10.0);
            assert_eq!(first, second, "jitter must be deterministic per account");
            assert!(first > interval, "positive jitter must add wait");
            assert!(
                first <= interval + ChronoDuration::seconds(18),
                "jitter must stay within 10%: {first:?}"
            );
            if first != interval + ChronoDuration::seconds(17) {
                seen_different = true;
            }
        }
        assert!(seen_different, "different accounts should not all collide");
        assert_eq!(jittered_interval("test", "a@x", interval, 0.0), interval);
        // 负/越界百分比也被钳制，不能产生负抖动或超过 100%。
        assert!(jittered_interval("test", "a@x", interval, -5.0) >= interval);
        assert!(
            jittered_interval("test", "a@x", interval, 250.0)
                <= interval + ChronoDuration::minutes(3)
        );
    }

    #[test]
    fn interval_never_below_min_refresh_floor() {
        let cfg = ScheduleConfig::from_settings(&settings::Quota {
            active_refresh_interval_ms: 1_000,
            parked_refresh_interval_ms: 0,
            exhausted_refresh_interval_ms: 0,
            min_refresh_interval_ms: 90_000,
            refresh_jitter_pct: 0.0,
            ..settings::Quota::default()
        });
        assert_eq!(
            cfg.interval_for(RefreshClass::Active).num_milliseconds(),
            90_000
        );
        assert_eq!(
            cfg.interval_for(RefreshClass::Parked).num_milliseconds(),
            90_000
        );
        assert_eq!(
            cfg.interval_for(RefreshClass::Exhausted).num_milliseconds(),
            90_000
        );
    }

    #[test]
    fn due_time_is_observation_plus_interval() {
        let cfg = config();
        let active_due = next_due_at(
            "test",
            "a@x",
            RefreshClass::Active,
            Some(t0() - ChronoDuration::seconds(200)),
            &cfg,
            t0(),
        );
        assert_eq!(active_due, t0() - ChronoDuration::seconds(20));

        let parked_due = next_due_at(
            "test",
            "b@x",
            RefreshClass::Parked,
            Some(t0() - ChronoDuration::seconds(100)),
            &cfg,
            t0(),
        );
        assert_eq!(parked_due, t0() + ChronoDuration::seconds(800));

        let never_seen = next_due_at("test", "c@x", RefreshClass::Parked, None, &cfg, t0());
        assert_eq!(never_seen, t0());

        let accounts = vec![
            observed("a@x", true, Vec::new(), Some(t0())),
            observed("b@x", false, Vec::new(), Some(t0())),
        ];
        assert_eq!(
            next_due(&accounts, QuotaPoolSemantics::Stacked, &cfg, t0()),
            Some(t0() + ChronoDuration::minutes(3))
        );
        assert!(due_accounts(&accounts, QuotaPoolSemantics::Stacked, &cfg, t0()).is_empty());
        assert_eq!(
            due_accounts(
                &accounts,
                QuotaPoolSemantics::Stacked,
                &cfg,
                t0() + ChronoDuration::minutes(4)
            )
            .len(),
            1
        );
    }

    #[test]
    fn stale_boundary_excludes_old_data_from_decisions() {
        let cfg = config();
        let max_age = cfg.decision_max_age;
        assert_eq!(
            fetch_state_for_observation(Some(t0() - max_age), t0(), &cfg),
            QuotaFetchState::Ready
        );
        let stale = fetch_state_for_observation(
            Some(t0() - max_age - ChronoDuration::milliseconds(1)),
            t0(),
            &cfg,
        );
        match stale {
            QuotaFetchState::Stale { cached_at, error } => {
                assert_eq!(cached_at, t0() - max_age - ChronoDuration::milliseconds(1));
                assert_eq!(error, SCHEDULED_STALE_REASON);
            }
            other => panic!("expected stale, got {other:?}"),
        }
        assert!(!fresh_for_decision(None, t0(), &cfg));
        assert!(!fresh_for_decision(
            Some(t0() + ChronoDuration::seconds(1)),
            t0(),
            &cfg
        ));
        assert_eq!(
            fetch_state_for_observation(None, t0(), &cfg),
            QuotaFetchState::Loading
        );
    }

    #[test]
    fn candidate_cap_excludes_manual_and_fresh() {
        let cfg = config();
        // 阈值 0.99、margin 15 → 84% 起进入候选提前确认区间。
        let active = observed(
            "active",
            true,
            vec![quota(
                QuotaWindow::FiveHour,
                85,
                100,
                QuotaStatus::Warn,
                None,
            )],
            Some(t0()),
        );
        let fresh = observed(
            "fresh",
            false,
            vec![quota(QuotaWindow::FiveHour, 1, 100, QuotaStatus::Ok, None)],
            Some(t0()),
        );
        let manual = {
            let mut a = observed("manual", false, Vec::new(), None);
            a.account.extra.insert("manual_only".into(), true.into());
            a
        };
        let exhausted = observed(
            "exhausted",
            false,
            vec![quota(
                QuotaWindow::FiveHour,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(t0()),
            )],
            None,
        );
        let mut stale_a = observed("stale-a", false, Vec::new(), None);
        stale_a.account.priority = 1;
        let mut stale_b = observed("stale-b", false, Vec::new(), None);
        stale_b.account.priority = 2;
        let mut stale_c = observed("stale-c", false, Vec::new(), None);
        stale_c.account.priority = 3;

        let accounts = vec![active, fresh, manual, exhausted, stale_a, stale_b, stale_c];
        let plan = candidate_refresh_plan(&accounts, 0.99, &cfg, t0());
        assert_eq!(
            plan.selected,
            vec![AccountId("stale-a".into()), AccountId("stale-b".into())],
            "must cap at two, skip manual/fresh, prefer unknown data by priority"
        );
        assert_eq!(
            plan.deferred,
            vec![AccountId("stale-c".into()), AccountId("exhausted".into())],
            "backlog must be tracked for bounded deferred telemetry"
        );
    }

    #[test]
    fn candidates_wait_for_fresh_active_confirmation() {
        let cfg = config();
        let stale_active = observed(
            "active",
            true,
            vec![quota(
                QuotaWindow::FiveHour,
                99,
                100,
                QuotaStatus::Warn,
                None,
            )],
            Some(t0() - cfg.decision_max_age - ChronoDuration::seconds(1)),
        );
        let candidate = observed("candidate", false, Vec::new(), None);
        assert!(candidate_refresh_targets(&[stale_active, candidate], 0.99, &cfg, t0()).is_empty());
    }

    #[test]
    fn healthy_active_does_not_refresh_candidates() {
        let cfg = config();
        let active = observed(
            "active",
            true,
            vec![quota(QuotaWindow::FiveHour, 10, 100, QuotaStatus::Ok, None)],
            Some(t0()),
        );
        let candidate = observed("candidate", false, Vec::new(), None);
        assert!(candidate_refresh_targets(&[active, candidate], 0.99, &cfg, t0()).is_empty());
    }

    #[test]
    fn stacked_recovery_uses_latest_blocking_reset() {
        let five_hour = t0() + ChronoDuration::hours(1);
        let seven_day = t0() + ChronoDuration::days(2);
        let quotas = vec![
            quota(
                QuotaWindow::FiveHour,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(five_hour),
            ),
            quota(
                QuotaWindow::SevenDay,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(seven_day),
            ),
        ];
        assert_eq!(
            account_recovery_reset(&quotas, QuotaPoolSemantics::Stacked, 0.99),
            Some(seven_day)
        );
        // 任一阻塞窗口缺 reset → 恢复时间未知，不能用来定时唤醒。
        let unknown = vec![
            quota(
                QuotaWindow::FiveHour,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(five_hour),
            ),
            quota(
                QuotaWindow::SevenDay,
                100,
                100,
                QuotaStatus::Exhausted,
                None,
            ),
        ];
        assert_eq!(
            account_recovery_reset(&unknown, QuotaPoolSemantics::Stacked, 0.99),
            None
        );
    }

    #[test]
    fn parallel_recovery_uses_earliest_reset_only_when_unusable() {
        let first = t0() + ChronoDuration::hours(1);
        let second = t0() + ChronoDuration::hours(2);
        let all_exhausted = vec![
            quota(
                QuotaWindow::FirstPartyModels,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(first),
            ),
            quota(
                QuotaWindow::Credits,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(second),
            ),
        ];
        assert_eq!(
            account_recovery_reset(&all_exhausted, QuotaPoolSemantics::Parallel, 0.99),
            Some(first)
        );
        let usable = vec![
            quota(
                QuotaWindow::FirstPartyModels,
                100,
                100,
                QuotaStatus::Exhausted,
                Some(first),
            ),
            quota(QuotaWindow::Credits, 10, 100, QuotaStatus::Ok, Some(second)),
        ];
        assert_eq!(
            account_recovery_reset(&usable, QuotaPoolSemantics::Parallel, 0.99),
            None
        );
    }

    #[test]
    fn reset_wake_dedups_multiple_accounts_at_same_window() {
        let cfg = config();
        let reset = t0() + ChronoDuration::hours(2);
        let candidates = vec![
            ResetCandidate {
                provider: "test".into(),
                account_id: AccountId("a".into()),
                reset_at: reset,
                not_before: None,
            },
            ResetCandidate {
                provider: "test".into(),
                account_id: AccountId("b".into()),
                reset_at: reset,
                not_before: None,
            },
            ResetCandidate {
                provider: "other".into(),
                account_id: AccountId("c".into()),
                reset_at: reset + ChronoDuration::seconds(3),
                not_before: None,
            },
        ];
        let tracker = ResetTracker::new();
        let wake = tracker
            .next_wake(t0(), &candidates, cfg.reset_delay)
            .expect("future reset should wake");
        assert_eq!(wake.at, reset + cfg.reset_delay);
        assert_eq!(wake.targets.len(), 2, "same instant dedups into one wake");
        assert_eq!(wake.keys.len(), 2);
    }

    #[test]
    fn reset_wake_is_handled_once_and_new_windows_reschedule() {
        let cfg = config();
        let reset = t0() + ChronoDuration::hours(1);
        let candidates = vec![ResetCandidate {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            reset_at: reset,
            not_before: None,
        }];
        let mut tracker = ResetTracker::new();
        let wake = tracker
            .next_wake(t0(), &candidates, cfg.reset_delay)
            .unwrap();
        assert!(tracker
            .next_wake(reset + cfg.reset_delay, &candidates, cfg.reset_delay)
            .is_some());
        assert!(tracker
            .due_wake(reset + cfg.reset_delay, &candidates, cfg.reset_delay)
            .is_some());
        tracker.mark_handled(&wake);
        assert!(
            tracker
                .due_wake(reset + cfg.reset_delay, &candidates, cfg.reset_delay)
                .is_none(),
            "same reset must not be queried again"
        );
        assert_eq!(tracker.handled_len(), 1);

        // 服务端给出新的 reset 窗口 → 重新调度。
        let next_reset = reset + ChronoDuration::hours(5);
        let next_candidates = vec![ResetCandidate {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            reset_at: next_reset,
            not_before: None,
        }];
        let next_wake = tracker
            .next_wake(t0(), &next_candidates, cfg.reset_delay)
            .expect("new reset window must reschedule");
        assert_eq!(next_wake.at, next_reset + cfg.reset_delay);

        // 账号恢复、reset 消失后再出现同一时间也不无限保留旧 key。
        tracker.prune(&[]);
        assert_eq!(tracker.handled_len(), 0);
    }

    #[test]
    fn overdue_reset_wakes_immediately_but_bounded() {
        let cfg = config();
        let candidates = vec![ResetCandidate {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            reset_at: t0() - ChronoDuration::minutes(10),
            not_before: None,
        }];
        let tracker = ResetTracker::new();
        let wake = tracker
            .due_wake(t0(), &candidates, cfg.reset_delay)
            .expect("overdue reset should be due");
        assert!(wake.at <= t0());
        let sleep = next_sleep(
            t0(),
            std::time::Duration::from_secs(60),
            None,
            Some(wake.at),
            std::time::Duration::from_secs(60),
        );
        assert_eq!(sleep, std::time::Duration::from_millis(1));
    }

    #[test]
    fn reset_and_due_never_precede_collector_deadline() {
        let cfg = config();
        let reset = t0() + ChronoDuration::hours(1);
        let deadline = reset + ChronoDuration::minutes(10);
        let candidates = vec![ResetCandidate {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            reset_at: reset,
            not_before: Some(deadline),
        }];
        let tracker = ResetTracker::new();
        let wake = tracker
            .next_wake(t0(), &candidates, cfg.reset_delay)
            .expect("reset should be scheduled");
        assert_eq!(wake.at, deadline, "collector floor/Retry-After wins");

        let due = next_due_at_with_deadline(
            "test",
            "a@x",
            RefreshClass::Active,
            Some(t0()),
            Some(t0() + ChronoDuration::minutes(2)),
            &cfg,
            t0(),
        );
        assert_eq!(due, t0() + ChronoDuration::minutes(3));
        let due_after_deadline = next_due_at_with_deadline(
            "test",
            "a@x",
            RefreshClass::Active,
            Some(t0()),
            Some(t0() + ChronoDuration::minutes(10)),
            &cfg,
            t0(),
        );
        assert_eq!(due_after_deadline, t0() + ChronoDuration::minutes(10));
    }

    #[test]
    fn sleep_uses_min_of_existing_due_and_reset_without_busy_loop() {
        let now = t0();
        let existing = std::time::Duration::from_secs(60);
        // reset 2 秒后 → 2 秒醒来，优先于 60s / 3m。
        assert_eq!(
            next_sleep(
                now,
                existing,
                Some(now + ChronoDuration::minutes(3)),
                Some(now + ChronoDuration::seconds(2)),
                std::time::Duration::from_secs(60),
            ),
            std::time::Duration::from_secs(2)
        );
        // 活跃到期在 10 秒后 → 10 秒。
        assert_eq!(
            next_sleep(
                now,
                existing,
                Some(now + ChronoDuration::seconds(10)),
                None,
                std::time::Duration::from_secs(60),
            ),
            std::time::Duration::from_secs(10)
        );
        // 已过期的普通到期（采集被协调器延后）→ 用重试间隔，不打零时长忙循环。
        assert_eq!(
            next_sleep(
                now,
                existing,
                Some(now - ChronoDuration::seconds(5)),
                None,
                std::time::Duration::from_secs(60),
            ),
            std::time::Duration::from_secs(60)
        );
        // 空闲模式：没有到期候选时保留既有 30 分钟节奏。
        assert_eq!(
            next_sleep(
                now,
                std::time::Duration::from_secs(30 * 60),
                None,
                None,
                std::time::Duration::from_secs(60),
            ),
            std::time::Duration::from_secs(30 * 60)
        );
        // 零/负候选也不会返回零。
        assert!(
            next_sleep(
                now,
                std::time::Duration::ZERO,
                Some(now),
                Some(now),
                std::time::Duration::from_secs(1),
            ) > std::time::Duration::ZERO
        );
    }

    #[test]
    fn recoverable_reset_requires_actually_unusable_active() {
        // 仅 Warn 未超阈值、也未耗尽的账号不需要 reset 唤醒。
        let healthy = vec![Quota {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            window: QuotaWindow::FiveHour,
            used: 90,
            limit: 100,
            reset_at: Some(t0() + ChronoDuration::hours(1)),
            status: QuotaStatus::Warn,
            note: None,
        }];
        assert_eq!(
            account_recovery_reset(&healthy, QuotaPoolSemantics::Stacked, 0.99),
            None
        );
    }

    #[test]
    fn healthy_windows_schedule_every_gating_reset_expiry() {
        let first = t0() + ChronoDuration::hours(1);
        let second = t0() + ChronoDuration::days(3);
        let quotas = vec![
            quota(QuotaWindow::FiveHour, 40, 100, QuotaStatus::Ok, Some(first)),
            quota(
                QuotaWindow::SevenDay,
                10,
                100,
                QuotaStatus::Ok,
                Some(second),
            ),
            // 重置道具不是 gating 窗口，不参与定时刷新。
            quota(
                QuotaWindow::ResetCredits,
                2,
                0,
                QuotaStatus::Ok,
                Some(t0() + ChronoDuration::minutes(5)),
            ),
        ];
        assert_eq!(gating_reset_times(&quotas), vec![first, second]);
        let candidates =
            reset_candidates_for_account("test", &AccountId("a".into()), &quotas, None);
        assert_eq!(candidates.len(), 2, "healthy windows must still schedule");
        assert_eq!(candidates[0].reset_at, first);
        assert_eq!(candidates[1].reset_at, second);
    }

    #[test]
    fn same_timestamp_windows_dedupe_to_one_target() {
        let reset = t0() + ChronoDuration::hours(1);
        let quotas = vec![
            quota(QuotaWindow::FiveHour, 40, 100, QuotaStatus::Ok, Some(reset)),
            quota(QuotaWindow::SevenDay, 10, 100, QuotaStatus::Ok, Some(reset)),
        ];
        let candidates =
            reset_candidates_for_account("test", &AccountId("a".into()), &quotas, None);
        assert_eq!(candidates.len(), 1);
        let tracker = ResetTracker::new();
        let wake = tracker
            .next_wake(t0(), &candidates, ChronoDuration::seconds(2))
            .expect("future reset should be scheduled");
        assert_eq!(wake.at, reset + ChronoDuration::seconds(2));
        assert_eq!(wake.targets.len(), 1);
        assert_eq!(wake.keys.len(), 1);
    }

    #[test]
    fn following_window_reschedules_after_previous_wake() {
        let first = t0() + ChronoDuration::hours(1);
        let second = t0() + ChronoDuration::hours(3);
        let quotas = vec![
            quota(QuotaWindow::FiveHour, 40, 100, QuotaStatus::Ok, Some(first)),
            quota(
                QuotaWindow::SevenDay,
                10,
                100,
                QuotaStatus::Ok,
                Some(second),
            ),
        ];
        let candidates =
            reset_candidates_for_account("test", &AccountId("a".into()), &quotas, None);
        let mut tracker = ResetTracker::new();
        let first_wake = tracker
            .next_wake(t0(), &candidates, ChronoDuration::seconds(2))
            .unwrap();
        assert_eq!(first_wake.at, first + ChronoDuration::seconds(2));
        tracker.mark_handled(&first_wake);

        let second_wake = tracker
            .next_wake(
                first + ChronoDuration::seconds(2),
                &candidates,
                ChronoDuration::seconds(2),
            )
            .expect("following window must stay scheduled");
        assert_eq!(second_wake.at, second + ChronoDuration::seconds(2));
        tracker.mark_handled(&second_wake);
        assert!(tracker
            .next_wake(
                second + ChronoDuration::seconds(2),
                &candidates,
                ChronoDuration::seconds(2)
            )
            .is_none());
    }

    #[test]
    fn overdue_windows_query_account_once_but_mark_all_handled() {
        let quotas = vec![
            quota(
                QuotaWindow::FiveHour,
                40,
                100,
                QuotaStatus::Ok,
                Some(t0() - ChronoDuration::hours(2)),
            ),
            quota(
                QuotaWindow::SevenDay,
                10,
                100,
                QuotaStatus::Ok,
                Some(t0() - ChronoDuration::hours(1)),
            ),
        ];
        let candidates =
            reset_candidates_for_account("test", &AccountId("a".into()), &quotas, None);
        let tracker = ResetTracker::new();
        let wake = tracker
            .due_wake(t0(), &candidates, ChronoDuration::seconds(2))
            .expect("overdue windows should be due");
        assert_eq!(wake.targets.len(), 1, "same account queries once");
        assert_eq!(wake.keys.len(), 2, "all overdue windows are handled");
    }

    #[test]
    fn preupgrade_cache_entries_seed_reset_schedules() {
        let raw = r#"{
            "entries": {
                "claude::a@x": {
                    "cached_at": "2026-10-24T12:00:00Z",
                    "quotas": [{
                        "provider": "claude",
                        "account_id": "a@x",
                        "window": "five_hour",
                        "used": 40,
                        "limit": 100,
                        "reset_at": "2026-10-24T13:00:00Z",
                        "status": "ok",
                        "note": null
                    }]
                },
                "codex::b": {
                    "cached_at": "2026-10-24T12:00:00Z",
                    "quotas": []
                },
                "malformed": {
                    "cached_at": "2026-10-24T12:00:00Z",
                    "quotas": [{
                        "provider": "x",
                        "account_id": "y",
                        "window": "five_hour",
                        "used": 1,
                        "limit": 100,
                        "reset_at": null,
                        "status": "ok",
                        "note": null
                    }]
                }
            },
            "failures": {}
        }"#;
        let seeds = parse_cached_reset_seeds(raw);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].provider, "claude");
        assert_eq!(seeds[0].account_id, AccountId("a@x".into()));
        assert_eq!(seeds[0].quotas.len(), 1);
        assert!(parse_cached_reset_seeds("not json").is_empty());
    }
}
