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

impl Account {
    /// 是否只能由用户手动激活。此类账号不会触发自动切出，也不会成为自动切换候选。
    pub fn manual_only(&self) -> bool {
        self.extra
            .get("manual_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    /// 计费方式。读 `extra["billing"]`，缺省（原生登录的订阅账号）视为 [`BillingKind::Flat`]。
    ///
    /// 这是给下游消费者（如 OpenConductor）判断"按量花钱"的唯一信号；新增 Provider
    /// 适配器（公司号、不限量 APIKEY、第三方中转号池等）只需在 `extra` 里如实标注，
    /// 不需要 subswap-core 认识具体账号名。
    ///
    /// 向后兼容：早于 0.3.23 版本登记的 API 账号没有 `billing` 字段，
    /// 但会带 `kind=api`（见 `subswap add-api` 的历史写入），自动视为 metered。
    pub fn billing(&self) -> BillingKind {
        if let Some(billing) = self
            .extra
            .get("billing")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())
        {
            return billing;
        }
        // 旧账号：kind=api 暗示按量计费端点（自定义 API 节点默认按量）。
        if self.extra.get("kind").and_then(serde_json::Value::as_str) == Some("api") {
            return BillingKind::Metered;
        }
        BillingKind::Flat
    }
}

fn default_priority() -> i32 {
    100
}

/// 账号的计费方式：决定它在自动切换中的优先级与对外的"是否真花钱"语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingKind {
    /// 固定费率订阅（如官方登录号），用量在套餐内不额外计费。
    Flat,
    /// 按量计费（如自定义 API 端点接的按 token 计费上游）。
    Metered,
    /// 不限量（如公司自建网关、不限量 API Key）。
    Unlimited,
}

impl fmt::Display for BillingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Flat => "flat",
            Self::Metered => "metered",
            Self::Unlimited => "unlimited",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for BillingKind {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "flat" => Ok(Self::Flat),
            "metered" => Ok(Self::Metered),
            "unlimited" => Ok(Self::Unlimited),
            _ => Err(()),
        }
    }
}

/// 额度统计窗口。
///
/// 窗口顺序与标签由**真实时长**决定（`duration_seconds`），不依赖枚举名单：
/// 已知 5h / 7d / 月保持稳定 token，其它已知时长的窗口落到 [`QuotaWindow::Timed`]，
/// 用 `<n>d|h|m|s` 形式的 token 表达（例如厂商新增 20 天窗口 → `20d`）。
/// 只有完全拿不到时长时才用 [`QuotaWindow::Custom`]；客户端无法为它排序或标注长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum QuotaWindow {
    /// Claude 的 5 小时窗口。
    FiveHour,
    /// Claude 的 7 天窗口。
    SevenDay,
    /// 月度窗口（Codex 等）。
    Month,
    /// Cursor 官方模型用量。
    FirstPartyModels,
    /// Cursor API 用量。
    Api,
    /// Cursor 套餐 Credits（美元账本；`used`/`limit` 存分）。
    Credits,
    /// Codex 限额重置道具（banked reset）：`used` = 可用数，`limit` = 0（不参与百分比与自动切换判定），
    /// `reset_at` = 最早过期时间。`0` 时不产生该窗口。
    ResetCredits,
    /// 其他自定义窗口（没有时长信息）。
    Custom,
    /// 非标准时长的滚动窗口：厂商新增的窗口长度（秒）。用于排序、标签与 JSON 身份。
    Timed(u64),
}

impl QuotaWindow {
    /// 把上游时长（秒）映射成窗口：标准长度归入已知窗口，其余保留真实时长。
    ///
    /// Provider 拿到窗口长度后必须走这里，禁止直接落 `Custom`——否则客户端失去排序依据。
    pub fn from_seconds(seconds: u64) -> Self {
        match seconds {
            // 上游偶尔有秒级取整误差，标准窗口给一分钟容差。
            s if (17_940..=18_060).contains(&s) => Self::FiveHour,
            s if (604_740..=604_860).contains(&s) => Self::SevenDay,
            // 官方 / 实测月度窗口常见 28–31 天。
            s if (28 * 86_400..=31 * 86_400).contains(&s) => Self::Month,
            s => Self::Timed(s),
        }
    }

