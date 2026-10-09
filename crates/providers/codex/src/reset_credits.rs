//! Codex 限额重置道具明细：`GET /backend-api/wham/rate-limit-reset-credits`。
//!
//! 只读展示用（数量 + 最早过期），绝不消耗。`available_count == 0` 时调用方不应请求本接口。

use chrono::{DateTime, Utc};
use subswap_core::error::{Error, Result};

const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
// 与 `openai_usage` 同一浏览器风格 UA，避免被识别为非交互客户端。
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 subswap/0.1"
);

/// 一张重置道具的可用明细（仅展示需要的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetCredit {
    pub id: String,
    pub title: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// 拉取重置明细。`status == "available"` 的才返回；全部宽容解析，单条坏了跳过不整份崩。
pub async fn fetch_reset_credits(
    access_token: &str,
    chatgpt_account_id: &str,
) -> Result<Vec<ResetCredit>> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("build http client: {e}")))?;

    let resp = client
        .get(RESET_CREDITS_URL)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", chatgpt_account_id)
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("request rate-limit-reset-credits: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::QuotaFetch(format!(
            "rate-limit-reset-credits returned {status}: {body}"
        )));
    }
    let raw: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::QuotaFetch(format!("rate-limit-reset-credits not JSON: {e}")))?;
    Ok(parse_reset_credits(&raw))
}

/// 从已拿到的 JSON 里抽可用明细（单测与复用入口）。
pub fn parse_reset_credits(raw: &serde_json::Value) -> Vec<ResetCredit> {
    let Some(credits) = raw.get("credits").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    credits
        .iter()
        .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("available"))
        .filter_map(|c| {
            Some(ResetCredit {
                id: c.get("id")?.as_str()?.to_string(),
                title: c
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("reset")
                    .to_string(),
                expires_at: c
                    .get("expires_at")
                    .or_else(|| c.get("expiresAt"))
                    .and_then(|value| {
                        value.as_str().and_then(parse_rfc3339).or_else(|| {
                            value
                                .as_i64()
                                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                        })
                    }),
            })
        })
        .collect()
}

fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_available_credits_with_expiry() {
        // 实测响应形状（2026-09-30，字段有删减）。
        let v = serde_json::json!({
            "credits": [{
                "id": "RateLimitResetCredit_f5903447a8048191b3b8b7b37367feab",
                "reset_type": "codex_rate_limits",
                "status": "available",
                "granted_at": "2026-09-29T18:54:48.219065Z",
                "expires_at": "2026-10-29T18:54:48.219065Z",
                "title": "Full reset (Weekly + 5 hr)",
            }],
            "available_count": 1,
        });
        let credits = parse_reset_credits(&v);
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].title, "Full reset (Weekly + 5 hr)");
        assert_eq!(
            credits[0].expires_at.map(|dt| dt.to_rfc3339()),
            Some("2026-10-29T18:54:48.219065+00:00".into())
        );
    }

    #[test]
    fn skips_redeemed_and_broken_entries() {
        let v = serde_json::json!({
            "credits": [
                {"id": "a", "status": "redeemed", "expires_at": "2026-10-01T00:00:00Z"},
                {"id": "b", "status": "available", "expires_at": "not-a-time"},
                {"status": "available"},
            ]
        });
        let credits = parse_reset_credits(&v);
        // redeemed 跳过；缺 id 跳过；坏时间只丢过期不丢整条。
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].id, "b");
        assert!(credits[0].expires_at.is_none());
    }

    #[test]
    fn empty_without_credits_array() {
        assert!(parse_reset_credits(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn parses_official_camel_case_epoch_expiry_and_nonexpiring_credits() {
        let raw = serde_json::json!({"credits": [
            {"id": "a", "status": "available", "expiresAt": 1800000000, "title": null},
            {"id": "b", "status": "available", "expiresAt": null}
        ]});
        let credits = parse_reset_credits(&raw);
        assert_eq!(credits.len(), 2);
        assert_eq!(credits[0].expires_at.unwrap().timestamp(), 1800000000);
        assert_eq!(credits[0].title, "reset");
        assert!(credits[1].expires_at.is_none());
    }
}
