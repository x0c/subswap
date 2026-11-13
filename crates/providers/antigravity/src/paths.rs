//! Antigravity 本地路径：官方固定 `~/.gemini/antigravity-cli`，全局配置 `~/.gemini/config`。

use std::path::{Path, PathBuf};

/// 解析 Antigravity CLI 家目录：`SUBSWAP_ANTIGRAVITY_HOME` > `~/.gemini/antigravity-cli`。
pub fn antigravity_home() -> PathBuf {
    if let Ok(v) = std::env::var("SUBSWAP_ANTIGRAVITY_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir().join(".gemini").join("antigravity-cli")
}

/// 读取 `<antigravity_home>/cache/default_project_id.txt`；缺失或空白时返回 `None`。
pub fn default_project_id(antigravity_home: &Path) -> Option<String> {
    let cache_file = antigravity_home.join("cache").join("default_project_id.txt");
    if let Ok(raw) = std::fs::read_to_string(cache_file) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// 按 `PATH`、`~/.local/bin/agy`、`/opt/homebrew/bin/agy`、`/usr/local/bin/agy` 顺序探测可执行文件。
pub fn resolve_agy_binary() -> PathBuf {
    let names: &[&str] = if cfg!(windows) {
        &["agy.exe", "agy.cmd", "agy"]
    } else {
        &["agy"]
    };
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    let home_local = home_dir().join(".local").join("bin").join("agy");
    for candidate in [
        home_local.clone(),
        PathBuf::from("/opt/homebrew/bin/agy"),
        PathBuf::from("/usr/local/bin/agy"),
    ] {
        if candidate.is_file() {
            return candidate;
        }
    }
    home_local
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

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn home_respects_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old = std::env::var("SUBSWAP_ANTIGRAVITY_HOME").ok();
        std::env::set_var("SUBSWAP_ANTIGRAVITY_HOME", "/tmp/agy-test");
        assert_eq!(antigravity_home(), PathBuf::from("/tmp/agy-test"));
        match old {
            Some(v) => std::env::set_var("SUBSWAP_ANTIGRAVITY_HOME", v),
            None => std::env::remove_var("SUBSWAP_ANTIGRAVITY_HOME"),
        }
    }

    #[test]
    fn default_project_reads_cache_or_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(default_project_id(tmp.path()), None);

        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("default_project_id.txt"), "custom-proj-123\n").unwrap();
        assert_eq!(
            default_project_id(tmp.path()).as_deref(),
            Some("custom-proj-123")
        );
    }
}
