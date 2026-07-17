//! 解析 kimi-code.json：整段当 opaque blob，只从 access_token JWT 抽 user_id 等做展示。

use subswap_provider_common::BlobMetadata;

/// 解析 base64url JWT payload。
pub fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
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

/// 从 blob 抽元数据。主键 = user_id；label 缺省也用 user_id；无 email。
pub fn parse_metadata(blob: &str) -> BlobMetadata {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(blob) else {
        return BlobMetadata::default();
    };
    let claims = value
        .get("access_token")
        .and_then(|t| t.as_str())
        .and_then(decode_jwt_payload);
    let user_id = claims
        .as_ref()
        .and_then(|c| c.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut extra = serde_json::Map::new();
    if let Some(scope) = value.get("scope").and_then(|v| v.as_str()) {
        extra.insert("scope".into(), serde_json::Value::String(scope.into()));
    }

    BlobMetadata {
        primary_id: user_id.clone(),
        label: user_id,
        dedup_key: None,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // header.payload.sig；payload = {"user_id":"u-123","client_id":"c-1","scope":"kimi-code"}
    const JWT: &str = "eyJhbGciOiJFUzI1NiJ9.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEiLCJzY29wZSI6ImtpbWktY29kZSJ9.sig";

    #[test]
    fn parses_user_id_from_jwt() {
        let blob = format!(r#"{{"access_token":"{JWT}","scope":"kimi-code"}}"#);
        let m = parse_metadata(&blob);
        assert_eq!(m.primary_id.as_deref(), Some("u-123"));
        assert_eq!(m.label.as_deref(), Some("u-123"));
        assert_eq!(m.extra.get("scope").and_then(|v| v.as_str()), Some("kimi-code"));
    }

    #[test]
    fn garbage_is_empty_metadata() {
        assert!(parse_metadata("not json").primary_id.is_none());
    }
}
