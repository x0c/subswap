//! Pi coding agent 家目录解析：官方固定 `~/.pi/agent`（相对 `$HOME`）。
//!
//! 覆盖顺序：`SUBSWAP_PI_HOME`（测试 / 便携）> `PI_CODING_AGENT_DIR`（Pi 原生）
//! > `~/.pi/agent`。`SUBSWAP_*_HOME` 优先让集成测试不用碰真实 Pi 目录。

use std::path::{Path, PathBuf};

/// 解析 Pi agent 目录。
pub fn pi_agent_dir() -> PathBuf {
    if let Ok(v) = std::env::var("SUBSWAP_PI_HOME") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(v) = std::env::var("PI_CODING_AGENT_DIR") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir().join(".pi").join("agent")
}

/// 当前激活凭证：`<agent>/auth.json`（多 slot 映射，slot 名即 Pi provider id）。
pub fn auth_json_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("auth.json")
}

/// 默认选择：`<agent>/settings.json`（只读写 `defaultProvider` / `defaultModel`）。
pub fn settings_json_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("settings.json")
}

/// Pi 模型目录（只读探针用；本 Provider 永不写入）。
pub fn models_json_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
