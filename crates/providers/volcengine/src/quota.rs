//! 火山方舟 Coding Plan 额度查询。
//!
//! 推理 Bearer Key 拿不到用量（实测无 `x-ratelimit-*` 头），必须走控制面 OpenAPI
//! `GetCodingPlanUsage`（Coding Plan，回百分比，优先）与 `GetAFPUsage`（Agent Plan，回绝对值，兜底），
//! 经火山签名 V4（AK/SK）鉴权。AK/SK 与推理 Key 是两套凭据，分开存放。
//! 签名按官方 Java/Python/PHP demo 与 magpie 实现：空请求体按 `{}` 哈希，
//! `Content-Type: application/json`，SignedHeaders 排序为
//! `content-type;host;x-content-sha256;x-date`，query 仅 `Action`/`Version`。

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use subswap_core::{AccountId, Quota, QuotaStatus, QuotaWindow};

use super::PROVIDER_ID;

/// 控制面 OpenAPI 统一网关（区别于数据面推理域名）。
const OPENAPI_HOST: &str = "open.volcengineapi.com";
const API_VERSION: &str = "2024-01-01";
const SERVICE: &str = "ark";
const CONTENT_TYPE: &str = "application/json";
const SIGNED_HEADERS: &str = "content-type;host;x-content-sha256;x-date";

