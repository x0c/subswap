//! 运行时可热加载的数值调优配置。
//!
//! 设计：
//! - **唯一 source of truth**：`<config_dir>/config.toml`（缺失则用 [`crate::defaults`] 编译期默认值）。
//! - **热更新**：daemon 每轮循环开头 [`reload_from_file`]；CLI 启动时调一次即可（CLI 短命）。
//! - **失败兜底**：文件不存在 → 用默认值；文件存在但解析失败 → 沿用上一次成功加载的值并打 warn，
//!   防止配置 typo 把 daemon 拖挂。
//! - **访问入口**：[`current`] 返回 `Arc<Settings>`，所有调用点都从这里读，避免在长生命周期对象里
//!   缓存可能过期的副本。
//!
//! 字段命名遵循「按 domain 分组」的 TOML 表，方便 `vim ~/.config/subswap/config.toml`。
//! 例：
//! ```toml
//! [auto_swap]
//! # threshold = <0.0~1.0>
//!
//! [daemon]
//! poll_interval_ms = 60000
//! idle_threshold_ms = 1800000
//! idle_poll_interval_ms = 900000
//!
//! [quota]
//! fetch_retries = 5
//! ```

use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::defaults;
use crate::error::{Error, Result};
use crate::paths::AppPaths;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub auto_swap: AutoSwap,
    pub quota: Quota,
    pub token: Token,
    pub daemon: Daemon,
    pub codex: Codex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoSwap {
    /// 自动切换总开关。false 时 daemon 和默认入口均跳过所有自动切换决策。
    pub enabled: bool,
    /// 自动切换触发阈值，0.0~1.0。任一窗口 used/limit ≥ 此值即触发。
    pub threshold: f64,
    /// 切换后冷却期（毫秒）：刚被切走/切到的账号短期不再选回，避免抖动。
    pub cooldown_ms: i64,
    /// 新激活账号沉淀宽限期（毫秒）：刚 active 的账号在此窗口内不因 quota
    /// loading / 拉取失败被自动切走，避免顶掉用户刚做的手动选择。
    pub settle_grace_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quota {
    /// 展示用 Warn 阈值（百分比 0~100）。不参与切换决策。
    pub warn_pct: f64,
    /// 展示用 Exhausted 阈值（百分比 0~100）。不参与切换决策。
    pub exhausted_pct: f64,
    /// 单次 quota 查询 attempt 的超时（毫秒）。
    pub fetch_timeout_ms: u64,
    /// quota 查询失败后额外重试几次；最多 5 次，401/403 不重试。
    pub fetch_retries: u32,
    /// 首次重试前等待多久（毫秒）；后续指数退避。
    pub fetch_retry_delay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Token {
    /// token 距过期还有多少毫秒视为「需要预刷新」。
    pub refresh_slack_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Daemon {
    /// 活跃时（最近 `idle_threshold_ms` 内 probe 文件被改过）的轮询周期。
    pub poll_interval_ms: u64,
    /// 多久无 probe 文件 mtime 变化判定为空闲。
    pub idle_threshold_ms: i64,
    /// 空闲时的轮询周期。
    pub idle_poll_interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Codex {
    /// 旧版 Codex 本地 last_usage 缓存允许使用的最大年龄。仅作字段漂移兜底。
    pub usage_cache_max_age_ms: i64,
}

impl Default for AutoSwap {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: defaults::AUTO_SWAP_THRESHOLD,
            cooldown_ms: defaults::AUTO_SWAP_COOLDOWN_MS,
            settle_grace_ms: defaults::AUTO_SWAP_SETTLE_GRACE_MS,
        }
    }
}

impl Default for Quota {
    fn default() -> Self {
        Self {
            warn_pct: defaults::QUOTA_WARN_PCT,
            exhausted_pct: defaults::QUOTA_EXHAUSTED_PCT,
            fetch_timeout_ms: defaults::QUOTA_FETCH_TIMEOUT_MS,
            fetch_retries: defaults::QUOTA_FETCH_RETRIES,
            fetch_retry_delay_ms: defaults::QUOTA_FETCH_RETRY_DELAY_MS,
        }
    }
}

impl Default for Token {
    fn default() -> Self {
        Self {
            refresh_slack_ms: defaults::REFRESH_SLACK_MS,
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self {
            poll_interval_ms: defaults::DAEMON_POLL_INTERVAL_MS,
            idle_threshold_ms: defaults::DAEMON_IDLE_THRESHOLD_MS,
            idle_poll_interval_ms: defaults::DAEMON_IDLE_POLL_INTERVAL_MS,
        }
    }
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            usage_cache_max_age_ms: defaults::CODEX_USAGE_CACHE_MAX_AGE_MS,
        }
    }
}

// ----- 全局当前值 ------------------------------------------------------------

static CURRENT: OnceLock<RwLock<Arc<Settings>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<Settings>> {
    CURRENT.get_or_init(|| RwLock::new(Arc::new(Settings::default())))
}

/// 读取当前生效的配置快照。
///
/// 各模块**每次访问**都调一次，不要把返回值长期缓存——daemon 会在循环开头 [`reload_from_file`]，
/// 长期缓存会拿到过期值。
pub fn current() -> Arc<Settings> {
    cell().read().expect("settings rwlock poisoned").clone()
}

/// 重新从默认路径 `<config_dir>/config.toml` 读取配置。
///
/// 行为：
/// - 文件不存在 → 写入 [`Settings::default`] 并返回。
/// - 解析成功 → 替换全局当前值并返回新快照。
/// - 解析失败 → 保留旧值，返回 `Err`（调用方一般 warn 一行就继续）。
pub fn reload_from_file() -> Result<Arc<Settings>> {
    let path = AppPaths::resolve()?.config_file();
    let parsed = load_from(&path)?;
    install(parsed.clone());
    Ok(parsed)
}

/// 解析指定路径的配置；文件不存在视为默认值。**不**修改全局状态。
pub fn load_from(path: &Path) -> Result<Arc<Settings>> {
    if !path.exists() {
        return Ok(Arc::new(Settings::default()));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?;
    if raw.trim().is_empty() {
        return Ok(Arc::new(Settings::default()));
    }
    let parsed: Settings = toml::from_str(&raw)
        .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
    Ok(Arc::new(parsed))
}

/// 直接替换全局当前值。测试与 CLI doctor 使用。
pub fn install(settings: Arc<Settings>) {
    *cell().write().expect("settings rwlock poisoned") = settings;
}

/// 将 `[auto_swap] enabled` 写入 `<config_dir>/config.toml`，其余字段保持不变。
///
/// 文件不存在时创建；存在时只修改目标键，不覆盖其他用户配置。
pub fn set_auto_swap_enabled(enabled: bool) -> Result<()> {
    let path = AppPaths::resolve()?.config_file();
    let mut doc: toml_edit::DocumentMut = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?;
        raw.parse::<toml_edit::DocumentMut>()
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?
    } else {
        toml_edit::DocumentMut::new()
    };

    doc["auto_swap"]["enabled"] = toml_edit::value(enabled);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Config(format!("create dir {}: {e}", parent.display())))?;
    }
    std::fs::write(&path, doc.to_string())
        .map_err(|e| Error::Config(format!("write {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_compile_time_constants() {
        let s = Settings::default();
        assert_eq!(s.auto_swap.threshold, defaults::AUTO_SWAP_THRESHOLD);
        assert_eq!(
            s.auto_swap.settle_grace_ms,
            defaults::AUTO_SWAP_SETTLE_GRACE_MS
        );
        assert_eq!(s.daemon.poll_interval_ms, defaults::DAEMON_POLL_INTERVAL_MS);
        assert_eq!(
            s.daemon.idle_threshold_ms,
            defaults::DAEMON_IDLE_THRESHOLD_MS
        );
        assert_eq!(s.quota.fetch_retries, defaults::QUOTA_FETCH_RETRIES);
        assert_eq!(
            s.quota.fetch_retry_delay_ms,
            defaults::QUOTA_FETCH_RETRY_DELAY_MS
        );
    }

    #[test]
    fn missing_file_yields_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let s = load_from(&path).unwrap();
        assert_eq!(*s, Settings::default());
    }

    #[test]
    fn partial_file_keeps_section_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[auto_swap]
threshold = 0.5
"#,
        )
        .unwrap();
        let s = load_from(&path).unwrap();
        assert_eq!(s.auto_swap.threshold, 0.5);
        // 其他字段保持默认
        assert_eq!(s.auto_swap.cooldown_ms, defaults::AUTO_SWAP_COOLDOWN_MS);
        assert_eq!(s.daemon.poll_interval_ms, defaults::DAEMON_POLL_INTERVAL_MS);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[auto_swap]
threshold = 0.5
foo = 1
"#,
        )
        .unwrap();
        let err = load_from(&path).unwrap_err().to_string();
        assert!(err.contains("foo"), "{err}");
    }
}
