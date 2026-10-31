//! Antigravity 本地路径：官方固定 `~/.gemini/antigravity-cli`，全局配置 `~/.gemini/config`。

use std::path::PathBuf;

/// 解析 Antigravity CLI 家目录：`SUBSWAP_ANTIGRAVITY_HOME` > `~/.gemini/antigravity-cli`。
pub fn antigravity_home() -> PathBuf {
    if let Ok(v) = std::env::var("SUBSWAP_ANTIGRAVITY_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir().join(".gemini").join("antigravity-cli")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_respects_override() {
        let old = std::env::var("SUBSWAP_ANTIGRAVITY_HOME").ok();
        std::env::set_var("SUBSWAP_ANTIGRAVITY_HOME", "/tmp/agy-test");
        assert_eq!(antigravity_home(), PathBuf::from("/tmp/agy-test"));
        match old {
            Some(v) => std::env::set_var("SUBSWAP_ANTIGRAVITY_HOME", v),
            None => std::env::remove_var("SUBSWAP_ANTIGRAVITY_HOME"),
        }
    }
}
