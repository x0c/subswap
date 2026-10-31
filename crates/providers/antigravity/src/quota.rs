//! Antigravity 额度：`POST {base}/v1internal:retrieveUserQuotaSummary` 主通道，
//! `POST {base}/v1internal:fetchAvailableModels` 兜底。
//!
//! - 主通道返回 2 池 ×（5h + weekly）桶（Gemini Models / Claude and GPT models），
//!   `remainingFraction`（0~1 余量）→ 已用% = `(1-remaining)*100`，`resetTime` RFC3339。
//! - daily 端点偶发只回部分组（缺 Gemini 组，见 Cli-Proxy-API #499）：按
//!   prod → daily → autopush 顺序全试，按 `displayName` 合并，首个非空桶胜出。
//! - proto3 会省略 0 值：有 `resetTime` 且非 `disabled` 的缺字段桶按耗尽（0）计。
//! - 429 ≠ 死号；401/403 → `needs re-login`（`is_authentication_failure` 识别）。

use chrono::{DateTime, Utc};
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

const PROD: &str = "https://cloudcode-pa.googleapis.com";
const DAILY: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const AUTOPUSH: &str = "https://autopush-cloudcode-pa.sandbox.googleapis.com";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn bases() -> Vec<String> {
    if let Ok(v) = std::env::var("SUBSWAP_ANTIGRAVITY_BASE") {
        if !v.trim().is_empty() {
            return vec![v.trim_end_matches('/').to_string()];
        }
    }
    vec![PROD.into(), DAILY.into(), AUTOPUSH.into()]
}

fn antigravity_ua() -> String {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Antigravity/1.18.3 Chrome/138.0.7204.235 Electron/37.3.1 Safari/537.36".into()
}

/// 解析单个桶为 Quota（group 名进 note，便于区分 Gemini / Claude 池）。
pub fn quota_from_bucket(
    bucket: &serde_json::Value,
    group_name: &str,
    provider: &str,
    id: &AccountId,
) -> Option<Quota> {
    if bucket.get("disabled").and_then(|v| v.as_bool()) == Some(true) {
        // weekly 被禁用时整组跳过；5h disabled 仍保留为 0（上游约定的展示语义）。
        let window_raw = window_of(bucket).to_lowercase();
        if window_raw.contains("week") {
            return None;
        }
    }
    let remaining = remaining_fraction(bucket)?;
    let used_pct = ((1.0 - remaining) * 100.0).clamp(0.0, 100.0);
    let used = used_pct.round().clamp(0.0, 100.0) as u64;
    let window = window_from_bucket(bucket);
    let reset_at = bucket
        .get("resetTime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let bucket_label = bucket
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let note = if group_name.is_empty() {
        (!bucket_label.is_empty()).then(|| bucket_label.to_string())
    } else if bucket_label.is_empty() {
        Some(group_name.to_string())
    } else {
        Some(format!("{group_name} · {bucket_label}"))
    };
    Some(Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window,
        used,
        limit: 100,
        reset_at,
        status: QuotaStatus::from_percent(used_pct),
        note,
    })
}

