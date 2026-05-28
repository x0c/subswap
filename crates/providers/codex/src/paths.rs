//! Codex 本地文件路径解析。

use std::path::PathBuf;

/// 解析 Codex 工作目录。
///
/// 优先级：
/// 1. `CODEX_HOME` 环境变量。
/// 2. `~/.codex/`。
/// 3. 当前目录下的 `.codex/`。
pub fn codex_home() -> PathBuf {
    if let Ok(v) = std::env::var("CODEX_HOME") {
        return PathBuf::from(v);
    }
    if let Some(d) = directories::UserDirs::new() {
        return d.home_dir().join(".codex");
    }
    PathBuf::from(".codex")
}

/// 当前激活账号的认证文件：`<codex_home>/auth.json`。
/// Codex CLI / VSCode 扩展 / Codex App 都从这里读取凭证。
pub fn active_auth_path(home: &std::path::Path) -> PathBuf {
    home.join("auth.json")
}
