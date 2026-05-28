//! Codex / ChatGPT 用量查询。
//!
//! 端点：
//! - `GET https://chatgpt.com/backend-api/wham/usage`
//! - Header: `Authorization: Bearer <access_token>` + `ChatGPT-Account-Id: <id>` + 浏览器 UA
//!
//! 响应字段不稳定（ChatGPT 后端会随产品调整），这里宽容解析：
//! - 任一已知字段成功 → 返回一个 Quota
//! - 全部解析失败 → 返回单条 status=Unknown 的 Quota，避免破坏调用方流程

use serde::Deserialize;
use subswap_core::error::{Error, Result};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
// 浏览器风格 UA，避免被识别为非交互客户端。
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 subswap/0.1"
);

/// 已知可能出现的字段集合。所有字段都 Optional，避免上游小幅调整导致整体失败。
#[derive(Debug, Default, Deserialize)]
pub struct WhamUsage {
    /// 已用百分比（0~100）。
    pub used_percent: Option<f64>,
    /// 上限（可能是 tokens / messages / credits）。
    pub limit: Option<u64>,
    /// 已用量。
    pub used: Option<u64>,
    /// 重置时间戳（秒或毫秒，按需识别）。
    pub resets_at: Option<i64>,
    /// 备用：reset_at 也见过这种写法。
    pub reset_at: Option<i64>,
    /// 备用：percent 字段也见过。
    pub percent: Option<f64>,
    /// 窗口长度（分钟）。Codex 当前常见值：300 / 10080。
    pub window_minutes: Option<u64>,
    /// 窗口长度（秒）。新版 wham/usage rate_limit 内常见字段。
    pub limit_window_seconds: Option<u64>,
}

/// 拉取原始响应（任意 JSON），失败返回 [`Error::QuotaFetch`]。
pub async fn fetch_usage_raw(
    access_token: &str,
    chatgpt_account_id: &str,
) -> Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("build http client: {e}")))?;

    let resp = client
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", chatgpt_account_id)
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("request wham/usage: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::QuotaFetch(format!(
            "wham/usage returned {status}: {body}"
        )));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| Error::QuotaFetch(format!("wham/usage response not JSON: {e}")))
}

/// 把宽松响应规范化为 [`WhamUsage`]。从顶层与常见嵌套位置尝试抽取字段。
pub fn normalize(raw: &serde_json::Value) -> WhamUsage {
    let mut out = WhamUsage::default();
    merge_usage(&mut out, raw);

    // 再从可能的嵌套字段（usage、quota、limits）试。
    for c in [raw.get("usage"), raw.get("quota"), raw.get("limits")]
        .into_iter()
        .flatten()
    {
        merge_usage(&mut out, c);
    }
    out
}

/// 把响应里的多个窗口规范化。新版 wham/usage 使用 `primary` / `secondary`。
pub fn normalize_all(raw: &serde_json::Value) -> Vec<WhamUsage> {
    let mut out = Vec::new();
    collect_named_windows(raw, &mut out);

    if out.is_empty() {
        out.push(normalize(raw));
    }
    out
}

/// 返回 JSON 结构摘要（只含 key/type，不含任何字段值），用于接口漂移排查。
pub fn shape_summary(raw: &serde_json::Value) -> String {
    shape(raw, 0)
}

fn shape(value: &serde_json::Value, depth: usize) -> String {
    const MAX_DEPTH: usize = 3;
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(_) => "bool".into(),
        serde_json::Value::Number(_) => "number".into(),
        serde_json::Value::String(_) => "string".into(),
        serde_json::Value::Array(items) => {
            if depth >= MAX_DEPTH {
                return format!("array(len={})", items.len());
            }
            let inner = items
                .first()
                .map(|item| shape(item, depth + 1))
                .unwrap_or_else(|| "empty".into());
            format!("array(len={}, item={inner})", items.len())
        }
        serde_json::Value::Object(map) => {
            if depth >= MAX_DEPTH {
                return format!("object(keys={})", map.len());
            }
            let parts: Vec<String> = map
                .iter()
                .take(20)
                .map(|(key, value)| format!("{key}:{}", shape(value, depth + 1)))
                .collect();
            format!("object{{{}}}", parts.join(","))
        }
    }
}