/// 从环境变量覆盖 OpenAPI 网关，供测试 mock 用。
/// 支持裸 host（`open.volcengineapi.com` / `host:port`）与完整 URL
///（`http://127.0.0.1:PORT`）；返回 `(scheme, host)`，签名 `host:` 头与实际 URL 共用同一结果。
fn openapi_endpoint() -> (String, String) {
    let raw = std::env::var("SUBSWAP_VOLCENGINE_OPENAPI_BASE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| OPENAPI_HOST.into());
    if let Some(rest) = raw.strip_prefix("http://") {
        ("http".into(), rest.to_string())
    } else if let Some(rest) = raw.strip_prefix("https://") {
        ("https".into(), rest.to_string())
    } else {
        ("https".into(), raw)
    }
}

/// 签名与 `Host` 头用的 host 部分（不含 scheme，含非默认端口）。
fn openapi_host() -> String {
    openapi_endpoint().1
}

/// HMAC-SHA256（标准构造，避免新增 `hmac` 依赖）。
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hash = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// 按 key 字母序排序的 canonical query；同一份字符串同时用于签名与实际 URL。
/// 只含 `Action`/`Version`（与官方计费示例及 `pi-ark-usage`/`magpie` 一致）；
/// `Region` 只进 credential scope，不进 query，避免网关对多余参数的严格校验。
fn canonical_query(action: &str) -> String {
    format!("Action={action}&Version={API_VERSION}")
}

/// 生成鉴权头三元组（`Authorization`、`X-Date`、`X-Content-Sha256`）。
fn sign(
    access_key_id: &str,
    secret_access_key: &str,
    region: &str,
    canonical: &str,
    body: &[u8],
    now: chrono::DateTime<Utc>,
) -> (String, String, String) {
    let x_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let short_date = now.format("%Y%m%d").to_string();
    let body_hash = sha256_hex(body);
    // 官方规范：canonical headers 按小写 header 名 ASCII 排序，与 SignedHeaders 顺序一致。
    let canonical_headers = format!(
        "content-type:{CONTENT_TYPE}\nhost:{}\nx-content-sha256:{body_hash}\nx-date:{x_date}\n",
        openapi_host(),
    );
    let canonical_request = format!(
        "POST\n/\n{canonical}\n{canonical_headers}\n{SIGNED_HEADERS}\n{body_hash}"
    );
    let scope = format!("{short_date}/{region}/{SERVICE}/request");
    let string_to_sign = format!(
        "HMAC-SHA256\n{x_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(secret_access_key.as_bytes(), short_date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"request");
    let signature: String = hmac_sha256(&k_signing, string_to_sign.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let authorization = format!(
        "HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={SIGNED_HEADERS}, Signature={signature}"
    );
    (authorization, x_date, body_hash)
}

/// 单次 OpenAPI 调用的归类结果。
enum CallOutcome {
    Body(serde_json::Value),
    Auth(String),
    Soft(String),
    /// 签名通过但账号无订阅：调用方视为“空”，再试另一通道（另一 Action / arkcli）。
    NotSubscribed(String),
    RateLimited { message: String, retry_at: Option<DateTime<Utc>> },
    Transient(String),
}

/// 从响应体提取火山 `ResponseMetadata.Error`。
fn response_error(body: &serde_json::Value) -> Option<(String, String)> {
    let err = body
        .get("ResponseMetadata")
        .and_then(|meta| meta.get("Error"))
        .or_else(|| body.get("Error"))?;
    let code = err.get("Code").and_then(|value| value.as_str()).unwrap_or_default();
    let msg = err.get("Message").and_then(|value| value.as_str()).unwrap_or_default();
    if code.is_empty() && msg.is_empty() {
        None
    } else {
        Some((code.into(), msg.into()))
    }
}

fn is_auth_code(code: &str) -> bool {
    let lower = code.to_lowercase();
    lower.contains("auth")
        || lower.contains("signature")
        || lower.contains("accessdenied")
        || lower.contains("denied")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("credential")
        || lower.contains("invalidtoken")
        || lower.contains("expiredtoken")
        || lower.contains("tokenexpired")
        || lower.contains("tokeninvalid")
        || lower.contains("invalidauthorization")
        || lower.contains("invalidaccesskey")
        || lower.contains("invalidsecret")
        || lower.contains("invalidcredential")
        || lower.contains("invalidkey")
        || lower.contains("signaturedoesnotmatch")
        || lower.contains("accesskey")
        || lower.contains("secretaccess")
        || lower.contains("unrecognizedclient")
        || lower.contains("missingauthentication")
}

/// 200 信封或非 2xx 信封里的限流码：必须走 `RateLimited`（含 429 语义），不得当 Soft 重试或切换通道。
fn is_rate_limit_code(code: &str) -> bool {
    let lower = code.to_lowercase();
    lower.contains("throttl")
        || lower.contains("ratelimit")
        || lower.contains("rate_limit")
        || lower.contains("too many")
        || lower.contains("toomanyrequests")
        || lower.contains("requestlimit")
        || lower.contains("limitexceeded")
        || lower.contains("slowdown")
        || lower.contains("flowcontrol")
}

/// 未订阅/未开通：签名通过但账号无 Coding Plan 订阅，调用方可再试另一通道。
/// 只认明确的订阅语义；`ResourceNotFound` 这类通用码按 Soft 处理，避免把
/// “Action 写错”等实现 bug 误判成未订阅而静默吞掉。
fn is_not_subscribed_code(code: &str) -> bool {
    let lower = code.to_lowercase();
    lower.contains("notsubscribed")
        || lower.contains("not_subscribed")
        || lower.contains("unsubscribed")
        || lower.contains("notpurchased")
        || lower.contains("not_purchased")
        || lower.contains("subscriptionnotfound")
        || lower.contains("productnotsubscribed")
}

async fn openapi_call(
    client: &reqwest::Client,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
    action: &str,
) -> CallOutcome {
    let canonical = canonical_query(action);
    let (scheme, host) = openapi_endpoint();
    let url = format!("{scheme}://{host}/?{canonical}");
    // 控制面 JSON API 空请求体按 `{}` 发送（`{}` 哈希参与签名），与 magpie/dsh/ark-cli 一致；
    // 空串哈希会导致签名与服务端期望不一致。
    let body: &[u8] = b"{}";
    let (authorization, x_date, body_hash) = sign(
        access_key_id,
        secret_access_key,
        region,
        &canonical,
        body,
        Utc::now(),
    );
    let sent = client
        .post(&url)
        .header("X-Date", x_date)
        .header("X-Content-Sha256", body_hash)
        .header("Content-Type", CONTENT_TYPE)
        .header("Accept", "application/json")
        .header("Authorization", authorization)
        .body(body.to_vec())
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    let resp = match sent {
        Ok(resp) => resp,
        Err(err) => return CallOutcome::Transient(format!("network error: {err}")),
    };
    let status = resp.status();
    let retry_at = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| subswap_core::time::parse_retry_after(value, Utc::now()));
    if status.as_u16() == 429 {
        let body = resp.text().await.unwrap_or_default();
        return CallOutcome::RateLimited {
            message: format!("Volcengine usage throttled (HTTP 429): {body}"),
            retry_at,
        };
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        let body = resp.text().await.unwrap_or_default();
        return CallOutcome::Auth(format!(
            "volcengine AK/SK auth failed (HTTP {status} needs re-login): {body}"
        ));
    }
    if !status.is_success() {
        let raw = resp.text().await.unwrap_or_default();
        if let Ok(body) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some((code, msg)) = response_error(&body) {
                if is_rate_limit_code(&code) {
                    return CallOutcome::RateLimited {
                        message: format!("Volcengine usage throttled ({code}): {msg}"),
                        retry_at,
                    };
                }
                if is_auth_code(&code) {
                    return CallOutcome::Auth(format!(
                        "volcengine AK/SK auth failed (HTTP {status} needs re-login): {code}: {msg}"
                    ));
                }
                if is_not_subscribed_code(&code) {
                    return CallOutcome::NotSubscribed(format!("{code}: {msg}"));
                }
                return CallOutcome::Soft(format!("HTTP {status} {code}: {msg}"));
            }
        }
        return CallOutcome::Soft(format!("HTTP {status}: {raw}"));
    }
    let raw = match resp.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => return CallOutcome::Transient(format!("read response failed: {err}")),
    };
    let body: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(err) => return CallOutcome::Soft(format!("parse response failed: {err}")),
    };
    if let Some((code, msg)) = response_error(&body) {
        if is_rate_limit_code(&code) {
            return CallOutcome::RateLimited {
                message: format!("Volcengine usage throttled ({code}): {msg}"),
                retry_at,
            };
        }
        if is_auth_code(&code) {
            return CallOutcome::Auth(format!(
                "volcengine AK/SK auth failed (needs re-login): {code}: {msg}"
            ));
        }
        if is_not_subscribed_code(&code) {
            return CallOutcome::NotSubscribed(format!("{code}: {msg}"));
        }
        return CallOutcome::Soft(format!("{code}: {msg}"));
    }
    CallOutcome::Body(body)
}

