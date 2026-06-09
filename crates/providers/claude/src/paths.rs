//! Claude 本地文件路径解析。允许通过环境变量覆写以便测试与非标准安装。

use std::path::{Path, PathBuf};

/// 解析 Claude 工作目录。
///
/// 优先级：
/// 1. `CLAUDE_CONFIG_DIR` 环境变量。
/// 2. `~/.claude/`（标准）。
/// 3. 当前目录下的 `.claude/`（退化路径，仅在无 HOME 时使用）。
pub fn claude_home() -> PathBuf {
    if let Ok(v) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(v);
    }
    if let Some(d) = directories::UserDirs::new() {
        return d.home_dir().join(".claude");
    }
    PathBuf::from(".claude")
}

/// `<claude_home>/.credentials.json`：OAuth 凭证。
pub fn credentials_path(home: &Path) -> PathBuf {
    home.join(".credentials.json")
}

/// `<claude_home>/settings.json`：Claude Code 用户级设置。
pub fn settings_path(home: &Path) -> PathBuf {
    home.join("settings.json")
}

/// `<claude_home>/.subswap-api.json`：subswap 自定义 API 激活状态与恢复信息。
pub fn api_state_path(home: &Path) -> PathBuf {
    home.join(".subswap-api.json")
}

/// 全局配置文件路径解析。
///
/// 兼容三种布局：
/// 1. **旧版**：`<claude_home>/.config.json`（若存在则优先）。
/// 2. **标准位置**（`claude_home == $HOME/.claude`）：`<HOME>/.claude.json`，即 home 同级。
/// 3. **自定义 `CLAUDE_CONFIG_DIR`**：就近放 `<claude_home>/.claude.json`，避免
///    `parent()` 跳到无关上级目录污染（例如 `CLAUDE_CONFIG_DIR=/tmp/foo/claude` 时
///    旧实现会写 `/tmp/foo/.claude.json`）。
pub fn global_config_path(home: &Path) -> PathBuf {
    let legacy = home.join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    if is_standard_claude_home(home) {
        if let Some(parent) = home.parent() {
            return parent.join(".claude.json");
        }
    }
    home.join(".claude.json")
}

/// `home` 是否等于 `$HOME/.claude`（标准位置）。任何环境变量覆写或非标路径都返回 false。
fn is_standard_claude_home(home: &Path) -> bool {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        return false;
    }
    directories::UserDirs::new()
        .map(|d| d.home_dir().join(".claude") == home)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_dir_does_not_escape_to_parent() {
        // 模拟 CLAUDE_CONFIG_DIR 自定义目录场景：不能跳到上级。
        let home = std::path::PathBuf::from("/tmp/some-other/claude-x");
        let path = global_config_path(&home);
        assert!(
            path.starts_with(&home),
            "global config must stay inside home, got: {}",
            path.display()
        );
        assert_eq!(path.file_name().unwrap(), ".claude.json");
    }
}
