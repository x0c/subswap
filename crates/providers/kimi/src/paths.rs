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