fn collect_named_windows(value: &serde_json::Value, out: &mut Vec<WhamUsage>) {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["primary", "secondary", "primary_window", "secondary_window"] {
                if let Some(value) = map.get(key) {
                    let mut usage = WhamUsage::default();
                    merge_usage(&mut usage, value);
                    if usage.has_quota_signal() {
                        out.push(usage);
                    }
                }
            }
            for child in map.values() {
                collect_named_windows(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_named_windows(child, out);
            }
        }
        _ => {}
    }
}

impl WhamUsage {
    fn has_quota_signal(&self) -> bool {
        self.used_percent.is_some()
            || self.percent.is_some()
            || self.used.is_some()
            || self.limit.is_some()
            || self.resets_at.is_some()
            || self.reset_at.is_some()
            || self.window_minutes.is_some()
            || self.limit_window_seconds.is_some()
    }
}

fn merge_usage(out: &mut WhamUsage, value: &serde_json::Value) {
    let pick_f64 =
        |v: &serde_json::Value, key: &str| -> Option<f64> { v.get(key).and_then(|x| x.as_f64()) };
    let pick_u64 =
        |v: &serde_json::Value, key: &str| -> Option<u64> { v.get(key).and_then(|x| x.as_u64()) };
    let pick_i64 =
        |v: &serde_json::Value, key: &str| -> Option<i64> { v.get(key).and_then(|x| x.as_i64()) };

    if out.used_percent.is_none() {
        out.used_percent = pick_f64(value, "used_percent").or_else(|| pick_f64(value, "percent"));
    }
    if out.percent.is_none() {
        out.percent = pick_f64(value, "percent");
    }
    if out.limit.is_none() {
        out.limit = pick_u64(value, "limit");
    }
    if out.used.is_none() {
        out.used = pick_u64(value, "used");
    }
    if out.resets_at.is_none() {
        out.resets_at = pick_i64(value, "resets_at");
    }
    if out.reset_at.is_none() {
        out.reset_at = pick_i64(value, "reset_at");
    }
    if out.window_minutes.is_none() {
        out.window_minutes = pick_u64(value, "window_minutes");
    }
    if out.limit_window_seconds.is_none() {
        out.limit_window_seconds = pick_u64(value, "limit_window_seconds");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_flat_fields() {
        let v = serde_json::json!({
            "used_percent": 42.5,
            "limit": 100,
            "used": 42,
            "resets_at": 1700000000
        });
        let n = normalize(&v);
        assert_eq!(n.used_percent, Some(42.5));
        assert_eq!(n.limit, Some(100));
        assert_eq!(n.resets_at, Some(1700000000));
    }

    #[test]
    fn normalize_nested_under_usage() {
        let v = serde_json::json!({
            "usage": { "percent": 80.0, "reset_at": 1700000999 }
        });
        let n = normalize(&v);
        assert_eq!(n.used_percent, Some(80.0));
        assert_eq!(n.reset_at, Some(1700000999));
    }

    #[test]
    fn normalize_all_primary_secondary_windows() {
        let v = serde_json::json!({
            "primary": {
                "used_percent": 1,
                "window_minutes": 300,
                "resets_at": 1779980451
            },
            "secondary": {
                "used_percent": 25,
                "window_minutes": 10080,
                "resets_at": 1780403988
            },
            "credits": { "has_credits": false },
            "plan_type": "plus"
        });
        let windows = normalize_all(&v);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, Some(1.0));
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[1].used_percent, Some(25.0));
        assert_eq!(windows[1].window_minutes, Some(10080));
    }

    #[test]
    fn normalize_all_rate_limit_windows() {
        let v = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 1,
                    "limit_window_seconds": 18000,
                    "reset_at": 1779980451
                },
                "secondary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": 604800,
                    "reset_at": 1780403988
                }
            }
        });
        let windows = normalize_all(&v);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, Some(1.0));
        assert_eq!(windows[0].limit_window_seconds, Some(18_000));
        assert_eq!(windows[1].used_percent, Some(25.0));
        assert_eq!(windows[1].limit_window_seconds, Some(604_800));
    }
}
