//! 调 Anthropic OAuth/usage 端点。
//!
//! 端点：
//! - Token 刷新：POST https://platform.claude.com/v1/oauth/token
//! - 用量查询：GET https://api.anthropic.com/api/oauth/usage
//!   响应包含 `five_hour.utilization` / `seven_day.utilization`（0~100 百分比）+ `resets_at`。

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use subswap_core::error::{Error, Result};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const BETA_HEADER: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "subswap/0.1";

/// 凭据中无 scope 时的 fallback。取自 Claude Code 的默认 OAuth scope 集。
const DEFAULT_SCOPES: &[&str] = &[
    "user:file_upload",
    "user:inference",
    "user:mcp_servers",
    "user:profile",
    "user:sessions:claude_code",
];

/// 默认 Anthropic OAuth Public Client ID。
/// 这是公开值（非 secret），上游若变更可用环境变量覆盖。
const DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

fn resolved_client_id() -> String {
    std::env::var("SUBSWAP_CLAUDE_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

/// 单个字段「解不出就当没有」的宽容反序列化。
///
/// usage 端点是未公开接口，字段类型会在版本间漂移（例：2026-07 `used_credits`
/// 从整数变成小数，直接把 `Option<u64>` 解崩，整份响应 parse 失败 → 全账号额度查不出）。
/// 用它包住每个字段后，任一字段变形只让该字段退化成 `None`，不再连累其余字段。
fn lenient<'de, D, T>(de: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let raw = serde_json::Value::deserialize(de)?;
    Ok(serde_json::from_value::<Option<T>>(raw).unwrap_or(None))
}

/// usage 端点返回结构。字段全部 optional 且逐字段宽容解析，上游字段调整时不会整体失败。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UsageResponse {
    #[serde(deserialize_with = "lenient")]
    pub five_hour: Option<WindowUsage>,
    #[serde(deserialize_with = "lenient")]
    pub seven_day: Option<WindowUsage>,
    #[serde(deserialize_with = "lenient")]
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WindowUsage {
    /// 百分比，0.0 ~ 100.0。
    #[serde(deserialize_with = "lenient")]
    pub utilization: Option<f64>,
    #[serde(deserialize_with = "lenient")]
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct ExtraUsage {
    #[serde(deserialize_with = "lenient")]
    pub utilization: Option<f64>,
    /// 付费额度上限（暂未在 UI/quota 中展示，M2.5 引入）。上游按币种带小数，故用 f64。
    #[serde(deserialize_with = "lenient")]
    pub monthly_limit: Option<f64>,
    /// 已消费额度（同上）。
    #[serde(deserialize_with = "lenient")]
    pub used_credits: Option<f64>,
    #[serde(deserialize_with = "lenient")]
    pub resets_at: Option<DateTime<Utc>>,
}

/// 查询用量。`access_token` 失效时调用方应先 [`refresh_access_token`]。
pub async fn fetch_usage(access_token: &str) -> Result<UsageResponse> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("build http client: {e}")))?;

    let resp = client
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .header("anthropic-beta", BETA_HEADER)
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("request usage endpoint: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::QuotaFetch(format!(
            "usage returned {status}: {body}"
        )));
    }

    resp.json::<UsageResponse>()
        .await
        .map_err(|e| Error::QuotaFetch(format!("parse usage response: {e}")))
}

#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    pub access_token: String,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
}

/// 刷新 access_token。client_id 默认走 [`DEFAULT_CLIENT_ID`]，
/// 可通过 `SUBSWAP_CLAUDE_OAUTH_CLIENT_ID` 环境变量覆写。
/// 空白 `scopes` 时使用 [`DEFAULT_SCOPES`] 作为 fallback——store 中旧凭据可能没有 scope 记录。
pub async fn refresh_access_token(
    refresh_token: &str,
    scopes: &[String],
) -> Result<RefreshResponse> {
    let client_id = resolved_client_id();
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("build http client: {e}")))?;

    let scope_str = if scopes.is_empty() {
        DEFAULT_SCOPES.join(" ")
    } else {
        scopes.join(" ")
    };
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": client_id,
        "scope": scope_str,
    });
    let resp = client
        .post(REFRESH_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("request refresh endpoint: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::QuotaFetch(format!(
            "refresh returned {status}: {body}"
        )));
    }

    resp.json::<RefreshResponse>()
        .await
        .map_err(|e| Error::QuotaFetch(format!("parse refresh response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07 线上真实响应片段：`used_credits` 是小数、多出一批未知窗口字段。
    /// 旧的 `Option<u64>` 会在这里解崩，导致整份响应 parse 失败、额度永远查不出。
    #[test]
    fn parses_2026_07_usage_payload_with_decimal_credits() {
        let raw = r#"{
            "five_hour": {"utilization": 7.0, "resets_at": "2026-07-25T22:50:00.396399+00:00",
                          "limit_dollars": null, "used_dollars": null, "remaining_dollars": null},
            "seven_day": {"utilization": 12.0, "resets_at": "2026-07-31T16:00:00.396423+00:00"},
            "seven_day_opus": null,
            "seven_day_omelette": null,
            "tangelo": null,
            "extra_usage": {"is_enabled": true, "monthly_limit": null, "used_credits": 0.0,
                            "utilization": null, "currency": "USD", "decimal_places": 2,
                            "daily": null, "weekly": null},
            "limits": [{"kind": "session", "percent": 7}],
            "spend": {"used": {"amount_minor": 0}},
            "member_dashboard_available": false
        }"#;
        let parsed: UsageResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.five_hour.unwrap().utilization, Some(7.0));
        assert_eq!(parsed.seven_day.unwrap().utilization, Some(12.0));
        assert_eq!(parsed.extra_usage.unwrap().used_credits, Some(0.0));
    }

    /// 单个字段类型漂移只让该字段退化成 None，不能连累同级其他字段。
    #[test]
    fn field_type_drift_degrades_only_that_field() {
        let raw = r#"{
            "five_hour": {"utilization": "n/a", "resets_at": "2026-07-25T22:50:00Z"},
            "seven_day": {"utilization": 12.0, "resets_at": 1785031058},
            "extra_usage": 42
        }"#;
        let parsed: UsageResponse = serde_json::from_str(raw).unwrap();
        let five = parsed.five_hour.unwrap();
        assert_eq!(five.utilization, None);
        assert!(five.resets_at.is_some());
        let seven = parsed.seven_day.unwrap();
        assert_eq!(seven.utilization, Some(12.0));
        assert_eq!(seven.resets_at, None);
        assert!(parsed.extra_usage.is_none());
    }

    /// 响应整体缺字段（老版本 / 精简响应）时同样不报错。
    #[test]
    fn missing_fields_are_none() {
        let parsed: UsageResponse = serde_json::from_str("{}").unwrap();
        assert!(parsed.five_hour.is_none());
        assert!(parsed.seven_day.is_none());
        assert!(parsed.extra_usage.is_none());
    }
}
