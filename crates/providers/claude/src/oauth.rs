//! 调 Anthropic OAuth/usage 端点。
//!
//! 端点参考 claude-swap：
//! - Token 刷新：POST https://platform.claude.com/v1/oauth/token
//! - 用量查询：GET https://api.anthropic.com/api/oauth/usage
//!   响应包含 `five_hour.utilization` / `seven_day.utilization`（0~100 百分比）+ `resets_at`。

use chrono::{DateTime, Utc};
use serde::Deserialize;
use subswap_core::error::{Error, Result};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const BETA_HEADER: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "subswap/0.1";

/// 默认 Anthropic OAuth Public Client ID（与 claude-swap 一致）。
/// 这是公开值（非 secret），上游若变更可用环境变量覆盖。
const DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

fn resolved_client_id() -> String {
    std::env::var("SUBSWAP_CLAUDE_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

/// usage 端点返回结构。字段全部 optional，遇到上游字段调整时不会整体失败。
#[derive(Debug, Deserialize)]
pub struct UsageResponse {
    pub five_hour: Option<WindowUsage>,
    pub seven_day: Option<WindowUsage>,
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Deserialize)]
pub struct WindowUsage {
    /// 百分比，0.0 ~ 100.0。
    pub utilization: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ExtraUsage {
    pub utilization: Option<f64>,
    /// 付费额度上限（暂未在 UI/quota 中展示，M2.5 引入）。
    pub monthly_limit: Option<u64>,
    /// 已消费额度（同上）。
    pub used_credits: Option<u64>,
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
pub async fn refresh_access_token(refresh_token: &str) -> Result<RefreshResponse> {
    let client_id = resolved_client_id();
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("build http client: {e}")))?;

    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": client_id,
    });

    let resp = client
        .post(REFRESH_URL)
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
