//! OpenCode 本地数据目录。官方客户端用 xdg-basedir：macOS 也是 `~/.local/share/opencode`，
//! 不是 `~/Library/Application Support`。

use std::path::{Path, PathBuf};

/// 解析 OpenCode 数据目录：`SUBSWAP_OPENCODE_HOME` > `XDG_DATA_HOME/opencode` > 平台默认。
pub fn opencode_home() -> PathBuf {
    if let Ok(v) = std::env::var("SUBSWAP_OPENCODE_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    data_home().join("opencode")
}

/// 当前激活的凭证文件：`<home>/auth.json`。
pub fn auth_json_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

fn data_home() -> PathBuf {
    if let Ok(v) = std::env::var("XDG_DATA_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(v) = std::env::var("LOCALAPPDATA") {
            if !v.trim().is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    home_dir().join(".local").join("share")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
