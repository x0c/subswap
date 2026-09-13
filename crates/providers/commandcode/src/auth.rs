//! Command Code `~/.commandcode/auth.json`：只存 API key（或 OAuth access）。
//! 官方与第三方读取形态不一，导入时统一成 store 条目，激活时写回 `{"apiKey":...}`。

use sha2::{Digest, Sha256};
use subswap_provider_common::BlobMetadata;

/// 由 API key 构造 store / 导入用 blob。
pub fn blob_from_key(key: &str) -> String {
    serde_json::json!({
        "type": "api",
        "key": key,
    })
    .to_string()
}

/// 从完整 live `auth.json` 抽出可存储的条目。
pub fn extract_blob(live_contents: &str) -> Option<String> {
    let key = api_key_from_live(live_contents)?;
    Some(blob_from_key(&key))
}

/// 把条目合进 live：统一写成官方 CLI 可读的 `apiKey` 形态。
pub fn compose_live(_existing_live: Option<&str>, blob: &str) -> String {
    let key = api_key_from_blob(blob).unwrap_or_default();
    serde_json::json!({ "apiKey": key }).to_string()
}

/// 从条目 blob 取 API key。
pub fn api_key_from_blob(blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    api_key_from_value(&v)
}

/// 解析官方 / 第三方多种 live 形态。
pub fn api_key_from_live(live_contents: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(live_contents).ok()?;
    if let Some(key) = string_field(&v, "apiKey") {
        return Some(key);
    }
    if let Some(key) = string_field(&v, "commandcode") {
        return Some(key);
    }
    for slot in ["commandcode", "command-code"] {
        if let Some(entry) = v.get(slot) {
            if let Some(key) = api_key_from_value(entry) {
                return Some(key);
            }
        }
    }
    api_key_from_value(&v)
}

fn api_key_from_value(v: &serde_json::Value) -> Option<String> {
    if let Some(key) = string_field(v, "key") {
        return Some(key);
    }
    if let Some(key) = string_field(v, "apiKey") {
        return Some(key);
    }
    // OAuth 形态：官方 CLI 偶发把 access 写进 auth.json。
    string_field(v, "access")
}

fn string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|k| k.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(String::from)
}

/// 稳定主键：`cc-` + key 的 SHA-256 前 16 hex。
pub fn fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("cc-{}", hex_prefix(&digest, 8))
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    bytes.iter().take(n).fold(String::new(), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn label_from_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.len() >= 8 {
        format!("cc-…{}", &trimmed[trimmed.len() - 4..])
    } else {
        trimmed.to_string()
    }
}

/// 从条目 blob 抽元数据。
pub fn parse_metadata(blob: &str) -> BlobMetadata {
    let Some(key) = api_key_from_blob(blob) else {
        return BlobMetadata::default();
    };
    let id = fingerprint(&key);
    BlobMetadata {
        primary_id: Some(id.clone()),
        label: Some(label_from_key(&key)),
        dedup_key: Some(id),
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "cc-test-key-1234";

    #[test]
    fn extract_api_key_form() {
        let live = r#"{"apiKey":"cc-test-key-1234"}"#;
        let blob = extract_blob(live).unwrap();
        assert_eq!(api_key_from_blob(&blob).as_deref(), Some(KEY));
    }

    #[test]
    fn extract_nested_slot() {
        let live = r#"{"commandcode":{"type":"api","key":"cc-test-key-1234"}}"#;
        assert_eq!(api_key_from_live(live).as_deref(), Some(KEY));
    }

    #[test]
    fn compose_writes_api_key() {
        let composed = compose_live(None, &blob_from_key(KEY));
        let v: serde_json::Value = serde_json::from_str(&composed).unwrap();
        assert_eq!(v["apiKey"], KEY);
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(KEY), fingerprint(KEY));
        assert_ne!(fingerprint(KEY), fingerprint("other"));
        assert!(fingerprint(KEY).starts_with("cc-"));
    }

    #[test]
    fn parse_metadata_uses_fingerprint() {
        let m = parse_metadata(&blob_from_key(KEY));
        assert_eq!(m.primary_id.as_deref(), Some(fingerprint(KEY).as_str()));
        assert_eq!(m.label.as_deref(), Some("cc-…1234"));
    }
}
