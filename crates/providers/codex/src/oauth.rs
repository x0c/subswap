//! Codex 停用号令牌刷新：直连 OpenAI OAuth（与社区换号工具同路径）。
//!
//! - `POST https://auth.openai.com/oauth/token`
//! - `client_id` 使用 Codex CLI 公开值 `app_EMoamEEZ73f0CkXaXp7hrann`
//! - 成功后合并回完整 `auth.json` blob，由引擎 `store.set` 写回仓库
//! - 仅 `refresh_token_reused` / `invalid_grant` / 401·403 等终态标 [`RefreshOutcome::DeadToken`]
//!
//! **当前号（active）不走本模块**——仍委托官方 app-server，避免与 live Codex 抢刷。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subswap_core::defaults::REFRESH_SLACK_MS;
use subswap_core::error::{Error, Result};
use subswap_provider_common::{extract_refresh_token, RefreshOutcome};

use crate::codex_files::access_token_needs_refresh;

const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex CLI 公开 OAuth client id（非 secret）；可用环境变量覆盖。
const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USER_AGENT: &str = "subswap/codex-oauth";
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// 上游明确表示 refresh 已废、禁止再打第二枪的错误码。
const TERMINAL_AUTH_CODES: &[&str] = &[
    "refresh_token_reused",
    "invalid_grant",
    "invalid_request",
    "access_denied",
];