fn window_of(bucket: &serde_json::Value) -> String {
    let w = bucket
        .get("window")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !w.is_empty() {
        return w;
    }
    bucket
        .get("bucketId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn window_from_bucket(bucket: &serde_json::Value) -> QuotaWindow {
    let raw = window_of(bucket).to_lowercase();
    let bid = bucket
        .get("bucketId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let combined = format!("{raw} {bid}");
    if combined.contains("5h") || combined.contains("fivehour") || combined.contains("five_hour") {
        return QuotaWindow::FiveHour;
    }
    if combined.contains("week") {
        return QuotaWindow::SevenDay;
    }
    if combined.contains("month") {
        return QuotaWindow::Month;
    }
    if combined.contains("day") || combined.contains("daily") {
        return QuotaWindow::from_seconds(86_400);
    }
    QuotaWindow::Custom
}

/// 余量 0~1；缺字段时有 resetTime 且非 disabled 按 0（耗尽）计，否则 None（整桶跳过）。
fn remaining_fraction(bucket: &serde_json::Value) -> Option<f64> {
    if let Some(n) = bucket.get("remainingFraction").and_then(|v| v.as_f64()) {
        return Some(n.clamp(0.0, 1.0));
    }
    if let Some(n) = bucket.get("remaining_fraction").and_then(|v| v.as_f64()) {
        return Some(n.clamp(0.0, 1.0));
    }
    // 嵌套 remaining.{remainingFraction,value/case} 三形态（第三方探针兼容）。
    if let Some(rem) = bucket.get("remaining").and_then(|v| v.as_object()) {
        for k in ["remainingFraction", "remaining_fraction"] {
            if let Some(n) = rem.get(k).and_then(|v| v.as_f64()) {
                return Some(n.clamp(0.0, 1.0));
            }
        }
        if rem.get("case").and_then(|v| v.as_str()) == Some("remainingFraction") {
            if let Some(n) = rem.get("value").and_then(|v| v.as_f64()) {
                return Some(n.clamp(0.0, 1.0));
            }
        }
    }
    let has_reset = bucket
        .get("resetTime")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    let disabled = bucket.get("disabled").and_then(|v| v.as_bool()) == Some(true);
    if has_reset && !disabled {
        return Some(0.0);
    }
    None
}

/// 解析 quotaSummary 全身：按组合并桶，每个桶一个 Quota（测试与外部诊断用）。
#[allow(dead_code)]
pub fn parse_summary(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let mut out = Vec::new();
    // 兼容两种形态：{groups:[{displayName,buckets:[]}]} 与扁平 {buckets:[]}。
    if let Some(groups) = v.get("groups").and_then(|g| g.as_array()) {
        for g in groups {
            let name = g
                .get("displayName")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(buckets) = g.get("buckets").and_then(|b| b.as_array()) {
                for b in buckets {
                    if let Some(q) = quota_from_bucket(b, &name, provider, id) {
                        out.push(q);
                    }
                }
            }
        }
    } else if let Some(buckets) = v.get("buckets").and_then(|b| b.as_array()) {
        for b in buckets {
            if let Some(q) = quota_from_bucket(b, "", provider, id) {
                out.push(q);
            }
        }
    }
    out
}

/// 解析 fetchAvailableModels 兜底：按 claude / gemini 取各组最小余量，落 Custom 窗口。
/// 仅 Exhausted 时参与自动切换（阈值只看 FiveHour），避免误触发。
pub fn parse_models_fallback(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let models = v.get("models").and_then(|m| m.as_object());
    let Some(models) = models else {
        return vec![];
    };
    let mut claude_min: Option<(f64, Option<DateTime<Utc>>)> = None;
    let mut gemini_min: Option<(f64, Option<DateTime<Utc>>)> = None;
    for (name, entry) in models {
        let disp = entry
            .get("displayName")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let combined = format!("{name} {disp}").to_lowercase();
        let qi = entry.get("quotaInfo");
        let rem = qi
            .and_then(|q| q.get("remainingFraction"))
            .and_then(|n| n.as_f64())
            .filter(|n| n.is_finite())
            .map(|n| n.clamp(0.0, 1.0));
        let Some(rem) = rem else { continue };
        // chat_* / tab_* 等非模型槽位不计入。
        if name.starts_with("chat_") || name.starts_with("tab_") {
            continue;
        }
        let reset = qi
            .and_then(|q| q.get("resetTime"))
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<DateTime<Utc>>().ok());
        let slot = if combined.contains("claude") {
            &mut claude_min
        } else if combined.contains("gemini") {
            &mut gemini_min
        } else {
            continue;
        };
        match slot {
            Some((cur, cur_reset)) if rem >= *cur => {
                // 保留更早的 reset（恢复更快）。
                if let (Some(nr), Some(cr)) = (reset, *cur_reset) {
                    if nr < cr {
                        *cur_reset = Some(nr);
                    }
                } else if reset.is_some() && cur_reset.is_none() {
                    *cur_reset = reset;
                }
            }
            _ => *slot = Some((rem, reset)),
        }
    }
    let mut out = Vec::new();
    for (label, slot) in [
        ("Antigravity Gemini", gemini_min),
        ("Antigravity Claude", claude_min),
    ] {
        if let Some((rem, reset)) = slot {
            let pct = ((1.0 - rem) * 100.0).clamp(0.0, 100.0);
            out.push(Quota {
                provider: provider.into(),
                account_id: id.clone(),
                window: QuotaWindow::Custom,
                used: pct.round() as u64,
                limit: 100,
                reset_at: reset,
                status: QuotaStatus::from_percent(pct),
                note: Some(label.into()),
            });
        }
    }
    out
}

/// 用 access_token 查额度：先全端点合并 quotaSummary，无组则回落 fetchAvailableModels。
pub async fn fetch_quota(
    access_token: &str,
    project_id: Option<&str>,
    account: &Account,
) -> Result<Vec<Quota>> {
    fetch_quota_at(access_token, project_id, account, &bases()).await
}

async fn fetch_quota_at(
    access_token: &str,
    project_id: Option<&str>,
    account: &Account,
    bases: &[String],
) -> Result<Vec<Quota>> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("antigravity quota client failed: {e}")))?;
    // 按 prod → daily → autopush 合并组（daily 偶发部分组）。
    let mut merged: std::collections::BTreeMap<String, Vec<serde_json::Value>> = Default::default();
    let mut order: Vec<String> = Vec::new();
    let mut last_status: Option<u16> = None;
    let mut last_body = String::new();
    let mut saw_429: Option<(String, Option<DateTime<Utc>>)> = None;
    for base in bases {
        let url = format!(
            "{}/v1internal:retrieveUserQuotaSummary",
            base.trim_end_matches('/')
        );
        let body = match project_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => serde_json::json!({"project": p}),
            None => serde_json::json!({}),
        };
        subswap_core::request_metrics::record("antigravity", &account.id.0, "http_start", None)
            .await;
        let sent = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", antigravity_ua())
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header(
                "Client-Metadata",
                r#"{"ideType":"ANTIGRAVITY","platform":"MACOS","pluginType":"GEMINI"}"#,
            )
            .json(&body)
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                subswap_core::request_metrics::record(
                    "antigravity",
                    &account.id.0,
                    "http_complete",
                    None,
                )
                .await;
                tracing::debug!(err = %e, base = %base, "antigravity quota endpoint failed; trying next");
                continue;
            }
        };
        let status = resp.status();
        subswap_core::request_metrics::record(
            "antigravity",
            &account.id.0,
            "http_complete",
            Some(status.as_u16()),
        )
        .await;
        let retry_at = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| subswap_core::time::parse_retry_after(v, Utc::now()));
        let text = resp.text().await.unwrap_or_default();
        last_status = Some(status.as_u16());
        last_body = text.clone();
        if status.as_u16() == 429 {
            saw_429 = Some(("Antigravity quota endpoint throttled".to_string(), retry_at));
            continue;
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            // 单个端点的 403 可能是该端点的 license 门槛，继续试下一个；
            // 全部失败后统一按鉴权失败报（needs re-login）。
            tracing::debug!(status = %status, base = %base, "antigravity quota endpoint denied; trying next");
            continue;
        }
        if !status.is_success() {
            tracing::debug!(status = %status, base = %base, "antigravity quota endpoint error; trying next");
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let groups = v
            .get("groups")
            .and_then(|g| g.as_array())
            .cloned()
            .unwrap_or_default();
        for g in groups {
            let name = g
                .get("displayName")
                .and_then(|s| s.as_str())
                .unwrap_or("Quota")
                .to_string();
            let buckets = g
                .get("buckets")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default();
            if buckets.is_empty() {
                continue;
            }
            if !merged.contains_key(&name) {
                order.push(name.clone());
            }
            let entry = merged.entry(name).or_default();
            // 首个非空桶胜出（按 bucketId 去重）。
            for b in buckets {
                let bid = b
                    .get("bucketId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let exists = entry.iter().any(|e: &serde_json::Value| {
                    e.get("bucketId").and_then(|s| s.as_str()).unwrap_or("") == bid
                });
                if !exists {
                    entry.push(b);
                }
            }
        }
    }
    // 合并出 Quota。
    let mut out = Vec::new();
    for name in &order {
        if let Some(buckets) = merged.get(name) {
            for b in buckets {
                if let Some(q) = quota_from_bucket(b, name, crate::PROVIDER_ID, &account.id) {
                    out.push(q);
                }
            }
        }
    }
    // 缺 Gemini 组时用 models 兜底补 Gemini 池（daily 偶发部分组，见 Cli-Proxy-API #499）。
    // 已有 Gemini 组则省一次请求；主通道完全无组时下放走完整回落。
    let has_gemini = order.iter().any(|n| n.to_lowercase().contains("gemini"));
    if !out.is_empty() && !has_gemini {
        for base in bases {
            match fetch_models_once(&client, base, access_token, project_id, &account.id).await {
                Ok(quotas) => {
                    out.extend(
                        quotas
                            .into_iter()
                            .filter(|q| q.note.as_deref() == Some("Antigravity Gemini")),
                    );
                    break;
                }
                Err(Error::QuotaRateLimited { message, retry_at }) => {
                    return Err(Error::QuotaRateLimited { message, retry_at });
                }
                Err(e) => {
                    tracing::debug!(err = %e, base = %base, "antigravity gemini gap-fill failed; continuing");
                    continue;
                }
            }
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    // 主通道无组：回落 fetchAvailableModels（同端点顺序，首个成功即用）。
    for base in bases {
        match fetch_models_once(&client, base, access_token, project_id, &account.id).await {
            Ok(quotas) if !quotas.is_empty() => return Ok(quotas),
            Ok(_) => continue,
            Err(Error::QuotaRateLimited { message, retry_at }) => {
                return Err(Error::QuotaRateLimited { message, retry_at });
            }
            Err(e) => {
                tracing::debug!(err = %e, base = %base, "antigravity models fallback failed; trying next");
                continue;
            }
        }
    }
    if let Some((msg, retry_at)) = saw_429 {
        return Err(Error::QuotaRateLimited {
            message: msg,
            retry_at,
        });
    }
    // 全部端点 401/403 → 鉴权失败（needs re-login）；其余 → 普通 QuotaFetch。
    if matches!(last_status, Some(401) | Some(403)) {
        return Err(Error::QuotaFetch(format!(
            "antigravity quota HTTP {}: needs re-login: {}",
            last_status.unwrap_or(0),
            last_body.chars().take(300).collect::<String>()
        )));
    }
    if out.is_empty() {
        // 无组但 200：视为未知占位而非 Err，避免把新账号判成拉取失败。
        return Ok(vec![]);
    }
    Err(Error::QuotaFetch(format!(
        "antigravity quota HTTP {}: {}",
        last_status.unwrap_or(0),
        last_body.chars().take(300).collect::<String>()
    )))
}

async fn fetch_models_once(
    client: &reqwest::Client,
    base: &str,
    access_token: &str,
    project_id: Option<&str>,
    account_id: &AccountId,
) -> Result<Vec<Quota>> {
    let url = format!(
        "{}/v1internal:fetchAvailableModels",
        base.trim_end_matches('/')
    );
    let body = match project_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => serde_json::json!({"project": p}),
        None => serde_json::json!({}),
    };
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", antigravity_ua())
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("antigravity models request failed: {e}")))?;
    let status = resp.status();
    let retry_at = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| subswap_core::time::parse_retry_after(v, Utc::now()));
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        return Err(Error::QuotaRateLimited {
            message: "Antigravity models endpoint throttled".into(),
            retry_at,
        });
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(Error::QuotaFetch(format!(
            "antigravity models HTTP {status}: needs re-login: {text}"
        )));
    }
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!(
            "antigravity models HTTP {status}: {text}"
        )));
    }
    // 直接用真实账号 id 解析，避免占位 id 污染快照与自动切换。
    Ok(parse_models_fallback(&text, crate::PROVIDER_ID, account_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = r#"{
      "groups": [{
        "displayName": "Claude and GPT models",
        "buckets": [
          {"bucketId":"3p-weekly","displayName":"Weekly Limit Remaining","window":"weekly","resetTime":"2026-11-07T10:39:19Z","remainingFraction":1},
          {"bucketId":"3p-5h","displayName":"Five Hour Limit Remaining","window":"5h","resetTime":"2026-10-31T15:39:19Z","remainingFraction":0.2}
        ]
      },{
        "displayName": "Gemini Models",
        "buckets": [
          {"bucketId":"gemini-weekly","window":"weekly","resetTime":"2026-11-07T10:39:19Z","remainingFraction":0.8},
          {"bucketId":"gemini-5h","window":"5h","resetTime":"2026-10-31T15:39:19Z","remainingFraction":1}
        ]
      }]
    }"#;

    #[test]
    fn parses_summary_windows() {
        let id = AccountId("a@gmail.com".into());
        let q = parse_summary(SUMMARY, "antigravity", &id);
        assert_eq!(q.len(), 4);
        // Claude 5h 余 20% → 已用 80。
        let c5h = q
            .iter()
            .find(|x| {
                x.note.as_deref().unwrap_or("").contains("Claude")
                    && x.window == QuotaWindow::FiveHour
            })
            .unwrap();
        assert_eq!(c5h.used, 80);
        assert!(c5h.reset_at.is_some());
        // Gemini weekly 余 80% → 已用 20。
        let g7d = q
            .iter()
            .find(|x| {
                x.note.as_deref().unwrap_or("").contains("Gemini")
                    && x.window == QuotaWindow::SevenDay
            })
            .unwrap();
        assert_eq!(g7d.used, 20);
    }

    #[test]
    fn missing_fraction_with_reset_is_exhausted() {
        let id = AccountId("x".into());
        let b: serde_json::Value = serde_json::from_str(
            r#"{"bucketId":"gemini-5h","window":"5h","resetTime":"2026-10-31T15:39:19Z"}"#,
        )
        .unwrap();
        let q = quota_from_bucket(&b, "Gemini Models", "antigravity", &id).unwrap();
        assert_eq!(q.used, 100);
        assert_eq!(q.status, QuotaStatus::Exhausted);
    }

    #[test]
    fn disabled_weekly_is_skipped() {
        let id = AccountId("x".into());
        let b: serde_json::Value = serde_json::from_str(
            r#"{"bucketId":"gemini-weekly","window":"weekly","resetTime":"2026-11-07T10:39:19Z","disabled":true}"#,
        )
        .unwrap();
        assert!(quota_from_bucket(&b, "G", "antigravity", &id).is_none());
    }

    #[test]
    fn models_fallback_groups() {
        let body = r#"{"models":{
          "claude-opus-4-6-thinking":{"displayName":"Claude Opus 4.6 (Thinking)","quotaInfo":{"remainingFraction":0.1,"resetTime":"2026-10-31T15:39:17Z"}},
          "gemini-3-flash":{"displayName":"Gemini 3 Flash","quotaInfo":{"remainingFraction":0.9,"resetTime":"2026-10-31T15:39:17Z"}},
          "chat_123":{},
          "gemini-2.5-flash":{"displayName":"Gemini 3.5 Flash Lite","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-10-31T15:39:17Z"}}
        }}"#;
        let q = parse_models_fallback(body, "antigravity", &AccountId("x".into()));
        assert_eq!(q.len(), 2);
        let g = q
            .iter()
            .find(|x| x.note.as_deref() == Some("Antigravity Gemini"))
            .unwrap();
        assert_eq!(g.used, 50); // min(0.9,0.5) → 已用 50
        let c = q
            .iter()
            .find(|x| x.note.as_deref() == Some("Antigravity Claude"))
            .unwrap();
        assert_eq!(c.used, 90);
    }
}
