//! OpenCode Go 额度：GET `{base}/usage`，Bearer API key。
//! 上游 `percent` 是已用百分比（0~100），与 subswap `Quota.used` 语义一致。

use chrono::{DateTime, Utc};
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

const DEFAULT_BASE: &str = "https://opencode.ai/zen/go/v1";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn base_url() -> String {
    std::env::var("SUBSWAP_OPENCODE_GO_BASE")
        .unwrap_or_else(|_| DEFAULT_BASE.into())
        .trim_end_matches('/')
        .to_string()
}

fn to_f64(v: Option<&serde_json::Value>) -> Option<f64> {
    let v = v?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return Some(n as f64);
    }
    v.as_str()?.parse().ok()
}

fn reset_at(window: &serde_json::Value) -> Option<DateTime<Utc>> {
    let s = window.get("resetsAt")?.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn quota_from_window(
    window: &serde_json::Value,
    kind: QuotaWindow,
    provider: &str,
    id: &AccountId,
) -> Option<Quota> {
    let status = window.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let pct = match status {
        "rate-limited" => 100.0,
        _ => to_f64(window.get("percent"))?,
    };
    if !pct.is_finite() {
        return None;
    }
    let used = pct.round().clamp(0.0, 100.0) as u64;
    Some(Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window: kind,
        used,
        limit: 100,
        reset_at: reset_at(window),
        status: QuotaStatus::from_percent(pct),
        note: None,
    })
}

/// 解析 `/usage` 响应为 rolling / weekly / monthly 三个窗口。
pub fn parse_usage(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let usage = v.get("usage").unwrap_or(&v);
    let mut out = Vec::new();
    if let Some(w) = usage.get("rolling") {
        if let Some(q) = quota_from_window(w, QuotaWindow::FiveHour, provider, id) {
            out.push(q);
        }
    }
    if let Some(w) = usage.get("weekly") {
        if let Some(q) = quota_from_window(w, QuotaWindow::SevenDay, provider, id) {
            out.push(q);
        }
    }
    if let Some(w) = usage.get("monthly") {
        if let Some(q) = quota_from_window(w, QuotaWindow::Month, provider, id) {
            out.push(q);
        }
    }
    out
}

/// 用 Go API key 查额度。
pub async fn fetch_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>> {
    fetch_quota_at(access_token, account, &base_url()).await
}

async fn fetch_quota_at(
    access_token: &str,
    account: &Account,
    api_base: &str,
) -> Result<Vec<Quota>> {
    let url = format!("{}/usage", api_base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("opencode go usage client failed: {e}")))?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header(
            "User-Agent",
            format!("subswap/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("opencode go usage request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(Error::QuotaFetch(format!(
            "opencode go usage HTTP {status}: needs re-login: {body}"
        )));
    }
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!(
            "opencode go usage HTTP {status}: {body}"
        )));
    }
    Ok(parse_usage(&body, crate::PROVIDER_ID, &account.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "usage": {
        "rolling": { "status": "ok", "percent": 4,  "resetsAt": "2026-08-13T16:27:38.287Z" },
        "weekly":  { "status": "ok", "percent": 3,  "resetsAt": "2026-08-17T00:00:00.287Z" },
        "monthly": { "status": "ok", "percent": 1,  "resetsAt": "2026-09-13T06:06:01.287Z" }
      }
    }"#;

    #[test]
    fn parses_three_windows() {
        let q = parse_usage(SAMPLE, "opencode", &AccountId("go-abc".into()));
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].window, QuotaWindow::FiveHour);
        assert_eq!((q[0].used, q[0].limit), (4, 100));
        assert_eq!(q[1].window, QuotaWindow::SevenDay);
        assert_eq!(q[1].used, 3);
        assert_eq!(q[2].window, QuotaWindow::Month);
        assert_eq!(q[2].used, 1);
        assert!(q[0].reset_at.is_some());
    }

    #[test]
    fn rate_limited_is_full() {
        let body = r#"{"usage":{"rolling":{"status":"rate-limited","percent":100}}}"#;
        let q = parse_usage(body, "opencode", &AccountId("x".into()));
        assert_eq!(q[0].used, 100);
        assert_eq!(q[0].status, QuotaStatus::Exhausted);
    }

    #[test]
    fn small_percent_is_not_ratio() {
        let body = r#"{"usage":{"rolling":{"status":"ok","percent":0.97}}}"#;
        let q = parse_usage(body, "opencode", &AccountId("x".into()));
        assert_eq!(q[0].used, 1);
        assert_eq!(q[0].status, QuotaStatus::Ok);
    }

    #[test]
    fn weekly_rate_limited_is_seven_day_exhausted() {
        let body = r#"{"usage":{"weekly":{"status":"rate-limited"}}}"#;
        let q = parse_usage(body, "opencode", &AccountId("x".into()));
        assert_eq!(q[0].window, QuotaWindow::SevenDay);
        assert_eq!(q[0].used, 100);
        assert_eq!(q[0].status, QuotaStatus::Exhausted);
    }

    fn sample_account() -> Account {
        Account {
            provider: "opencode".into(),
            id: AccountId("go-test".into()),
            label: "sk-…test".into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    struct MockServer {
        base_url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
            use std::io::{Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = std::sync::Arc::clone(&requests);
            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    let request = String::from_utf8_lossy(&buffer[..count]);
                    captured
                        .lock()
                        .unwrap()
                        .push(request.lines().next().unwrap_or_default().to_string());
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            Self {
                base_url,
                requests,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> &str {
            &self.base_url
        }

        fn finish(mut self) -> Vec<String> {
            self.handle.take().unwrap().join().unwrap();
            std::sync::Arc::try_unwrap(self.requests)
                .unwrap()
                .into_inner()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn fetch_maps_three_windows_from_http() {
        let server = MockServer::start(vec![("200 OK", SAMPLE)]);
        let quotas = fetch_quota_at("sk-live", &sample_account(), server.base_url())
            .await
            .unwrap();
        assert_eq!(quotas.len(), 3);
        assert_eq!(quotas[0].window, QuotaWindow::FiveHour);
        assert_eq!(quotas[0].used, 4);
        assert_eq!(quotas[1].window, QuotaWindow::SevenDay);
        assert_eq!(quotas[2].window, QuotaWindow::Month);
        assert_eq!(server.finish(), vec!["GET /usage HTTP/1.1".to_string()]);
    }

    #[tokio::test]
    async fn fetch_401_is_authentication_failure() {
        let server =
            MockServer::start(vec![("401 Unauthorized", r#"{"error":"invalid_api_key"}"#)]);
        let err = fetch_quota_at("sk-dead", &sample_account(), server.base_url())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            subswap_core::is_authentication_failure(&text),
            "401 must block auto-swap candidates: {text}"
        );
        assert!(
            text.contains("needs re-login"),
            "dead Go key should ask for re-login: {text}"
        );
        let _ = server.finish();
    }

    #[tokio::test]
    async fn fetch_429_is_not_authentication_failure() {
        let server = MockServer::start(vec![(
            "429 Too Many Requests",
            r#"{"error":"rate_limited"}"#,
        )]);
        let err = fetch_quota_at("sk-live", &sample_account(), server.base_url())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            !subswap_core::is_authentication_failure(&text),
            "429 must stay a transient fetch failure, not a dead key: {text}"
        );
        assert!(text.contains("429"), "{text}");
        let _ = server.finish();
    }
}