/// 重置时间兼容秒/毫秒/浮点/数字字符串/RFC3339；0 或负数视为无重置时间。
fn parse_reset(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
            return Some(dt.with_timezone(&Utc));
        }
        // 数字字符串（如 `"1753189560000"`）：按数值走统一分支。
        if let Ok(num) = trimmed.parse::<f64>() {
            if num.is_finite() {
                return reset_from_f64(num);
            }
        }
        return None;
    }
    if let Some(num) = value.as_i64() {
        return reset_from_i64(num);
    }
    if let Some(num) = value.as_u64() {
        if num <= i64::MAX as u64 {
            return reset_from_i64(num as i64);
        }
        return None;
    }
    if let Some(num) = value.as_f64() {
        return reset_from_f64(num);
    }
    None
}

/// 整数时间戳：秒级 < 1e12，毫秒级 >= 1e12；0/负数无重置时间。
fn reset_from_i64(num: i64) -> Option<DateTime<Utc>> {
    if num <= 0 {
        return None;
    }
    let (secs, nsecs) = if num < 1_000_000_000_000 {
        (num, 0)
    } else {
        (num / 1000, ((num % 1000) * 1_000_000) as u32)
    };
    DateTime::from_timestamp(secs, nsecs)
}

/// 浮点时间戳：先取整再按整数规则处理；非有限/<=0 无重置时间。
fn reset_from_f64(num: f64) -> Option<DateTime<Utc>> {
    if !num.is_finite() || num <= 0.0 {
        return None;
    }
    reset_from_i64(num.trunc() as i64)
}

fn parse_f64(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        value
            .as_str()
            .and_then(|text| text.trim().parse().ok())
            .filter(|num: &f64| num.is_finite())
    })
}

/// 百分比窗口落盘形态：`used` 为已用百分比 0~100，`limit` 固定 100。
fn percent_quota(
    id: &AccountId,
    window: QuotaWindow,
    percent: f64,
    reset_at: Option<DateTime<Utc>>,
) -> Quota {
    let used = percent.clamp(0.0, 100.0).round() as u64;
    Quota {
        provider: PROVIDER_ID.into(),
        account_id: id.clone(),
        window,
        used,
        limit: 100,
        reset_at,
        status: QuotaStatus::from_percent(used as f64),
        note: None,
    }
}

fn coding_window(label: &str) -> Option<QuotaWindow> {
    match label.to_lowercase().as_str() {
        "session" | "5h" | "fivehour" | "five_hour" | "rolling_5h" => Some(QuotaWindow::FiveHour),
        "weekly" | "week" | "7d" => Some(QuotaWindow::SevenDay),
        "monthly" | "month" => Some(QuotaWindow::Month),
        _ => None,
    }
}

