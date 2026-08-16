//! OpenCode `auth.json` 里只管理 `opencode-go` 这一项；其它供应商条目保持不动。

use sha2::{Digest, Sha256};
use subswap_provider_common::BlobMetadata;

/// `auth.json` 里 OpenCode Go 订阅的键名，与官方客户端一致。
pub const AUTH_SLOT: &str = "opencode-go";

/// 从完整 `auth.json` 抽出 `opencode-go` 条目（store 里只存这一项）。
pub fn extract_blob(live_contents: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(live_contents).ok()?;
    let entry = v.get(AUTH_SLOT)?;
    api_key_from_value(entry)?;
    serde_json::to_string(entry).ok()
}

/// 把 Go 条目合进现有 `auth.json`，保留其它供应商。
pub fn compose_live(existing_live: Option<&str>, blob: &str) -> String {
    let mut map = existing_live
        .and_then(|s| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(s).ok())
        .unwrap_or_default();
    let entry = serde_json::from_str::<serde_json::Value>(blob).unwrap_or(serde_json::Value::Null);
    map.insert(AUTH_SLOT.into(), entry);
    serde_json::Value::Object(map).to_string()
}

/// 由 API key 构造官方 `auth.json` 条目。
pub fn blob_from_key(key: &str) -> String {
    serde_json::json!({
        "type": "api",
        "key": key,
    })
    .to_string()
}

/// 从条目 blob 取 API key。
pub fn api_key_from_blob(blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    api_key_from_value(&v)
}

fn api_key_from_value(v: &serde_json::Value) -> Option<String> {
    v.get("key")
        .and_then(|k| k.as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(String::from)
}

/// 稳定主键：`go-` + key 的 SHA-256 前 16 个 hex。同一把 key 重复导入落到同一账号。
pub fn fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("go-{}", hex_prefix(&digest, 8))
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
        format!("sk-…{}", &trimmed[trimmed.len() - 4..])
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

    const KEY: &str = "sk-test-key-1234";

    #[test]
    fn extract_only_go_slot() {
        let live = r#"{"openai":{"type":"api","key":"sk-other"},"opencode-go":{"type":"api","key":"sk-test-key-1234"}}"#;
        let blob = extract_blob(live).unwrap();
        assert_eq!(api_key_from_blob(&blob).as_deref(), Some(KEY));
        assert!(!blob.contains("openai"));
    }

    #[test]
    fn extract_missing_slot_is_none() {
        assert!(extract_blob(r#"{"openai":{"type":"api","key":"x"}}"#).is_none());
        assert!(extract_blob(r#"{"opencode-go":{"type":"api","key":""}}"#).is_none());
    }

    #[test]
    fn compose_preserves_neighbors() {
        let live = r#"{"openai":{"type":"api","key":"keep-me"}}"#;
        let composed = compose_live(Some(live), &blob_from_key(KEY));
        let v: serde_json::Value = serde_json::from_str(&composed).unwrap();
        assert_eq!(v["openai"]["key"], "keep-me");
        assert_eq!(v["opencode-go"]["key"], KEY);
        assert_eq!(v["opencode-go"]["type"], "api");
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(KEY), fingerprint(KEY));
        assert_ne!(fingerprint(KEY), fingerprint("sk-other"));
        assert!(fingerprint(KEY).starts_with("go-"));
    }

    #[test]
    fn parse_metadata_uses_fingerprint() {
        let m = parse_metadata(&blob_from_key(KEY));
        assert_eq!(m.primary_id.as_deref(), Some(fingerprint(KEY).as_str()));
        assert_eq!(m.label.as_deref(), Some("sk-…1234"));
    }
}