    /// 该窗口的时长（秒）；非时间池与未知窗口返回 `None`。
    pub fn duration_seconds(self) -> Option<u64> {
        match self {
            Self::FiveHour => Some(18_000),
            Self::SevenDay => Some(604_800),
            Self::Month => Some(30 * 86_400),
            Self::Timed(seconds) => Some(seconds),
            Self::FirstPartyModels
            | Self::Api
            | Self::Credits
            | Self::ResetCredits
            | Self::Custom => None,
        }
    }

    /// 写入 JSON / 缓存时使用的稳定 token。客户端依赖该 token 的字符串本身携带时长。
    pub fn token(self) -> String {
        match self {
            Self::FiveHour => "five_hour".into(),
            Self::SevenDay => "seven_day".into(),
            Self::Month => "month".into(),
            Self::FirstPartyModels => "first_party_models".into(),
            Self::Api => "api".into(),
            Self::Credits => "credits".into(),
            Self::ResetCredits => "reset_credits".into(),
            Self::Custom => "custom".into(),
            Self::Timed(seconds) => Self::duration_token(seconds),
        }
    }

    /// 时长 token：优先整天 / 整小时 / 整分钟，最小到秒。
    pub fn duration_token(seconds: u64) -> String {
        if seconds % 86_400 == 0 {
            format!("{}d", seconds / 86_400)
        } else if seconds % 3_600 == 0 {
            format!("{}h", seconds / 3_600)
        } else if seconds % 60 == 0 {
            format!("{}m", seconds / 60)
        } else {
            format!("{seconds}s")
        }
    }

    /// 解析 token；未知 token 保守落 `Custom`，保证旧缓存与旧快照仍可读。
    fn from_token(token: &str) -> Self {
        match token {
            "five_hour" => return Self::FiveHour,
            "seven_day" => return Self::SevenDay,
            "month" => return Self::Month,
            "first_party_models" => return Self::FirstPartyModels,
            "api" => return Self::Api,
            "credits" => return Self::Credits,
            "reset_credits" => return Self::ResetCredits,
            "custom" => return Self::Custom,
            _ => {}
        }
        parse_duration_token(token).map_or(Self::Custom, Self::from_seconds)
    }
}

/// 解析 `<n>d` / `<n>h` / `<n>m` / `<n>s` 形态的时长 token。
fn parse_duration_token(token: &str) -> Option<u64> {
    let (digits, unit) = token.split_at(token.char_indices().last()?.0);
    let value: u64 = digits.parse().ok()?;
    let seconds = match unit {
        "d" => value.checked_mul(86_400)?,
        "h" => value.checked_mul(3_600)?,
        "m" => value.checked_mul(60)?,
        "s" => value,
        _ => return None,
    };
    (value > 0).then_some(seconds)
}

impl From<QuotaWindow> for String {
    fn from(window: QuotaWindow) -> Self {
        window.token()
    }
}

impl From<String> for QuotaWindow {
    fn from(value: String) -> Self {
        Self::from_token(&value)
    }
}

/// 额度状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    /// 健康可用。
    Ok,
    /// 接近展示阈值。
    Warn,
    /// 已耗尽或被限流。
    Exhausted,
    /// 查询失败 / 未知。
    Unknown,
}

