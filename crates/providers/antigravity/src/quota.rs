//! Antigravity 额度：`POST {base}/v1internal:retrieveUserQuotaSummary` 主通道，
//! `POST {base}/v1internal:fetchAvailableModels` 兜底。
//!
//! - 请求 `User-Agent` 使用 `antigravity/hub/2.24.0 (aidev_client; ...)`（旧 UA 会被上游
//!   过滤掉 `Gemini Models` / `Gemini 4 Argon` 组，只返第三方组）。
//! - 按 `auth_method` 选择计量主域名：个人 consumer 号优先 `daily-cloudcode-pa.googleapis.com`
//!   再回退 `cloudcode-pa.googleapis.com`，企业 business 号反之；首个端点同时拿到 `Gemini`
//!   与 `Claude/GPT` 时立即返回，不再串行请求备用域名。
//! - 池名规范化写入 `Quota.note`（`Gemini` 排最前，独立计量的 `Gemini 4 Argon` / `Carbon` 次之，
//!   `Claude/GPT` 排后），CLI 按池分行对齐，`status --json` 自动导出唯一窗口键，消除 `--`。
//! - 未动用的满额桶（`remainingFraction == 1` 且无 `description` 且 `resetTime ≈ now + window`）
//!   置 `reset_at = None`，避免把上游滚动占位时间渲染成永不递减的倒计时。
//! - proto3 省略 0 值：有 `resetTime` 且非 `disabled` 的缺字段桶按耗尽（0）计。
//! - 429 立即返回 `Error::QuotaRateLimited`（含 `Retry-After`），不切备用 base、不判死号；
//!   401/403 → `needs re-login`。

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

const DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
const PROD: &str = "https://cloudcode-pa.googleapis.com";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const CLIENT_METADATA_HEADER: &str =
    r#"{"ideType":"ANTIGRAVITY","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#;

fn bases_for(auth_method: Option<&str>) -> Vec<String> {
    if let Ok(v) = std::env::var("SUBSWAP_ANTIGRAVITY_BASE") {
        if !v.trim().is_empty() {
            return vec![v.trim_end_matches('/').to_string()];
        }
    }
    let is_business = auth_method
        .map(|m| m.to_ascii_lowercase().contains("business"))
        .unwrap_or(false);
    if is_business {
        vec![PROD.into(), DAILY.into()]
    } else {
        vec![DAILY.into(), PROD.into()]
    }
}

fn http_client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| Error::QuotaFetch(format!("antigravity quota client failed: {e}")))
}

fn antigravity_ua() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| {
        let os_type = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        };
        format!(
            "antigravity/hub/2.24.0 (aidev_client; os_type={os_type}; arch={arch}; cl=963137146)"
        )
    })
}

/// 按桶 ID 与上游组名规范化池名：保留独立计量的 Argon / Carbon 组，防止与默认 Gemini 周桶混淆。
fn bucket_pool_name(group_name: &str, bucket_id: &str) -> String {
    let trimmed = group_name.trim();
    let lower_group = trimmed.to_ascii_lowercase();
    let lower_bid = bucket_id.trim().to_ascii_lowercase();

    if lower_bid.starts_with("gemini-argon-") || lower_group.contains("argon") {
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("quota") {
            return trimmed.to_string();
        }
        return "Gemini Argon".into();
    }
    if lower_bid.starts_with("gemini-carbon-") || lower_group.contains("carbon") {
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("quota") {
            return trimmed.to_string();
        }
        return "Gemini Carbon".into();
    }
    if lower_bid.starts_with("gemini-") || lower_group.contains("gemini") {
        return "Gemini".into();
    }
    if lower_bid.starts_with("3p-") || lower_group.contains("claude") || lower_group.contains("gpt")
    {
        return "Claude/GPT".into();
    }
    if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("quota") {
        trimmed.to_string()
    } else {
        String::new()
    }
}

