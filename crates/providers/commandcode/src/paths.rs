//! Command Code 本地数据目录：官方固定 `~/.commandcode`（相对 `$HOME`）。

use std::path::{Path, PathBuf};

/// 解析 Command Code 家目录：`SUBSWAP_COMMANDCODE_HOME` > `~/.commandcode`。
pub fn commandcode_home() -> PathBuf {
    if let Ok(v) = std::env::var("SUBSWAP_COMMANDCODE_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir().join(".commandcode")
}

/// 当前激活凭证：`<home>/auth.json`。
pub fn auth_json_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
