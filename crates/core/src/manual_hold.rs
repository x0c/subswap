//! 手动切换保持：用户手动 `swap` / `login` 某 provider 后，该 provider 暂停自动切换一段时间。
//!
//! - **写**：手动切换入口（`subswap swap <目标>` / 各 provider `login`）成功后，
//!   调用 [`record_manual_swap`] 写入 `<state_dir>/manual_hold/<provider>.json`。
//! - **读**：自动切换决策前（CLI 默认入口 / daemon），调用 [`hold_remaining_ms`]；
//!   仍在保持期内 → 整个 provider 本轮返回 `NoOp`（连确定性额度切换一起挡）。
//! - **落盘而非内存**：CLI 是短命进程、daemon 可能重启，内存态记不住；文件两者都认。
//! - **只认手动**：自动切换成功不写（否则自动切完会把自己锁住）。
//! - 损坏/缺失/过期一律视为无保持（fail-open：自动切换照常）。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths::AppPaths;

/// 手动切换后写入保持标记。失败只返回 `Err`（调用方 warn 一行继续，不挡切换本身）。
pub fn record_manual_swap(provider: &str) -> crate::error::Result<()> {
    record_manual_swap_with_hold(
        provider,
        crate::settings::current().auto_swap.manual_hold_ms,
    )
}

/// `record_manual_swap` 的显式时长版本：调用方（尤其测试）可直接指定保持时长，
/// 不依赖全局 settings。`hold_ms <= 0` 时不写文件。
pub fn record_manual_swap_with_hold(provider: &str, hold_ms: i64) -> crate::error::Result<()> {
    let paths = AppPaths::resolve()?;
    if hold_ms <= 0 {
        return Ok(());
    }
    let until_ms = now_ms().saturating_add(hold_ms.max(0) as u64);
    let path = paths.manual_hold_file(provider);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({ "until_ms": until_ms });
    std::fs::write(&path, serde_json::to_string(&body)?)?;
    Ok(())
}

/// 保持期剩余毫秒；0 表示无保持（缺失 / 损坏 / 过期 / 配置关闭）。
pub fn hold_remaining_ms(provider: &str) -> i64 {
    let hold_ms = crate::settings::current().auto_swap.manual_hold_ms;
    if hold_ms <= 0 {
        return 0;
    }
    let path = match AppPaths::resolve() {
        Ok(p) => p.manual_hold_file(provider),
        Err(_) => return 0,
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let until_ms: u64 = match serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("until_ms")?.as_u64())
    {
        Some(v) => v,
        None => return 0,
    };
    until_ms.saturating_sub(now_ms()) as i64
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SUBSWAP_HOME` 是进程级环境变量，并行测试会互相干扰 → 与 auto_policy 共用同一把锁。
    fn env_lock() -> &'static std::sync::Mutex<()> {
        crate::auto_policy::hold_test_lock()
    }

    fn with_home(tmp: &tempfile::TempDir) -> String {
        let home = tmp.path().join("subswap");
        std::env::set_var("SUBSWAP_HOME", &home);
        home.to_string_lossy().into_owned()
    }

    #[test]
    fn missing_file_means_no_hold() {
        let _guard = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("SUBSWAP_HOME");
        with_home(&tmp);
        // SUBSWAP_HOME 指向空目录：文件必然缺失 → 无保持。
        assert_eq!(hold_remaining_ms("codex-no-such"), 0);
        match prev {
            Some(v) => std::env::set_var("SUBSWAP_HOME", v),
            None => std::env::remove_var("SUBSWAP_HOME"),
        }
    }

    #[test]
    fn record_then_hold_is_positive_and_expired_file_is_zero() {
        let _guard = env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("SUBSWAP_HOME");
        with_home(&tmp);
        record_manual_swap_with_hold("codex", 600_000).unwrap();
        assert!(hold_remaining_ms("codex") > 0);
        // 过期文件视为无保持。
        let path = crate::paths::AppPaths::resolve()
            .unwrap()
            .manual_hold_file("codex");
        std::fs::write(&path, r#"{"until_ms": 1}"#).unwrap();
        assert_eq!(hold_remaining_ms("codex"), 0);
        // 损坏文件同样 fail-open。
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(hold_remaining_ms("codex"), 0);
        match prev {
            Some(v) => std::env::set_var("SUBSWAP_HOME", v),
            None => std::env::remove_var("SUBSWAP_HOME"),
        }
    }
}