/// 固定池顺序：默认 Gemini 第一行，独立 Gemini 子池（Argon/Carbon）次之，Claude/GPT 随后。
fn pool_sort_rank(pool: &str) -> u8 {
    if pool == "Gemini" {
        0
    } else if pool.starts_with("Gemini") {
        1
    } else if pool == "Claude/GPT" {
        2
    } else if pool.is_empty() {
        3
    } else {
        4
    }
}

/// 未动用的满额桶（`remainingFraction == 1` 且无 `description`）上游会返回 `now + window`
/// 的滚动时间；识别后清空为 `None`，与其它 Provider 未启动滚动窗一致显示 `reset --`。
fn summary_reset_at(
    bucket: &serde_json::Value,
    remaining: f64,
    window: QuotaWindow,
    requested_at: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let parsed = bucket
        .get("resetTime")
        .or_else(|| bucket.get("reset_time"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())?;

    let Some(req_at) = requested_at else {
        return Some(parsed);
    };
    let has_description = bucket
        .get("description")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    if (remaining - 1.0).abs() < 1e-9
        && !has_description
        && window.duration_seconds().is_some_and(|window_secs| {
            (parsed.signed_duration_since(req_at).num_seconds() - window_secs as i64).abs() <= 120
        })
    {
        return None;
    }
    Some(parsed)
}

/// 解析单个桶为 Quota（规范化池名进 `note` 前缀，便于按池分组展示）。
#[cfg(test)]
pub fn quota_from_bucket(
    bucket: &serde_json::Value,
    group_name: &str,
    provider: &str,
    id: &AccountId,
) -> Option<Quota> {
    quota_from_bucket_at(bucket, group_name, provider, id, None)
}

fn quota_from_bucket_at(
    bucket: &serde_json::Value,
    group_name: &str,
    provider: &str,
    id: &AccountId,
    requested_at: Option<DateTime<Utc>>,
) -> Option<Quota> {
    if bucket.get("disabled").and_then(|v| v.as_bool()) == Some(true) {
        // weekly 被禁用时整桶跳过；5h disabled 仍保留为 0（上游约定的展示语义）。
        let window_raw = window_of(bucket).to_lowercase();
        if window_raw.contains("week") {
            return None;
        }
    }
    let remaining = remaining_fraction(bucket)?;
    let used_pct = ((1.0 - remaining) * 100.0).clamp(0.0, 100.0);
    let used = used_pct.round().clamp(0.0, 100.0) as u64;
    let window = window_from_bucket(bucket);
    let reset_at = summary_reset_at(bucket, remaining, window, requested_at);
    let bucket_label = bucket
        .get("displayName")
        .or_else(|| bucket.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let pool = bucket_pool_name(group_name, bucket_id_of(bucket));
    let note = if pool.is_empty() {
        (!bucket_label.is_empty()).then(|| bucket_label.to_string())
    } else if bucket_label.is_empty() {
        Some(pool)
    } else {
        Some(format!("{pool} · {bucket_label}"))
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

fn bucket_id_of(bucket: &serde_json::Value) -> &str {
    bucket
        .get("bucketId")
        .or_else(|| bucket.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
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
    bucket_id_of(bucket).to_string()
}

fn window_from_bucket(bucket: &serde_json::Value) -> QuotaWindow {
    let raw = window_of(bucket).to_lowercase();
    let bid = bucket_id_of(bucket).to_lowercase();
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
    // 未知桶名保守归入 5h 滚动窗，避免落入无时长 `--`（Custom）。
    QuotaWindow::FiveHour
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
        .or_else(|| bucket.get("reset_time"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    let disabled = bucket.get("disabled").and_then(|v| v.as_bool()) == Some(true);
    if has_reset && !disabled {
        return Some(0.0);
    }
    None
}

/// 解析 quotaSummary 全身：按规范化池名排序（Gemini 在前，Argon 次之，Claude/GPT 在后），供测试复用。
#[cfg(test)]
pub fn parse_summary(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    parse_summary_at(body, provider, id, None)
}

#[cfg(test)]
fn parse_summary_at(
    body: &str,
    provider: &str,
    id: &AccountId,
    requested_at: Option<DateTime<Utc>>,
) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let mut pooled: Vec<(usize, String, Quota)> = Vec::new();
    let mut seq = 0_usize;
    if let Some(groups) = v.get("groups").and_then(|g| g.as_array()) {
        for g in groups {
            let raw_name = g
                .get("displayName")
                .or_else(|| g.get("name"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let Some(buckets) = g.get("buckets").and_then(|b| b.as_array()) else {
                continue;
            };
            for b in buckets {
                if let Some(q) = quota_from_bucket_at(b, raw_name, provider, id, requested_at) {
                    let pool = q.pool_group().unwrap_or("").to_string();
                    pooled.push((seq, pool, q));
                    seq += 1;
                }
            }
        }
    } else if let Some(buckets) = v.get("buckets").and_then(|b| b.as_array()) {
        for b in buckets {
            if let Some(q) = quota_from_bucket_at(b, "", provider, id, requested_at) {
                let pool = q.pool_group().unwrap_or("").to_string();
                pooled.push((seq, pool, q));
                seq += 1;
            }
        }
    }
    pooled.sort_by_key(|(idx, pool, _)| (pool_sort_rank(pool), *idx));
    pooled.into_iter().map(|(_, _, q)| q).collect()
}

fn update_min_slot(
    slot: &mut Option<(f64, Option<DateTime<Utc>>)>,
    rem: f64,
    reset: Option<DateTime<Utc>>,
) {
    match slot {
        Some((cur, cur_reset)) if rem >= *cur => {
            if (rem - *cur).abs() < 1e-9 {
                if let (Some(nr), Some(cr)) = (reset, *cur_reset) {
                    if nr < cr {
                        *cur_reset = Some(nr);
                    }
                } else if reset.is_some() && cur_reset.is_none() {
                    *cur_reset = reset;
                }
            }
        }
        _ => *slot = Some((rem, reset)),
    }
}

fn extract_quota_info(qi: Option<&serde_json::Value>) -> Option<(f64, Option<DateTime<Utc>>)> {
    let qi = qi?;
    let rem = qi
        .get("remainingFraction")
        .or_else(|| qi.get("remaining_fraction"))
        .and_then(|n| n.as_f64())
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(0.0, 1.0))?;
    let reset = qi
        .get("resetTime")
        .or_else(|| qi.get("reset_time"))
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());
    Some((rem, reset))
}

/// 解析 fetchAvailableModels 兜底：按 Gemini / Claude/GPT 取各池 5h 与 weekly 最小余量。
pub fn parse_models_fallback(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let Some(models) = v.get("models").and_then(|m| m.as_object()) else {
        return vec![];
    };
    let mut gemini_5h: Option<(f64, Option<DateTime<Utc>>)> = None;
    let mut gemini_7d: Option<(f64, Option<DateTime<Utc>>)> = None;
    let mut claude_5h: Option<(f64, Option<DateTime<Utc>>)> = None;
    let mut claude_7d: Option<(f64, Option<DateTime<Utc>>)> = None;

    for (name, entry) in models {
        if name.starts_with("chat_") || name.starts_with("tab_") {
            continue;
        }
        let disp = entry
            .get("displayName")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let mp = entry
            .get("modelProvider")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let combined = format!("{name} {disp} {mp}").to_lowercase();
        let is_claude_or_gpt = combined.contains("claude")
            || combined.contains("anthropic")
            || combined.contains("gpt")
            || combined.contains("openai");
        let is_gemini = combined.contains("gemini") || combined.contains("google");
        if !is_claude_or_gpt && !is_gemini {
            continue;
        }

        if let Some((rem, reset)) = extract_quota_info(entry.get("quotaInfo")) {
            let slot = if is_claude_or_gpt {
                &mut claude_5h
            } else {
                &mut gemini_5h
            };
            update_min_slot(slot, rem, reset);
        }
        if let Some((rem, reset)) = extract_quota_info(
            entry
                .get("weeklyQuotaInfo")
                .or_else(|| entry.get("weekly_quota_info")),
        ) {
            let slot = if is_claude_or_gpt {
                &mut claude_7d
            } else {
                &mut gemini_7d
            };
            update_min_slot(slot, rem, reset);
        }
    }

    let mut out = Vec::new();
    for (pool, window, label, slot) in [
        (
            "Gemini",
            QuotaWindow::FiveHour,
            "Five Hour Limit Remaining",
            gemini_5h,
        ),
        (
            "Gemini",
            QuotaWindow::SevenDay,
            "Weekly Limit Remaining",
            gemini_7d,
        ),
        (
            "Claude/GPT",
            QuotaWindow::FiveHour,
            "Five Hour Limit Remaining",
            claude_5h,
        ),
        (
            "Claude/GPT",
            QuotaWindow::SevenDay,
            "Weekly Limit Remaining",
            claude_7d,
        ),
    ] {
        if let Some((rem, reset)) = slot {
            let pct = ((1.0 - rem) * 100.0).clamp(0.0, 100.0);
            out.push(Quota {
                provider: provider.into(),
                account_id: id.clone(),
                window,
                used: pct.round() as u64,
                limit: 100,
                reset_at: reset,
                status: QuotaStatus::from_percent(pct),
                note: Some(format!("{pool} · {label}")),
            });
        }
    }
    out
}

/// 用 access_token 查额度：优先计量主域名拿全双池即停，缺池时才补查备用端点或 fetchAvailableModels。
pub async fn fetch_quota(
    access_token: &str,
    project_id: Option<&str>,
    auth_method: Option<&str>,
    account: &Account,
) -> Result<Vec<Quota>> {
    fetch_quota_at(access_token, project_id, account, &bases_for(auth_method)).await
}

async fn fetch_quota_at(
    access_token: &str,
    project_id: Option<&str>,
    account: &Account,
    bases: &[String],
) -> Result<Vec<Quota>> {
    let client = http_client()?;
    let mut merged: std::collections::BTreeMap<String, Vec<(DateTime<Utc>, serde_json::Value)>> =
        Default::default();
    let mut order: Vec<String> = Vec::new();
    let mut working_base: Option<String> = None;
    let mut last_status: Option<u16> = None;
    let mut last_body = String::new();

    for base in bases {
        let url = format!(
            "{}/v1internal:retrieveUserQuotaSummary",
            base.trim_end_matches('/')
        );
        let body = match project_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) => serde_json::json!({"project": p}),
            None => serde_json::json!({}),
        };
        let requested_at = Utc::now();
        subswap_core::request_metrics::record(crate::PROVIDER_ID, &account.id.0, "http_start", None)
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
            .header("Client-Metadata", CLIENT_METADATA_HEADER)
            .json(&body)
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                subswap_core::request_metrics::record(
                    crate::PROVIDER_ID,
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
            crate::PROVIDER_ID,
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
            // 429 立即返回限流错误，不再串行打下一个 base（见 PROVIDER_KNOWLEDGE_BASE 429 规则）。
            return Err(Error::QuotaRateLimited {
                message: "Antigravity quota endpoint throttled".into(),
                retry_at,
            });
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
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
        if working_base.is_none() {
            working_base = Some(base.clone());
        }
        let groups = v
            .get("groups")
            .and_then(|g| g.as_array())
            .cloned()
            .unwrap_or_default();
        for g in groups {
            let raw_name = g
                .get("displayName")
                .or_else(|| g.get("name"))
                .and_then(|s| s.as_str())
                .unwrap_or("Quota");
            let buckets = g
                .get("buckets")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default();
            for b in buckets {
                let bid = bucket_id_of(&b).to_string();
                let pool = bucket_pool_name(raw_name, &bid);
                if !merged.contains_key(&pool) {
                    order.push(pool.clone());
                }
                let entry = merged.entry(pool).or_default();
                // 首个非空桶胜出（按 bucketId 去重）。
                let exists = entry
                    .iter()
                    .any(|(_, e): &(DateTime<Utc>, serde_json::Value)| bucket_id_of(e) == bid);
                if !exists {
                    entry.push((requested_at, b));
                }
            }
        }
        // 已同时拿到 Gemini 与 Claude/GPT 双池，立即返回，省掉备用端点往返。
        if merged.contains_key("Gemini") && merged.contains_key("Claude/GPT") {
            break;
        }
    }

    // 固定将 Gemini（及 Argon/Carbon 子池）排在 Claude/GPT 之前。
    let mut ordered_pools: Vec<(usize, String)> = order.into_iter().enumerate().collect();
    ordered_pools.sort_by_key(|(idx, pool)| (pool_sort_rank(pool), *idx));

    let mut out = Vec::new();
    for (_, name) in &ordered_pools {
        if let Some(buckets) = merged.get(name) {
            for (req_at, b) in buckets {
                if let Some(q) = quota_from_bucket_at(
                    b,
                    name,
                    crate::PROVIDER_ID,
                    &account.id,
                    Some(*req_at),
                ) {
                    out.push(q);
                }
            }
        }
    }

    // 缺 Gemini 组时用 fetchAvailableModels 补齐 Gemini（插在列表开头保持 Gemini 在前）。
    let has_gemini = merged.keys().any(|k| k.starts_with("Gemini"));
    if !out.is_empty() && !has_gemini {
        let mut candidate_bases: Vec<&str> = Vec::new();
        if let Some(wb) = working_base.as_deref() {
            candidate_bases.push(wb);
        }
        for b in bases {
            if Some(b.as_str()) != working_base.as_deref() {
                candidate_bases.push(b.as_str());
            }
        }
        for base in candidate_bases {
            match fetch_models_once(client, base, access_token, project_id, &account.id).await {
                Ok(quotas) => {
                    let mut gemini: Vec<Quota> = quotas
                        .into_iter()
                        .filter(|q| q.pool_group() == Some("Gemini"))
                        .collect();
                    if !gemini.is_empty() {
                        gemini.extend(out);
                        out = gemini;
                    }
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

    // 主通道无组：回落 fetchAvailableModels（首个成功即用）。
    for base in bases {
        match fetch_models_once(client, base, access_token, project_id, &account.id).await {
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

    // 全部端点 401/403 → 鉴权失败（needs re-login）；200 空组 → 空列表；其余 → QuotaFetch。
    if matches!(last_status, Some(401) | Some(403)) {
        return Err(Error::QuotaFetch(format!(
            "antigravity quota HTTP {}: needs re-login: {}",
            last_status.unwrap_or(0),
            last_body.chars().take(300).collect::<String>()
        )));
    }
    if last_status == Some(200) {
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
    subswap_core::request_metrics::record(crate::PROVIDER_ID, &account_id.0, "http_start", None)
        .await;
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", antigravity_ua())
        .header(
            "X-Goog-Api-Client",
            "google-cloud-sdk vscode_cloudshelleditor/0.1",
        )
        .header("Client-Metadata", CLIENT_METADATA_HEADER)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            subswap_core::request_metrics::record(
                crate::PROVIDER_ID,
                &account_id.0,
                "http_complete",
                None,
            )
            .await;
            return Err(Error::QuotaFetch(format!(
                "antigravity models request failed: {e}"
            )));
        }
    };
    let status = resp.status();
    subswap_core::request_metrics::record(
        crate::PROVIDER_ID,
        &account_id.0,
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
    Ok(parse_models_fallback(&text, crate::PROVIDER_ID, account_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
          {"bucketId":"gemini-5h","window":"5h","resetTime":"2026-10-31T15:39:19Z","remainingFraction":0.7}
        ]
      }]
    }"#;

    fn dummy_account(id: &str) -> Account {
        Account {
            provider: "antigravity".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn ua_uses_hub_2_24_0_identity() {
        let ua = antigravity_ua();
        assert!(ua.starts_with("antigravity/hub/2.24.0 (aidev_client; os_type="), "{ua}");
    }

    #[test]
    fn parses_summary_windows_with_gemini_ordered_first() {
        let id = AccountId("a@gmail.com".into());
        let q = parse_summary(SUMMARY, "antigravity", &id);
        assert_eq!(q.len(), 4);
        // 即使上游 JSON 把 Claude 放第 0 项，输出也固定 Gemini 在前、Claude/GPT 在后。
        assert_eq!(q[0].pool_group(), Some("Gemini"));
        assert_eq!(q[1].pool_group(), Some("Gemini"));
        assert_eq!(q[2].pool_group(), Some("Claude/GPT"));
        assert_eq!(q[3].pool_group(), Some("Claude/GPT"));

        let g5h = q
            .iter()
            .find(|x| x.pool_group() == Some("Gemini") && x.window == QuotaWindow::FiveHour)
            .unwrap();
        assert_eq!(g5h.used, 30);

        let c5h = q
            .iter()
            .find(|x| x.pool_group() == Some("Claude/GPT") && x.window == QuotaWindow::FiveHour)
            .unwrap();
        assert_eq!(c5h.used, 80);
        assert!(q.iter().all(|x| x.window != QuotaWindow::Custom));
    }

    #[test]
    fn preserves_argon_pool_separate_from_default_gemini() {
        let body = r#"{
          "groups": [
            {"displayName":"Claude and GPT models","buckets":[{"bucketId":"3p-weekly","window":"weekly","remainingFraction":1}]},
            {"displayName":"Gemini 4 Argon","buckets":[{"bucketId":"gemini-argon-wtu-weekly","window":"weekly","remainingFraction":0.4}]},
            {"displayName":"Gemini Models","buckets":[{"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.9}]}
          ]
        }"#;
        let q = parse_summary(body, "antigravity", &AccountId("u@gmail.com".into()));
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].pool_group(), Some("Gemini"));
        assert_eq!(q[0].used, 10);
        assert_eq!(q[1].pool_group(), Some("Gemini 4 Argon"));
        assert_eq!(q[1].used, 60);
        assert_eq!(q[2].pool_group(), Some("Claude/GPT"));
        assert_eq!(q[2].used, 0);
    }

    #[test]
    fn untouched_rolling_bucket_clears_synthetic_reset_time() {
        let req_at: DateTime<Utc> = "2026-11-13T17:00:00Z".parse().unwrap();
        let body = r#"{
          "groups": [{
            "displayName": "Claude and GPT models",
            "buckets": [
              {"bucketId":"3p-5h","window":"5h","remainingFraction":1,"resetTime":"2026-11-13T22:00:01Z"},
              {"bucketId":"3p-weekly","window":"weekly","remainingFraction":0.85,"resetTime":"2026-11-20T17:00:01Z"}
            ]
          }]
        }"#;
        let q = parse_summary_at(
            body,
            "antigravity",
            &AccountId("u@gmail.com".into()),
            Some(req_at),
        );
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].reset_at, None, "untouched full window must hide rolling reset");
        assert!(q[1].reset_at.is_some(), "consumed window must keep real reset");
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
        assert_eq!(q.pool_group(), Some("Gemini"));
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
    fn models_fallback_groups_never_use_custom_window_and_reads_weekly() {
        let body = r#"{"models":{
          "claude-opus-4-6-thinking":{"displayName":"Claude Opus 4.6 (Thinking)","quotaInfo":{"remainingFraction":0.1,"resetTime":"2026-10-31T15:39:17Z"}},
          "gemini-3-flash":{"displayName":"Gemini 3 Flash","quotaInfo":{"remainingFraction":0.9,"resetTime":"2026-10-31T15:39:17Z"},"weeklyQuotaInfo":{"remainingFraction":0.8,"resetTime":"2026-11-07T10:39:17Z"}},
          "chat_123":{},
          "gemini-2.5-flash":{"displayName":"Gemini 3.5 Flash Lite","quotaInfo":{"remainingFraction":0.5,"resetTime":"2026-10-31T15:39:17Z"}}
        }}"#;
        let q = parse_models_fallback(body, "antigravity", &AccountId("x".into()));
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].pool_group(), Some("Gemini"));
        assert_eq!(q[0].window, QuotaWindow::FiveHour);
        assert_eq!(q[0].used, 50);
        assert_eq!(q[1].pool_group(), Some("Gemini"));
        assert_eq!(q[1].window, QuotaWindow::SevenDay);
        assert_eq!(q[1].used, 20);
        assert_eq!(q[2].pool_group(), Some("Claude/GPT"));
        assert_eq!(q[2].window, QuotaWindow::FiveHour);
        assert_eq!(q[2].used, 90);
    }

    #[tokio::test]
    async fn fetch_quota_stops_after_first_complete_base() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let primary = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_base = format!("http://{}", primary.local_addr().unwrap());
        let secondary = TcpListener::bind("127.0.0.1:0").unwrap();
        let secondary_base = format!("http://{}", secondary.local_addr().unwrap());
        let secondary_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = secondary_hits.clone();

        let h1 = std::thread::spawn(move || {
            let (mut s, _) = primary.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(
                req.to_ascii_lowercase().contains("user-agent: antigravity/hub/2.24.0"),
                "request must carry hub/2.24.0 UA, got: {req}"
            );
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                SUMMARY.len(),
                SUMMARY
            )
            .unwrap();
        });
        secondary.set_nonblocking(true).unwrap();

        let acc = dummy_account("u@gmail.com");
        let quotas = fetch_quota_at(
            "ya29.token",
            None,
            &acc,
            &[primary_base, secondary_base],
        )
        .await
        .unwrap();
        h1.join().unwrap();
        if secondary.accept().is_ok() {
            hits_clone.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(
            secondary_hits.load(Ordering::SeqCst),
            0,
            "secondary base must not be hit when primary returns both pools"
        );
        assert_eq!(quotas.len(), 4);
        assert_eq!(quotas[0].pool_group(), Some("Gemini"));
        assert_eq!(quotas[2].pool_group(), Some("Claude/GPT"));
    }

    #[tokio::test]
    async fn fetch_quota_429_returns_immediately_without_trying_next_base() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let primary = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_base = format!("http://{}", primary.local_addr().unwrap());
        let secondary = TcpListener::bind("127.0.0.1:0").unwrap();
        let secondary_base = format!("http://{}", secondary.local_addr().unwrap());

        let h1 = std::thread::spawn(move || {
            let (mut s, _) = primary.accept().unwrap();
            let mut buf = [0_u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"error":"rate_limited"}"#;
            write!(
                s,
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1800\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        secondary.set_nonblocking(true).unwrap();

        let acc = dummy_account("u@gmail.com");
        let err = fetch_quota_at(
            "ya29.token",
            None,
            &acc,
            &[primary_base, secondary_base],
        )
        .await
        .unwrap_err();
        h1.join().unwrap();
        assert!(
            secondary.accept().is_err(),
            "429 must not cascade onto fallback base"
        );
        match err {
            Error::QuotaRateLimited { retry_at, .. } => {
                assert!(retry_at.is_some(), "Retry-After header must be parsed");
            }
            other => panic!("expected QuotaRateLimited, got: {other:?}"),
        }
    }
}
