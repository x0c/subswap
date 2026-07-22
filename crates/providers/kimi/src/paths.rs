//! Kimi 本地凭证路径解析。

use std::path::{Path, PathBuf};

/// 解析 Kimi 工作目录：`KIMI_CODE_HOME` > `~/.kimi-code` > `.kimi-code`。
pub fn kimi_home() -> PathBuf {
    if let Ok(v) = std::env::var("KIMI_CODE_HOME") {
        return PathBuf::from(v);
    }
    if let Some(d) = directories::UserDirs::new() {
        return d.home_dir().join(".kimi-code");
    }
    PathBuf::from(".kimi-code")
}

/// 当前激活凭证文件：`<home>/credentials/kimi-code.json`。
pub fn active_cred_path(home: &Path) -> PathBuf {
    home.join("credentials").join("kimi-code.json")
}

/// 官方 Kimi 1.31+ 用于协调多进程令牌刷新的锁文件。
pub fn credentials_lock_path(home: &Path) -> PathBuf {
    home.join("credentials").join("kimi-code.lock")
}

/// 新版 TypeScript Kimi 交给 proper-lockfile 的目标占位文件。
pub fn oauth_lock_sentinel_path(home: &Path) -> PathBuf {
    home.join("oauth").join("kimi-code")
}

/// 新版 TypeScript Kimi 的 proper-lockfile 实际互斥目录。
pub fn oauth_lock_dir_path(home: &Path) -> PathBuf {
    home.join("oauth").join("kimi-code.lock")
}

/// subswap 对已被上游拒绝的 refresh token 保存 SHA-256 指纹，避免后台反复请求。
pub fn dead_refresh_fingerprint_path(home: &Path) -> PathBuf {
    home.join("credentials").join(".subswap-dead-refresh")
}
