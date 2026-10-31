//! Antigravity OAuth：复用官方 Antigravity OAuth 应用（公开常量，与 agy / opencode 插件一致）。
//!
//! - Client ID / Secret 是随官方客户端分发的公开值，非用户 secret（见插件源码与 agy 二进制）。
//! - refresh_token 为 Google 长效 token（复用，不轮换），`invalid_grant` 才判死。
//! - 测试用 `SUBSWAP_ANTIGRAVITY_TOKEN_URL` 覆盖 token 端点（mock）。

use subswap_core::error::{Error, Result};

use crate::auth::OauthBlob;

/// 官方 Antigravity OAuth Client ID（公开值）。
pub const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// 官方 Antigravity OAuth Client Secret（随公开客户端分发）。
pub const CLIENT_SECRET: &str = "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";

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn token_url() -> String {
    std::env::var("SUBSWAP_ANTIGRAVITY_TOKEN_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| TOKEN_URL.into())
}

/// 刷新结果。
pub enum RefreshOutcome {
    /// 成功：返回更新后的 blob（access 已换，refresh 复用；若上游轮换则跟进）。
    Refreshed(OauthBlob),
    /// refresh 已死（invalid_grant / 401 / 403），需重登。
    DeadToken(String),
}

/// 用 refresh_token 换新 access_token（parked / active 自愈共用底层）。
pub async fn refresh_blob(blob: &OauthBlob) -> Result<RefreshOutcome> {
    refresh_refresh_token(
        &blob.refresh_token,
        blob.email.clone(),
        blob.project_id.clone(),
        blob.managed_project_id.clone(),
    )
    .await
}

pub async fn refresh_refresh_token(
    refresh_token: &str,
    email: Option<String>,
    project_id: Option<String>,
    managed_project_id: Option<String>,
) -> Result<RefreshOutcome> {
    if refresh_token.trim().is_empty() {
        return Ok(RefreshOutcome::DeadToken("empty refresh token".into()));
    }
    let started = chrono::Utc::now();
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("antigravity token client failed: {e}")))?;
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
    ];
    let resp = client
        .post(token_url())
        .form(&params)
        .header(
            "User-Agent",
            format!("subswap/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("antigravity token request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 400 || status.as_u16() == 401 || status.as_u16() == 403 {
        // Google 用 invalid_grant 表示 refresh 作废；401/403 同判死。
        if body.contains("invalid_grant") || status.as_u16() == 401 || status.as_u16() == 403 {
            return Ok(RefreshOutcome::DeadToken(body));
        }
    }
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!(
            "antigravity token HTTP {status}: {body}"
        )));
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| Error::QuotaFetch(format!("antigravity token bad response: {e}: {body}")))?;
    let access = v
        .get("access_token")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::QuotaFetch(format!("antigravity token missing access_token: {body}"))
        })?;
    let expires_in = v
        .get("expires_in")
        .and_then(|n| n.as_i64().or_else(|| n.as_u64().map(|x| x as i64)))
        .unwrap_or(3600);
    // 上游偶发轮换 refresh（Google 一般复用）；有则跟进，无则保留旧值。
    let new_refresh = v
        .get("refresh_token")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(refresh_token)
        .to_string();
    let expires_at = started + chrono::Duration::seconds(expires_in.max(60));
    Ok(RefreshOutcome::Refreshed(OauthBlob {
        refresh_token: new_refresh,
        access_token: access.to_string(),
        expires_at: Some(expires_at),
        email,
        project_id,
        managed_project_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同文件两单测共用进程级 `SUBSWAP_ANTIGRAVITY_TOKEN_URL`，并行会互串 mock 端点
    /// （一方的请求落到另一方的单次 server，另一方 `accept` 永不到）。串行化。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn mock(body: &'static str, status: &'static str) -> (String, impl Drop) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let _ = s.read(&mut buf);
            write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        struct G(Option<std::thread::JoinHandle<()>>);
        impl Drop for G {
            fn drop(&mut self) {
                if let Some(h) = self.0.take() {
                    let _ = h.join();
                }
            }
        }
        std::env::set_var("SUBSWAP_ANTIGRAVITY_TOKEN_URL", format!("{base}/token"));
        (base, G(Some(handle)))
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn refresh_success_keeps_refresh() {
        let _lock = SERIAL.lock().unwrap();
        let (_b, _g) = mock(
            r#"{"access_token":"ya29.new","expires_in":3599,"token_type":"Bearer"}"#,
            "200 OK",
        );
        let blob = OauthBlob {
            refresh_token: "1//old".into(),
            access_token: "ya29.old".into(),
            expires_at: None,
            email: Some("a@gmail.com".into()),
            project_id: None,
            managed_project_id: None,
        };
        match refresh_blob(&blob).await.unwrap() {
            RefreshOutcome::Refreshed(n) => {
                assert_eq!(n.access_token, "ya29.new");
                assert_eq!(n.refresh_token, "1//old");
                assert!(n.expires_at.is_some());
            }
            other => panic!(
                "expected refreshed, got dead: {}",
                matches!(other, RefreshOutcome::DeadToken(_))
            ),
        }
        std::env::remove_var("SUBSWAP_ANTIGRAVITY_TOKEN_URL");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn invalid_grant_is_dead() {
        let _lock = SERIAL.lock().unwrap();
        let (_b, _g) = mock(
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            "400 Bad Request",
        );
        let blob = OauthBlob {
            refresh_token: "1//dead".into(),
            access_token: "".into(),
            expires_at: None,
            email: None,
            project_id: None,
            managed_project_id: None,
        };
        match refresh_blob(&blob).await.unwrap() {
            RefreshOutcome::DeadToken(_) => {}
            _ => panic!("expected dead"),
        }
        std::env::remove_var("SUBSWAP_ANTIGRAVITY_TOKEN_URL");
    }
}
