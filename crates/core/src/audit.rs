//! 审计日志：append-only JSON Lines，记录所有改变激活态或凭证的操作。
//!
//! 目的：
//! - 出问题时能复盘「什么时候、谁、切到了哪个账号」
//! - daemon 自动切换抖动时能反查
//!
//! 设计：
//! - 每条事件一行 JSON（JSON Lines / NDJSON），方便 `tail -f` 与 `jq` 处理
//! - 写入失败只 tracing::warn，绝不阻塞主流程

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: DateTime<Utc>,
    /// 操作类型，例如 "activate" / "refresh" / "auto_swap" / "rm" / "add"。
    pub action: String,
    pub provider: String,
    /// 目标账号 id；删除/创建场景可空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// "ok" / "error"。
    pub result: String,
    /// 可选附加信息：错误消息、上一个 active 账号等。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AuditEvent {
    pub fn ok(action: &str, provider: &str, account: Option<&str>) -> Self {
        Self {
            timestamp: Utc::now(),
            action: action.into(),
            provider: provider.into(),
            account: account.map(str::to_string),
            result: "ok".into(),
            detail: None,
        }
    }

    pub fn err(action: &str, provider: &str, account: Option<&str>, detail: &str) -> Self {
        Self {
            timestamp: Utc::now(),
            action: action.into(),
            provider: provider.into(),
            account: account.map(str::to_string),
            result: "error".into(),
            detail: Some(detail.into()),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn from_default_paths() -> Result<Self> {
        Ok(Self {
            path: AppPaths::resolve()?.audit_log(),
        })
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// 追加一条事件；失败只 warn，不抛错。
    pub fn append(&self, event: AuditEvent) {
        if let Err(e) = self.try_append(&event) {
            tracing::warn!(err=%e, path=%self.path.display(), "audit log write failed");
        }
    }

    fn try_append(&self, event: &AuditEvent) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(event)? + "\n";
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_file_and_writes_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::new(tmp.path().join("audit.log"));
        log.append(AuditEvent::ok("activate", "claude", Some("alice")));
        log.append(AuditEvent::err(
            "refresh",
            "claude",
            Some("bob"),
            "refresh endpoint 400",
        ));
        let content = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let e0: AuditEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(e0.action, "activate");
        assert_eq!(e0.result, "ok");
        let e1: AuditEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(e1.result, "error");
        assert!(e1.detail.is_some());
    }
}
