//! 产品级**编译期**默认常量集中地。
//!
//! 运行时实际生效的值来自 [`crate::settings::current`]，由 `config.toml` 覆盖。
//! 这里的常量只是 `Settings::default()` 的 fallback，缺字段 / 配置文件缺失时使用。
//!
//! 命名约定：`<DOMAIN>_<NAME>`。
//! 单位：百分比统一 0.0~1.0 或 0~100，命名里点明；时间统一毫秒。

// ============================================================
// 自动切换
// ============================================================

/// AutoSwap 触发阈值（0.0~1.0）。
///
/// `subswap` 默认入口与 `subswapd` daemon 均使用此值；
/// 任一窗口 used/limit ≥ 此值即触发切换。
///
/// 配套不变量：AGENTS.md #5。
pub const AUTO_SWAP_THRESHOLD: f64 = 0.98;

/// 自动切换冷却期（毫秒）。
///
/// 一个账号刚被切走后，在此期间不会再被选为切换目标，避免抖动。
/// daemon (M4) 使用。
pub const AUTO_SWAP_COOLDOWN_MS: i64 = 5 * 60 * 1000;

// ============================================================
// 额度状态视觉阈值（仅影响展示，不影响 AutoSwap 决策）
// ============================================================

/// Provider 将 [`crate::QuotaStatus::Warn`] 标记给 quota 的阈值（百分比 0~100）。
///
/// 用途：CLI 展示着色 / 用户感知接近上限。**不耦合 [`AUTO_SWAP_THRESHOLD`]**。
/// 设低于 AutoSwap 阈值，让用户在自动切换发生之前就能看到 WARN。
pub const QUOTA_WARN_PCT: f64 = 90.0;

/// `QuotaStatus::Exhausted` 的阈值（百分比 0~100）。通常就是 100。
pub const QUOTA_EXHAUSTED_PCT: f64 = 100.0;

/// Codex usage 实时接口字段漂移时，允许使用旧版本地缓存的最长时间。
///
/// 仅作为兼容兜底；过期缓存不参与展示/自动切换，避免 stale quota 误导策略。
pub const CODEX_USAGE_CACHE_MAX_AGE_MS: i64 = 10 * 60 * 1000;

/// 单次 quota 查询 attempt 的超时（毫秒）。
///
/// CLI 与 daemon 都通过统一重试包装查询 quota。单次 attempt 超过此值会被取消，并按
/// [`QUOTA_FETCH_RETRIES`] 决定是否重试。
pub const QUOTA_FETCH_TIMEOUT_MS: u64 = 3000;

/// quota 查询失败后的重试次数。
///
/// 这里表示「首次请求之外」额外再试几次。默认 5 次；401/403 不会重试。
pub const QUOTA_FETCH_RETRIES: u32 = 5;

/// quota 查询首次重试前等待多久（毫秒）。
///
/// 后续按 `base * 2^(attempt-1)` 指数退避，给瞬时网络错误恢复窗口。
pub const QUOTA_FETCH_RETRY_DELAY_MS: u64 = 500;

// ============================================================
// Token 生命周期
// ============================================================

/// Token 距离过期还有多少毫秒内视为「需要预刷新」。
///
/// Claude `activate` 路径会在此窗口内尝试 best-effort 刷新；daemon 后台保活也用同一窗口。
pub const REFRESH_SLACK_MS: i64 = 5 * 60 * 1000;

// ============================================================
// daemon 周期
// ============================================================

/// daemon 轮询周期（毫秒）。M4 使用。活跃时（最近有客户端在跑）的频率。
pub const DAEMON_POLL_INTERVAL_MS: u64 = 60 * 1000;

/// daemon 「空闲」判定阈值（毫秒）。
///
/// provider 的 `client_targets().probe_path` mtime 距今超过此值 → 视为用户没在用 AI，
/// daemon 退到 [`DAEMON_IDLE_POLL_INTERVAL_MS`] 节奏，减少无意义的 quota 请求。
pub const DAEMON_IDLE_THRESHOLD_MS: i64 = 30 * 60 * 1000;

/// daemon 空闲时的轮询周期（毫秒）。
///
/// 由 [`DAEMON_IDLE_THRESHOLD_MS`] 判定空闲后启用；一旦 probe 文件再次变动，
/// 下一轮立刻回到 [`DAEMON_POLL_INTERVAL_MS`]。
pub const DAEMON_IDLE_POLL_INTERVAL_MS: u64 = 15 * 60 * 1000;