/// 解析 `GetCodingPlanUsage` 的 `Result`（只给百分比）。
fn parse_coding_plan_tiers(id: &AccountId, result: &serde_json::Value) -> Vec<Quota> {
    let arr = result
        .get("QuotaUsage")
        .and_then(|value| value.as_array())
        .or_else(|| result.get("Usages").and_then(|value| value.as_array()))
        .or_else(|| result.get("Details").and_then(|value| value.as_array()));
    let Some(arr) = arr else {
        return Vec::new();
    };
    let mut quotas = Vec::new();
    for item in arr {
        let label = item
            .get("Level")
            .and_then(|value| value.as_str())
            .or_else(|| item.get("Type").and_then(|value| value.as_str()))
            .or_else(|| item.get("Period").and_then(|value| value.as_str()))
            .or_else(|| item.get("Label").and_then(|value| value.as_str()))
            .or_else(|| item.get("Window").and_then(|value| value.as_str()))
            .unwrap_or("");
        let Some(window) = coding_window(label) else {
            continue;
        };
        let percent = item
            .get("Percent")
            .and_then(parse_f64)
            .or_else(|| item.get("UsedPercent").and_then(parse_f64))
            .or_else(|| item.get("UsagePercent").and_then(parse_f64))
            .unwrap_or(0.0);
        let reset_at = item
            .get("ResetTimestamp")
            .or_else(|| item.get("ResetTime"))
            .and_then(parse_reset);
        quotas.push(percent_quota(id, window, percent, reset_at));
    }
    // 按真实时长排序，保证展示顺序稳定（session/5h → weekly/7d → monthly）。
    quotas.sort_by_key(|quota| quota.window.duration_seconds().unwrap_or(u64::MAX));
    quotas
}

/// 解析 `GetAFPUsage` 的 `Result`（给绝对 `Quota`/`Used`）。
fn parse_afp_tiers(id: &AccountId, result: &serde_json::Value) -> Vec<Quota> {
    let mut quotas = Vec::new();
    for (key, window) in [
        ("AFPFiveHour", QuotaWindow::FiveHour),
        ("AFPWeekly", QuotaWindow::SevenDay),
        ("AFPMonthly", QuotaWindow::Month),
    ] {
        let Some(slot) = result.get(key) else {
            continue;
        };
        let limit = slot.get("Quota").and_then(parse_f64).unwrap_or(0.0);
        if limit <= 0.0 {
            continue;
        }
        let used = slot.get("Used").and_then(parse_f64).unwrap_or(0.0);
        let pct = used / limit * 100.0;
        let reset_at = slot.get("ResetTime").and_then(parse_reset);
        quotas.push(Quota {
            provider: PROVIDER_ID.into(),
            account_id: id.clone(),
            window,
            used: used.max(0.0).round() as u64,
            limit: limit.max(0.0).round() as u64,
            reset_at,
            status: QuotaStatus::from_percent(pct.clamp(0.0, 100.0)),
            note: None,
        });
    }
    quotas
}