impl QuotaStatus {
    /// 用统一阈值（[`crate::settings::Quota::warn_pct`] / `exhausted_pct`）
    /// 把已用百分比（0~100）映射为 [`QuotaStatus`]。
    ///
    /// 各 Provider 的 `query_quota` 不要自己写阈值分支，统一走这里，
    /// 调阈值只改 `config.toml` 一处。
    pub fn from_percent(pct: f64) -> Self {
        let q = crate::settings::current().quota.clone();
        if pct >= q.exhausted_pct {
            Self::Exhausted
        } else if pct >= q.warn_pct {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

/// 额度池语义：决定自动切换如何判定“可用”与“恢复”。
/// 由各 Provider 通过 [`crate::Provider::quota_pool_semantics`] 声明，
/// 共享决策逻辑只读该声明，不按 Provider 名分发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPoolSemantics {
    /// 叠加池（默认）：大窗口包含小窗口，任一窗口耗尽整体即不可用；
    /// 恢复取阻塞窗口中最晚者。
    Stacked,
    /// 并行池：任一池有余量即可承接，仅全部耗尽才需切换；
    /// 恢复取最早者。
    Parallel,
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

/// `rm` 断开原生凭证的结果。`Err` 表示原生没断掉，调用方必须直接报错退出、
/// 不清本地记录，避免“删了又回来”的假成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialDisconnect {
    /// 原生凭证已断开/登出，后续同步不会导回。
    Disconnected,
    /// 原生本来就没有这份凭证（parked，或已在别处登出），直接清本地即可（幂等）。
    AlreadyGone,
    /// 原生不支持自动断（如 V1 Console），保持只清本地 + 旧提示。
    Unsupported,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 标准长度归入已知窗口；非标准长度必须保留真实秒数，不能落 Custom。
    #[test]
    fn window_from_seconds_keeps_unknown_lengths() {
        assert_eq!(QuotaWindow::from_seconds(18_000), QuotaWindow::FiveHour);
        assert_eq!(QuotaWindow::from_seconds(18_037), QuotaWindow::FiveHour);
        assert_eq!(QuotaWindow::from_seconds(604_800), QuotaWindow::SevenDay);
        assert_eq!(QuotaWindow::from_seconds(30 * 86_400), QuotaWindow::Month);
        assert_eq!(QuotaWindow::from_seconds(28 * 86_400), QuotaWindow::Month);
        assert_eq!(
            QuotaWindow::from_seconds(20 * 86_400),
            QuotaWindow::Timed(1_728_000)
        );
        assert_eq!(
            QuotaWindow::from_seconds(3 * 3_600),
            QuotaWindow::Timed(10_800)
        );
    }

    /// 时长 token 必须可读、可回读，并在 JSON 里自带长度。
    #[test]
    fn duration_tokens_round_trip_through_json() {
        for (seconds, token) in [
            (20 * 86_400, "20d"),
            (3 * 3_600, "3h"),
            (90 * 60, "90m"),
            (45, "45s"),
        ] {
            let window = QuotaWindow::from_seconds(seconds);
            assert_eq!(window.token(), token);
            let encoded = serde_json::to_string(&window).unwrap();
            assert_eq!(encoded, format!("\"{token}\""));
            assert_eq!(
                serde_json::from_str::<QuotaWindow>(&encoded).unwrap(),
                window
            );
        }
        assert_eq!(QuotaWindow::FiveHour.token(), "five_hour");
        assert_eq!(QuotaWindow::SevenDay.token(), "seven_day");
        assert_eq!(QuotaWindow::Month.token(), "month");
        assert_eq!(QuotaWindow::Credits.token(), "credits");
        // 未知 / 没有时长信息的 token 仍可读，只是失去排序依据。
        assert_eq!(
            serde_json::from_str::<QuotaWindow>("\"custom\"").unwrap(),
            QuotaWindow::Custom
        );
        assert_eq!(
            serde_json::from_str::<QuotaWindow>("\"week\"").unwrap(),
            QuotaWindow::Custom
        );
        assert_eq!(QuotaWindow::Custom.duration_seconds(), None);
        assert_eq!(QuotaWindow::Credits.duration_seconds(), None);
    }

    /// 完整 Quota 序列化保留时长 token，缓存可原样回读。
    #[test]
    fn quota_round_trip_keeps_timed_window() {
        let quota = Quota {
            provider: "codex".into(),
            account_id: AccountId("user@example.com".into()),
            window: QuotaWindow::from_seconds(20 * 86_400),
            used: 10,
            limit: 100,
            reset_at: None,
            status: QuotaStatus::Ok,
            note: None,
        };
        let json = serde_json::to_string(&quota).unwrap();
        assert!(json.contains("\"window\":\"20d\""), "{json}");
        let decoded: Quota = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.window, QuotaWindow::Timed(1_728_000));
    }
}
