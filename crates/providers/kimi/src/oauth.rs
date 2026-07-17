//! Kimi OAuth 刷新：POST {oauth_host}/api/oauth/token（form-urlencoded, grant_type=refresh_token）。

use subswap_core::error::{Error, Result};
use subswap_provider_common::{extract_refresh_token, RefreshOutcome};

use crate::kimi_files::decode_jwt_payload;

/// 解析 OAuth host：`KIMI_CODE_OAUTH_HOST` > `https://auth.kimi.com`。
fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .unwrap_or_else(|_| "https://auth.kimi.com".into())
        .trim_end_matches('/')
        .to_string()
}

/// 从 blob 的 access_token JWT 里取 client_id（刷新请求需要）。
fn client_id_from_blob(blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    let token = v.get("access_token")?.as_str()?;
    decode_jwt_payload(token)?
        .get("client_id")?
        .as_str()
        .map(String::from)
}

/// 用 blob 里的 refresh_token 换新令牌，返回轮换后的完整 blob（合并回原 JSON 结构）。
pub async fn refresh_blob(blob: &str) -> Result<RefreshOutcome> {
    let Some(refresh) = extract_refresh_token(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };
    let Some(client_id) = client_id_from_blob(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };

    let url = format!("{}/api/oauth/token", oauth_host());
    let form = [
        ("client_id", client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
    ];

    let resp = reqwest::Client::new()
        .post(&url)
        .header("User-Agent", "subswap")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("kimi refresh request failed: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(RefreshOutcome::DeadToken);
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    if parsed.get("error").and_then(|v| v.as_str()) == Some("invalid_grant") {
        return Ok(RefreshOutcome::DeadToken);
    }
    if !status.is_success() {
        return Err(Error::Provider(format!("kimi refresh HTTP {status}: {body}")));
    }
    let access = parsed.get("access_token").and_then(|v| v.as_str());
    let Some(access) = access else {
        return Err(Error::Provider("kimi refresh response missing access_token".into()));
    };

    // 合并回原 blob 结构，保留未知字段。
    let mut merged: serde_json::Value = serde_json::from_str(blob).unwrap_or(serde_json::json!({}));
    let obj = merged.as_object_mut().unwrap();
    obj.insert("access_token".into(), serde_json::Value::String(access.into()));
    for key in ["refresh_token", "scope", "token_type", "expires_in"] {
        if let Some(v) = parsed.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    if let Some(exp) = parsed.get("expires_in").and_then(|v| v.as_i64()) {
        let now = chrono::Utc::now().timestamp();
        obj.insert("expires_at".into(), serde_json::Value::from(now + exp));
    }
    Ok(RefreshOutcome::Rotated(merged.to_string()))
}
