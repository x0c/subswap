//! 读写 ~/.codex/auth.json。
//!
//! 设计：subswap 不假设 auth.json 内部结构稳定（codex 自身经历了 v2→v3→v4 schema 迁移），
//! 整段当 opaque blob 处理。只解析少量元数据用于展示与去重：
//! - `account_key`（首选主键，缺失时回退用 email）
//! - `email` / `alias`（用户友好标签）
//! - `chatgpt_account_id` / `chatgpt_user_id`（额度查询时用作 header）

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use subswap_core::error::{Error, Result};

/// 从 auth.json 解析出的最小元数据。其余字段不动，整段透传。
///
/// 注意：所有字段都加 `skip_serializing_if = "Option::is_none"`。原因：metadata 最终会被序列化
/// 进 registry.toml，而 TOML 规范不支持 null/unit；如果 None 字段出现在中间 JSON 里，toml 序列化
/// 会失败（`unsupported unit type`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage_at: Option<i64>,
}

impl AuthMetadata {
    /// 主键候选：account_key > email > alias > chatgpt_account_id。
    pub fn primary_id(&self) -> Option<String> {
        self.account_key
            .as_ref()
            .or(self.email.as_ref())
            .or(self.alias.as_ref())
            .or(self.chatgpt_account_id.as_ref())
            .cloned()
    }

    /// 展示用 label：email > alias > account_name > account_key。
    pub fn label(&self) -> Option<String> {
        self.email
            .as_ref()
            .or(self.alias.as_ref())
            .or(self.account_name.as_ref())
            .or(self.account_key.as_ref())
            .cloned()
    }
}

/// 从 JSON 字符串解析元数据。解析失败返回空 [`AuthMetadata`]（透传策略，不要因为格式变了就崩）。
pub fn parse_metadata(raw: &str) -> AuthMetadata {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return AuthMetadata::default();
    };
    let mut metadata = serde_json::from_value::<AuthMetadata>(value.clone()).unwrap_or_default();
    let id_token_claims =
        string_at(&value, &["tokens", "id_token"]).and_then(|token| decode_jwt_payload(&token));

    if metadata.chatgpt_account_id.is_none() {
        metadata.chatgpt_account_id = string_at(&value, &["tokens", "account_id"])
            .or_else(|| string_at(&value, &["account_id"]));
    }
    if metadata.email.is_none() {
        metadata.email = id_token_claims
            .as_ref()
            .and_then(|claims| string_at(claims, &["email"]))
            .or_else(|| find_string_by_key(&value, "email"));
    }
    if metadata.account_name.is_none() {
        metadata.account_name = id_token_claims
            .as_ref()
            .and_then(|claims| string_at(claims, &["name"]));
    }
    if metadata.chatgpt_user_id.is_none() {
        metadata.chatgpt_user_id = id_token_claims
            .as_ref()
            .and_then(|claims| string_at(claims, &["sub"]));
    }
    if metadata.account_key.is_none() && metadata.primary_id().is_none() {
        if let Some(api_key) = string_at(&value, &["OPENAI_API_KEY"]) {
            let fingerprint = api_key_fingerprint(&api_key);
            metadata.account_key = Some(format!("api-key:{fingerprint}"));
            metadata.alias = Some(format!("Codex API key {fingerprint}"));
        }
    }
    metadata
}

fn string_at(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(str::to_owned)
}

fn find_string_by_key(value: &serde_json::Value, target: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get(target) {
                return Some(s.clone());
            }
            map.values()
                .find_map(|child| find_string_by_key(child, target))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|child| find_string_by_key(child, target)),
        _ => None,
    }
}

fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((input.len() * 3) / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }

    Some(out)
}

fn api_key_fingerprint(api_key: &str) -> String {
    // 仅用于本地去重展示；完整 API key 仍只存在 keyring 的 auth blob 中。
    let mut hash = 0xcbf29ce484222325u64;
    for byte in api_key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// 读取 auth.json 原文。
pub fn read_auth(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map_err(|e| Error::Provider(format!("read {} failed: {e}", path.display())))
}

/// 原子写 auth.json：tmp + rename + 0o600。
pub fn write_auth(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("json"),
        std::process::id()
    ));
    fs::write(&tmp, contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }

    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metadata_extracts_known_fields() {
        let raw = r#"{
            "account_key": "key-abc",
            "email": "alice@example.com",
            "alias": "alice",
            "chatgpt_account_id": "ca-xyz",
            "chatgpt_user_id": "cu-xyz",
            "account_name": "Alice",
            "plan": "Pro",
            "last_usage": {
                "primary": { "used_percent": 1, "window_minutes": 300 }
            },
            "last_usage_at": 1779962452,
            "tokens": { "access_token": "t" }
        }"#;
        let m = parse_metadata(&raw);
        assert_eq!(m.account_key.as_deref(), Some("key-abc"));
        assert_eq!(m.primary_id().as_deref(), Some("key-abc"));
        assert_eq!(m.label().as_deref(), Some("alice@example.com"));
        assert!(m.last_usage.is_some());
        assert_eq!(m.last_usage_at, Some(1779962452));
    }

    #[test]
    fn parse_metadata_extracts_api_key_account_shape() {
        let id_token = concat!(
            "ignored.",
            "eyJlbWFpbCI6ImFsaWNlQGV4YW1wbGUuY29tIiwibmFtZSI6IkFsaWNlIiwic3ViIjoidXNlci0xIn0",
            ".ignored"
        );
        let raw = r#"{
            "OPENAI_API_KEY": "sk-test-secret",
            "last_refresh": 1779962452,
            "tokens": {
                "account_id": "acct_abc",
                "access_token": "tok",
                "id_token": "__ID_TOKEN__"
            }
        }"#
        .replace("__ID_TOKEN__", id_token);
        let m = parse_metadata(&raw);
        assert_eq!(m.email.as_deref(), Some("alice@example.com"));
        assert_eq!(m.account_name.as_deref(), Some("Alice"));
        assert_eq!(m.chatgpt_user_id.as_deref(), Some("user-1"));
        assert_eq!(m.chatgpt_account_id.as_deref(), Some("acct_abc"));
        assert_eq!(m.primary_id().as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn parse_metadata_uses_api_key_fingerprint_without_account_id() {
        let raw = r#"{ "OPENAI_API_KEY": "sk-test-secret" }"#;
        let m = parse_metadata(raw);
        assert!(m.primary_id().unwrap().starts_with("api-key:"));
        assert!(m.label().unwrap().starts_with("Codex API key "));
        assert_ne!(m.primary_id().as_deref(), Some("sk-test-secret"));
    }

    #[test]
    fn parse_metadata_tolerates_garbage() {
        let m = parse_metadata("not json at all");
        assert!(m.primary_id().is_none());
        assert!(m.label().is_none());
    }

    #[test]
    fn write_then_read_auth_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let body = r#"{"account_key":"k","email":"a@b"}"#;
        write_auth(&path, body).unwrap();
        let back = read_auth(&path).unwrap();
        assert_eq!(back, body);
    }
}