/// 经 AK/SK 直查控制面；返回 `None` 表示未订阅（调用方再试 arkcli 兜底）。
/// 本 Provider 是 Coding Plan（`/api/coding`），优先查 `GetCodingPlanUsage`；
/// 仅当 Coding Plan 无订阅/空时再试 `GetAFPUsage`（双订阅账号必须先看 Coding Plan，
/// 否则 Agent Plan 有数据会掩盖 Coding Plan，见 cc-switch#6499）。
async fn query_via_aksk(
    client: &reqwest::Client,
    id: &AccountId,
    access_key_id: &str,
    secret_access_key: &str,
) -> subswap_core::error::Result<Option<Vec<Quota>>> {
    use subswap_core::error::Error;
    // Coding Plan 固定 cn-beijing；与推理域名一致。
    let region = "cn-beijing";
    let mut soft_errors = Vec::new();
    let mut empty_bodies = Vec::new();
    let mut auth_errors: Vec<String> = Vec::new();
    let mut transient_errors: Vec<String> = Vec::new();

    // 1) Coding Plan 优先：session/weekly/monthly 百分比。
    match openapi_call(
        client,
        region,
        access_key_id,
        secret_access_key,
        "GetCodingPlanUsage",
    )
    .await
    {
        CallOutcome::Body(body) => {
            let result = body.get("Result").unwrap_or(&body);
            let tiers = parse_coding_plan_tiers(id, result);
            if !tiers.is_empty() {
                return Ok(Some(tiers));
            }
            empty_bodies.push("GetCodingPlanUsage empty".to_string());
        }
        CallOutcome::NotSubscribed(detail) => {
            empty_bodies.push(format!("GetCodingPlanUsage not subscribed: {detail}"));
        }
        CallOutcome::Auth(detail) => {
            auth_errors.push(format!("GetCodingPlanUsage: {detail}"));
        }
        CallOutcome::RateLimited { message, retry_at } => {
            // 限流直接停，不再打第二个 Action 添堵。
            return Err(Error::QuotaRateLimited { message, retry_at });
        }
        CallOutcome::Transient(detail) => {
            transient_errors.push(format!("GetCodingPlanUsage: {detail}"));
        }
        CallOutcome::Soft(detail) => soft_errors.push(format!("GetCodingPlanUsage: {detail}")),
    }

    // 2) AFP 兜底：双订阅/单 Agent Plan 账号仍能看到绝对值。
    match openapi_call(client, region, access_key_id, secret_access_key, "GetAFPUsage").await {
        CallOutcome::Body(body) => {
            let result = body.get("Result").unwrap_or(&body);
            let tiers = parse_afp_tiers(id, result);
            if !tiers.is_empty() {
                return Ok(Some(tiers));
            }
            empty_bodies.push("GetAFPUsage empty".to_string());
        }
        CallOutcome::NotSubscribed(detail) => {
            empty_bodies.push(format!("GetAFPUsage not subscribed: {detail}"));
        }
        CallOutcome::Auth(detail) => {
            auth_errors.push(format!("GetAFPUsage: {detail}"));
        }
        CallOutcome::RateLimited { message, retry_at } => {
            return Err(Error::QuotaRateLimited { message, retry_at });
        }
        CallOutcome::Transient(detail) => {
            transient_errors.push(format!("GetAFPUsage: {detail}"));
        }
        CallOutcome::Soft(detail) => soft_errors.push(format!("GetAFPUsage: {detail}")),
    }

    if empty_bodies.len() == 2 {
        // 签名通过但两边都无订阅：明确未订阅，调用方可再试 arkcli。
        return Ok(None);
    }
    if !auth_errors.is_empty() && soft_errors.is_empty() && transient_errors.is_empty() {
        // 两边都鉴权失败（或一边鉴权、一边空）：凭据问题，提示换 AK/SK。
        return Err(Error::QuotaFetch(format!(
            "volcengine AK/SK auth failed: {}; check AccessKey permission (ark:GetCodingPlanUsage, ark:GetAFPUsage)",
            auth_errors.join("; ")
        )));
    }
    if !transient_errors.is_empty()
        && auth_errors.is_empty()
        && soft_errors.is_empty()
        && empty_bodies.is_empty()
    {
        return Err(Error::QuotaFetch(format!(
            "volcengine {}",
            transient_errors.join("; ")
        )));
    }
    if !soft_errors.is_empty() {
        let mut details = soft_errors;
        details.extend(auth_errors);
        details.extend(transient_errors);
        return Err(Error::QuotaFetch(format!(
            "volcengine usage query failed: {}",
            details.join("; ")
        )));
    }
    if !transient_errors.is_empty() {
        return Err(Error::QuotaFetch(format!(
            "volcengine {}",
            transient_errors.join("; ")
        )));
    }
    if !auth_errors.is_empty() {
        return Err(Error::QuotaFetch(format!(
            "volcengine AK/SK auth failed: {}; check AccessKey permission (ark:GetCodingPlanUsage, ark:GetAFPUsage)",
            auth_errors.join("; ")
        )));
    }
    Ok(None)
}