fn token_url() -> String {
    std::env::var("SUBSWAP_CODEX_OAUTH_TOKEN_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string())
}

fn client_id() -> String {
    std::env::var("SUBSWAP_CODEX_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

fn dead_refresh_guard() -> &'static Mutex<HashSet<String>> {
    static GUARD: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(HashSet::new()))
}

fn refresh_fingerprint(refresh: &str) -> String {
    Sha256::digest(refresh.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_refresh_dead(refresh: &str) -> bool {
    dead_refresh_guard()
        .lock()
        .map(|set| set.contains(&refresh_fingerprint(refresh)))
        .unwrap_or(false)
}

fn mark_refresh_dead(refresh: &str) {
    if let Ok(mut set) = dead_refresh_guard().lock() {
        set.insert(refresh_fingerprint(refresh));
    }
}

/// 停用号：access 临近过期时直连 OAuth 刷新，返回轮换后的完整 blob。
pub async fn refresh_parked_blob(blob: &str) -> Result<RefreshOutcome> {
    refresh_parked_blob_at(blob, &token_url()).await
}

async fn refresh_parked_blob_at(blob: &str, url: &str) -> Result<RefreshOutcome> {
    let Some(refresh) = extract_refresh_token(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };
    if is_refresh_dead(&refresh) {
        return Ok(RefreshOutcome::DeadToken);
    }

    let now = chrono::Utc::now().timestamp();
    let slack_secs = REFRESH_SLACK_MS / 1000;
    if !access_token_needs_refresh(blob, now, slack_secs) {
        return Ok(RefreshOutcome::Unsupported);
    }

    let client = reqwest::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::Provider(format!("Codex OAuth client failed: {e}")))?;

    let form = [
        ("client_id", client_id()),
        ("grant_type", "refresh_token".into()),
        ("refresh_token", refresh.clone()),
    ];

    let resp = match client.post(url).form(&form).send().await {
        Ok(resp) => resp,
        Err(error) => {
            tracing::debug!(error = %error, "Codex OAuth refresh transport failed");
            return Ok(RefreshOutcome::Unsupported);
        }
    };

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if is_terminal_oauth_failure(status.as_u16(), &body) {
        mark_refresh_dead(&refresh);
        return Ok(RefreshOutcome::DeadToken);
    }
    if !status.is_success() {
        tracing::debug!(%status, body_len = body.len(), "Codex OAuth refresh non-terminal failure");
        return Ok(RefreshOutcome::Unsupported);
    }

    let parsed: OAuthTokenResponse = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(Error::Provider(format!(
                "Codex OAuth refresh response not JSON: {error}"
            )));
        }
    };

    let Some(access) = parsed.access_token.filter(|s| !s.is_empty()) else {
        return Err(Error::Provider(
            "Codex OAuth refresh response missing access_token".into(),
        ));
    };
    let Some(new_refresh) = parsed.refresh_token.filter(|s| !s.is_empty()) else {
        // OpenAI Codex 路径应轮换 refresh；缺 refresh 时不写回半截凭证。
        return Err(Error::Provider(
            "Codex OAuth refresh response missing refresh_token".into(),
        ));
    };

    let merged = apply_refreshed_tokens(blob, &access, &new_refresh, parsed.id_token.as_deref())?;
    Ok(RefreshOutcome::Rotated(merged))
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    #[serde(default)]
    error: Option<OAuthErrorField>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OAuthErrorField {
    Code(String),
    Detail {
        code: Option<String>,
        message: Option<String>,
        #[serde(rename = "type")]
        kind: Option<String>,
    },
}

impl OAuthErrorField {
    fn code_and_message(&self) -> (String, Option<String>) {
        match self {
            Self::Code(code) => (code.clone(), None),
            Self::Detail {
                code,
                message,
                kind,
            } => (
                code.clone()
                    .or_else(|| kind.clone())
                    .unwrap_or_else(|| "unknown_error".into()),
                message.clone(),
            ),
        }
    }
}

/// 是否为 refresh 已废的终态（禁止重试同一 refresh）。
pub(crate) fn is_terminal_oauth_failure(status: u16, body: &str) -> bool {
    if matches!(status, 401 | 403) {
        return true;
    }
    let Ok(parsed) = serde_json::from_str::<OAuthTokenResponse>(body) else {
        let lower = body.to_ascii_lowercase();
        return TERMINAL_AUTH_CODES.iter().any(|code| lower.contains(code));
    };
    if let Some(error) = parsed.error {
        let (code, message) = error.code_and_message();
        let haystack = format!(
            "{} {} {}",
            code,
            message.unwrap_or_default(),
            parsed.error_description.unwrap_or_default()
        )
        .to_ascii_lowercase();
        if TERMINAL_AUTH_CODES
            .iter()
            .any(|needle| haystack.contains(needle))
        {
            return true;
        }
        // OpenAI 偶发把终态包在 400 + refresh_token_reused code 里（非 401）。
        if status == 400
            && (haystack.contains("refresh_token") || haystack.contains("already been used"))
        {
            return true;
        }
    }
    false
}

/// 把新令牌合并进原 `auth.json` 结构，保留未知字段。
pub(crate) fn apply_refreshed_tokens(
    blob: &str,
    access_token: &str,
    refresh_token: &str,
    id_token: Option<&str>,
) -> Result<String> {
    let mut value: Value = serde_json::from_str(blob)
        .map_err(|e| Error::Provider(format!("Codex auth blob is not JSON: {e}")))?;
    let root = value
        .as_object_mut()
        .ok_or_else(|| Error::Provider("Codex auth blob is not a JSON object".into()))?;

    if let Some(tokens) = root.get_mut("tokens").and_then(Value::as_object_mut) {
        tokens.insert("access_token".into(), Value::String(access_token.into()));
        tokens.insert("refresh_token".into(), Value::String(refresh_token.into()));
        if let Some(id) = id_token.filter(|s| !s.is_empty()) {
            tokens.insert("id_token".into(), Value::String(id.into()));
        }
    } else {
        root.insert("access_token".into(), Value::String(access_token.into()));
        root.insert("refresh_token".into(), Value::String(refresh_token.into()));
        if let Some(id) = id_token.filter(|s| !s.is_empty()) {
            root.insert("id_token".into(), Value::String(id.into()));
        }
    }

    root.insert(
        "last_refresh".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    serde_json::to_string_pretty(&value)
        .map_err(|e| Error::Provider(format!("serialize refreshed Codex auth: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn expired_access_jwt() -> String {
        // {"exp":1700000000}
        let payload = "eyJleHAiOjE3MDAwMDAwMDB9";
        format!("hdr.{payload}.sig")
    }

    fn fresh_access_jwt() -> String {
        // {"exp":9999999999}
        let payload = "eyJleHAiOjk5OTk5OTk5OTl9";
        format!("hdr.{payload}.sig")
    }

    fn sample_blob(access: &str, refresh: &str) -> String {
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access,
                "refresh_token": refresh,
                "id_token": "old-id",
                "account_id": "acct_1"
            }
        })
        .to_string()
    }

    #[test]
    fn apply_refreshed_tokens_updates_nested_tokens_and_keeps_account_id() {
        let blob = sample_blob("old-access", "old-refresh");
        let merged =
            apply_refreshed_tokens(&blob, "new-access", "new-refresh", Some("new-id")).unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["tokens"]["access_token"], "new-access");
        assert_eq!(value["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(value["tokens"]["id_token"], "new-id");
        assert_eq!(value["tokens"]["account_id"], "acct_1");
        assert!(value.get("last_refresh").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn terminal_oauth_failure_detects_reused_and_invalid_grant() {
        let body = r#"{"error":{"message":"already used","code":"refresh_token_reused","type":"invalid_request_error"}}"#;
        assert!(is_terminal_oauth_failure(401, body));
        assert!(is_terminal_oauth_failure(
            400,
            r#"{"error":"invalid_grant","error_description":"expired"}"#
        ));
        assert!(!is_terminal_oauth_failure(
            500,
            r#"{"error":"server_error"}"#
        ));
        assert!(is_terminal_oauth_failure(403, "forbidden"));
    }

    #[tokio::test]
    async fn refresh_skips_when_access_still_fresh() {
        let blob = sample_blob(&fresh_access_jwt(), "rt-fresh");
        let outcome = refresh_parked_blob_at(&blob, "http://127.0.0.1:1")
            .await
            .unwrap();
        assert!(matches!(outcome, RefreshOutcome::Unsupported));
    }

    #[tokio::test]
    async fn refresh_rotates_via_oauth_and_merges_blob() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(0u32));
        let hits_bg = Arc::clone(&hits);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            *hits_bg.lock().unwrap() += 1;
            let body = json!({
                "access_token": "rotated-access",
                "refresh_token": "rotated-refresh",
                "id_token": "rotated-id"
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        });

        let blob = sample_blob(&expired_access_jwt(), "rt-1");
        let url = format!("http://{addr}/oauth/token");
        let outcome = refresh_parked_blob_at(&blob, &url).await.unwrap();
        match outcome {
            RefreshOutcome::Rotated(new_blob) => {
                let value: Value = serde_json::from_str(&new_blob).unwrap();
                assert_eq!(value["tokens"]["access_token"], "rotated-access");
                assert_eq!(value["tokens"]["refresh_token"], "rotated-refresh");
                assert_eq!(value["tokens"]["id_token"], "rotated-id");
            }
            _ => panic!("expected Rotated"),
        }
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn refresh_marks_reused_as_dead_and_skips_second_shot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(0u32));
        let hits_bg = Arc::clone(&hits);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            *hits_bg.lock().unwrap() += 1;
            let body = r#"{"error":{"code":"refresh_token_reused","message":"used"}}"#;
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        });

        let refresh = format!("rt-dead-{}", uuid_like());
        let blob = sample_blob(&expired_access_jwt(), &refresh);
        let url = format!("http://{addr}/oauth/token");
        let first = refresh_parked_blob_at(&blob, &url).await.unwrap();
        assert!(matches!(first, RefreshOutcome::DeadToken));
        assert_eq!(*hits.lock().unwrap(), 1);

        // 第二次不应再打网络（守卫命中）。
        let second = refresh_parked_blob_at(&blob, "http://127.0.0.1:1")
            .await
            .unwrap();
        assert!(matches!(second, RefreshOutcome::DeadToken));
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }
}
