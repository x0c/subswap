//! subswap 统一数据模型。
//!
//! 这里只放语义清晰、Provider 共通的字段；Provider 私有细节放各自实现里。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 账号 ID。Provider 内部唯一，全局组合 `(provider_id, account_id)` 才唯一。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AccountId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// 账号元数据。凭证本身不在这里，由 [`crate::store::CredentialStore`] 持有。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub provider: String,
    pub id: AccountId,
    /// 用户友好标签，例如邮箱前缀或备注。
    pub label: String,
    /// 是否为当前激活账号。
    pub active: bool,
    /// 创建/导入时间。
    pub created_at: DateTime<Utc>,
    /// 上次成功使用时间（切换或调用）。
    pub last_used_at: Option<DateTime<Utc>>,
    /// 用户给的优先级（数字越小越优先）；自动切换时作为 tie-breaker。
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// 任意 Provider 私有 KV，用于扩展（不入 keyring）。
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_priority() -> i32 {
    100
}

/// 额度统计窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindow {
    /// Claude 的 5 小时窗口。
    FiveHour,
    /// Claude 的 7 天窗口。
    SevenDay,
    /// 月度窗口（Codex 等）。
    Month,
    /// 其他自定义窗口。
    Custom,
}

/// 额度状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    /// 健康可用。
    Ok,
    /// 接近阈值（默认 used >= 99%）。
    Warn,
    /// 已耗尽或被限流。
    Exhausted,
    /// 查询失败 / 未知。
    Unknown,
}

/// 单个窗口的额度快照。一个账号可能同时存在多个窗口（如 Claude 同时给 5h 与 7d）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quota {
    pub provider: String,
    pub account_id: AccountId,
    pub window: QuotaWindow,
    /// 已使用量（单位由 Provider 自行约定，通常是 tokens 或百分比基数）。
    pub used: u64,
    /// 上限。0 表示未知。
    pub limit: u64,
    /// 重置时间（若 Provider 提供）。
    pub reset_at: Option<DateTime<Utc>>,
    pub status: QuotaStatus,
    /// 人类可读补充说明（错误信息、提示等）。
    #[serde(default)]
    pub note: Option<String>,
}

impl Quota {
    /// 使用率 0.0~1.0；limit=0 时返回 None。
    pub fn usage_ratio(&self) -> Option<f64> {
        if self.limit == 0 {
            None
        } else {
            Some(self.used as f64 / self.limit as f64)
        }
    }

    /// 是否达到给定阈值（0.0~1.0）。limit 未知时返回 false（保守不触发自动切换）。
    pub fn is_above(&self, threshold: f64) -> bool {
        self.usage_ratio().map(|r| r >= threshold).unwrap_or(false)
    }
}

/// 一次切换可能要触达的本地客户端目标（CLI、IDE 扩展、桌面端等）。
/// Provider 在 `client_targets()` 中声明，切换时由统一的 FileSyncer 处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTarget {
    /// 客户端标识，例如 `codex_cli` / `codex_vscode` / `claude_cli`。
    pub id: String,
    /// 人类可读名称，用于 doctor / 日志输出。
    pub display_name: String,
    /// 该客户端的根目录或主配置文件，doctor 用来探测是否存在。
    pub probe_path: std::path::PathBuf,
}