/// arkcli 兜底：`arkcli usage plan --product coding-plan --format json`（SSO 登录态）。
/// 返回 `None` 表示不可用（未安装/未登录/未订阅），调用方保持空额度。
/// stdin 必须接 null，避免 arkcli 在未登录时阻塞等输入；调用方再包一层超时。
fn query_via_arkcli_blocking(id: &AccountId) -> Option<Vec<Quota>> {
    use std::process::Stdio;
    let output = std::process::Command::new("arkcli")
        .args([
            "usage",
            "plan",
            "--product",
            "coding-plan",
            "--format",
            "json",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let items = body.get("items")?.as_array()?;
    let item = items.iter().find(|item| {
        item.get("product").and_then(|value| value.as_str()) == Some("coding-plan")
            && item.get("subscribed").and_then(|value| value.as_bool()) != Some(false)
    })?;
    let periods = item.get("periods")?.as_array()?;
    if periods.is_empty() {
        return None;
    }
    let mut quotas = Vec::new();
    for period in periods {
        let label = period.get("label").and_then(|value| value.as_str()).unwrap_or("");
        let Some(window) = coding_window(label) else {
            continue;
        };
        // CodingPlan 后端只返 Percent；兼容 used/total 形态（omitempty 缺省时兜底算百分比）。
        let percent = period
            .get("percent")
            .and_then(parse_f64)
            .or_else(|| {
                let used = period.get("used").and_then(parse_f64)?;
                let total = period.get("total").and_then(parse_f64)?;
                if total > 0.0 {
                    Some(used / total * 100.0)
                } else {
                    None
                }
            })?;
        let reset_at = period.get("reset_at").and_then(parse_reset);
        quotas.push(percent_quota(id, window, percent, reset_at));
    }
    if quotas.is_empty() {
        return None;
    }
    quotas.sort_by_key(|quota: &Quota| quota.window.duration_seconds().unwrap_or(u64::MAX));
    Some(quotas)
}

/// 额度查询入口：优先 AK/SK 直查，缺凭据或未订阅时试 arkcli，最后返回空额度。
pub(super) async fn fetch_quota(
    id: AccountId,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
) -> subswap_core::error::Result<Vec<Quota>> {
    let ak = access_key_id.unwrap_or_default();
    let sk = secret_access_key.unwrap_or_default();
    if !ak.trim().is_empty() && !sk.trim().is_empty() {
        // 复用全局 Client（连接池），避免每次额度轮询重建 TLS/连接池。
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        let client = CLIENT.get_or_init(reqwest::Client::new);
        match query_via_aksk(client, &id, ak.trim(), sk.trim()).await {
            Ok(Some(quotas)) => return Ok(quotas),
            Ok(None) => {
                // 已鉴权但无订阅：仍试 arkcli（可能是另一身份的订阅）。
            }
            Err(err) => return Err(err),
        }
    }
    let id_for_cli = id.clone();
    // arkcli 可能阻塞等输入/网络：15s 超时后按不可用处理，保持空额度。
    let fallback = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::task::spawn_blocking(move || query_via_arkcli_blocking(&id_for_cli)),
    )
    .await
    .map_err(|_| {
        subswap_core::error::Error::QuotaFetch("volcengine arkcli timed out".into())
    })?
    .map_err(|err| {
        subswap_core::error::Error::Provider(format!("volcengine arkcli task failed: {err}"))
    })?;
    if let Some(quotas) = fallback {
        return Ok(quotas);
    }
    // 无凭据/未订阅/arkcli 不可用：保持空额度（默认入口显示 `custom`），不伪造数字。
    // 具体引导写在文档与 login 提示里。
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc_vector() {
        // RFC 4231 TC2：key="Jefe"，data="what do ya want for nothing?"。
        let got = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = got.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn canonical_query_has_action_and_version_only() {
        // Region 只进 scope，不进 query（与官方计费示例 / magpie / pi-ark-usage 一致）。
        assert_eq!(
            canonical_query("GetCodingPlanUsage"),
            "Action=GetCodingPlanUsage&Version=2024-01-01"
        );
        assert_eq!(
            canonical_query("GetAFPUsage"),
            "Action=GetAFPUsage&Version=2024-01-01"
        );
    }

    #[test]
    fn sign_uses_sorted_signed_headers_and_scope() {
        // 固定时间 + 固定 host，验证 SignedHeaders 排序与 scope 格式；
        // 空请求体按 `{}` 哈希（与 magpie/dsh/ark-cli 一致），不是空串哈希。
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let canonical = canonical_query("GetCodingPlanUsage");
        let (auth, x_date, body_hash) =
            sign("EXAMPLEKEYID", "EXAMPLESECRET", "cn-beijing", &canonical, b"{}", now);
        assert_eq!(x_date, "20260101T000000Z");
        assert_eq!(
            body_hash,
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert!(
            auth.contains("SignedHeaders=content-type;host;x-content-sha256;x-date"),
            "unexpected auth: {auth}"
        );
        assert!(
            auth.contains("Credential=EXAMPLEKEYID/20260101/cn-beijing/ark/request"),
            "unexpected auth: {auth}"
        );
        assert!(auth.starts_with("HMAC-SHA256 "));
    }

    #[test]
    fn parses_coding_plan_percent_windows() {
        let id = AccountId("ark-test".into());
        let result = serde_json::json!({
            "Status": "Running",
            "UpdateTimestamp": 1784278016,
            "QuotaUsage": [
                {"Level": "session", "Percent": 40.74, "ResetTimestamp": 1784278625},
                {"Level": "weekly", "Percent": 43.54, "ResetTimestamp": 1784217600},
                {"Level": "monthly", "Percent": 21.77, "ResetTimestamp": 1786723199},
            ]
        });
        let quotas = parse_coding_plan_tiers(&id, &result);
        assert_eq!(quotas.len(), 3);
        assert_eq!(quotas[0].window, QuotaWindow::FiveHour);
        assert_eq!((quotas[0].used, quotas[0].limit), (41, 100));
        assert_eq!(quotas[1].window, QuotaWindow::SevenDay);
        assert_eq!(quotas[2].window, QuotaWindow::Month);
        assert!(quotas[0].reset_at.is_some());
    }

    #[test]
    fn parses_afp_absolute_windows() {
        let id = AccountId("ark-test".into());
        let result = serde_json::json!({
            "AFPFiveHour": {"Quota": 2000.0, "Used": 1309.88, "ResetTime": 1753189560000i64},
            "AFPWeekly": {"Quota": 7000.0, "Used": 5359.78, "ResetTime": 1753536000000i64},
            "AFPDaily": {"Quota": 99999.0, "Used": 1.0, "ResetTime": 1753189560000i64},
        });
        let quotas = parse_afp_tiers(&id, &result);
        assert_eq!(quotas.len(), 2);
        assert_eq!(quotas[0].window, QuotaWindow::FiveHour);
        assert_eq!((quotas[0].used, quotas[0].limit), (1310, 2000));
    }

    #[test]
    fn reset_handles_seconds_millis_and_zero() {
        assert!(parse_reset(&serde_json::json!(0)).is_none());
        assert!(parse_reset(&serde_json::json!(-5)).is_none());
        assert!(parse_reset(&serde_json::json!(-1)).is_none());
        let from_secs = parse_reset(&serde_json::json!(1784287365)).unwrap();
        let from_millis = parse_reset(&serde_json::json!(1784287365000i64)).unwrap();
        assert_eq!(from_secs, from_millis);
        let from_rfc = parse_reset(&serde_json::json!("2026-07-17T19:22:45+08:00")).unwrap();
        // 同一时刻（UTC 11:22:45），只比较时间戳不比较显示时区。
        assert_eq!(from_rfc.timestamp(), from_secs.timestamp());
        // 数字字符串毫秒与浮点秒都兼容（AFP 回 string ms、CodingPlan 回 float 的防御）。
        let from_str_ms = parse_reset(&serde_json::json!("1784287365000")).unwrap();
        assert_eq!(from_str_ms, from_secs);
        let from_float = parse_reset(&serde_json::json!(1784287365.0)).unwrap();
        assert_eq!(from_float.timestamp(), from_secs.timestamp());
        assert!(parse_reset(&serde_json::json!("not-a-time")).is_none());
    }

    #[test]
    fn parses_arkcli_coding_plan_output() {
        let id = AccountId("ark-test".into());
        let body = serde_json::json!({
            "items": [{
                "product": "coding-plan",
                "subscribed": true,
                "periods": [
                    {"label": "session", "percent": 40.74, "reset_at": "2026-07-17T19:22:45+08:00"},
                    {"label": "weekly", "percent": 43.54, "reset_at": "2026-07-20T00:00:00+08:00"},
                    {"label": "monthly", "percent": 21.77, "reset_at": "2026-08-15T23:59:59+08:00"},
                ]
            }]
        });
        // 复用与真实 arkcli 同形的解析路径（抽出 periods 映射）。
        let items = body.get("items").unwrap().as_array().unwrap();
        let item = items.iter().find(|item| {
            item.get("product").and_then(|value| value.as_str()) == Some("coding-plan")
        }).unwrap();
        let mut quotas = Vec::new();
        for period in item.get("periods").unwrap().as_array().unwrap() {
            let window = coding_window(period.get("label").unwrap().as_str().unwrap()).unwrap();
            quotas.push(percent_quota(
                &id,
                window,
                period.get("percent").and_then(parse_f64).unwrap(),
                period.get("reset_at").and_then(parse_reset),
            ));
        }
        assert_eq!(quotas.len(), 3);
        assert_eq!(quotas[0].used, 41);
    }

    #[test]
    fn auth_and_unsubscribed_classification_is_tight() {
        // `token` 裸词不再当鉴权，避免把参数错误误判成 needs re-login。
        assert!(!is_auth_code("InvalidParameter: token limit"));
        assert!(is_auth_code("InvalidToken"));
        assert!(is_auth_code("SignatureDoesNotMatch"));
        // 通用 ResourceNotFound 按 Soft 处理，不吞实现 bug。
        assert!(!is_not_subscribed_code("ResourceNotFound"));
        assert!(is_not_subscribed_code("NotSubscribed"));
        assert!(is_not_subscribed_code("ProductNotSubscribed"));
    }

    #[test]
    fn coding_window_covers_known_labels() {
        use subswap_core::QuotaWindow;
        assert_eq!(coding_window("session"), Some(QuotaWindow::FiveHour));
        assert_eq!(coding_window("5h"), Some(QuotaWindow::FiveHour));
        assert_eq!(coding_window("weekly"), Some(QuotaWindow::SevenDay));
        assert_eq!(coding_window("monthly"), Some(QuotaWindow::Month));
        assert_eq!(coding_window("daily"), None);
    }

    /// Mock 网关端到端：CodingPlan 优先于 AFP，且请求体为 `{}`、签名头排序正确。
    /// 用标准库 TcpListener 做最小 HTTP mock，避免新增 `wiremock` 依赖；
    /// 通过 `SUBSWAP_VOLCENGINE_OPENAPI_BASE` 覆盖网关指向 127.0.0.1。
    #[tokio::test]
    async fn coding_plan_preferred_over_afp_via_mock_gateway() {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_server = seen.clone();

        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                listener.set_nonblocking(false).ok();
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // 先读到 headers 结束，再按 Content-Length 补齐 body。
                let mut header_end = None;
                while header_end.is_none() {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            if let Some(pos) = find_headers_end(&raw) {
                                header_end = Some(pos);
                                break;
                            }
                            if raw.len() > 32 * 1024 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let header_end = header_end.unwrap_or(raw.len());
                let header_text = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let content_len = header_text
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_lowercase();
                        lower.strip_prefix("content-length:").map(|v| {
                            v.trim().parse::<usize>().unwrap_or(0)
                        })
                    })
                    .unwrap_or(0);
                let mut body_len = raw.len().saturating_sub(header_end);
                while body_len < content_len {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            body_len += n;
                        }
                        Err(_) => break,
                    }
                }
                let request_text = String::from_utf8_lossy(&raw).to_string();
                seen_server.lock().unwrap().push(request_text.clone());

                let is_coding = request_text.contains("Action=GetCodingPlanUsage");
                let payload = if is_coding {
                    r#"{"ResponseMetadata":{},"Result":{"Status":"Running","QuotaUsage":[{"Level":"session","Percent":10.5,"ResetTimestamp":1784278625}]}}"#
                } else {
                    r#"{"ResponseMetadata":{},"Result":{"AFPFiveHour":{"Quota":2000,"Used":1000,"ResetTime":1784287365000}}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
                if is_coding {
                    // CodingPlan 已命中：本 Provider 不应再打 AFP，直接结束 mock。
                    break;
                }
            }
        });

        let prev = std::env::var("SUBSWAP_VOLCENGINE_OPENAPI_BASE").ok();
        unsafe {
            std::env::set_var(
                "SUBSWAP_VOLCENGINE_OPENAPI_BASE",
                format!("http://127.0.0.1:{port}"),
            );
        }
        let id = AccountId("ark-test".into());
        let client = reqwest::Client::new();
        let result = query_via_aksk(&client, &id, "AK", "SK").await;
        if let Some(value) = prev {
            unsafe {
                std::env::set_var("SUBSWAP_VOLCENGINE_OPENAPI_BASE", value);
            }
        } else {
            unsafe {
                std::env::remove_var("SUBSWAP_VOLCENGINE_OPENAPI_BASE");
            }
        }
        let _ = server.join();

        let quotas = result.unwrap().expect("coding plan should win");
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].used, 11);
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1, "AFP must not be queried when CodingPlan hits");
        let req = &requests[0];
        assert!(req.contains("Action=GetCodingPlanUsage"), "unexpected request: {req}");
        assert!(
            req.contains("SignedHeaders=content-type;host;x-content-sha256;x-date"),
            "unexpected auth order: {req}"
        );
        assert!(req.contains("content-type: application/json"), "unexpected ct: {req}");
        assert!(req.ends_with("{}"), "body must be {{}}, got tail: {}", &req[req.len().saturating_sub(32)..]);
    }

    fn find_headers_end(raw: &[u8]) -> Option<usize> {
        raw.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| pos + 4)
    }
}
