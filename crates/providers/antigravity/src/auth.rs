//! Antigravity OAuth 凭证 blob：存在 CredentialStore 的单字段 JSON。
//!
//! 形态：
//! ```json
//! {
//!   "refresh_token": "1//...",
//!   "access_token": "ya29....",
//!   "expires_at": "2026-10-31T16:51:10+08:00",
//!   "email": "user@gmail.com",
//!   "project_id": "",
//!   "managed_project_id": ""
//! }
//! ```
//! `project_id` / `managed_project_id` 为空时省略（serde skip），避免 TOML null。

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// CredentialStore 字段名。
pub const STORE_FIELD: &str = "oauth_json";

/// 可持久化的 OAuth 快照。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OauthBlob {
    pub refresh_token: String,
    pub access_token: String,
    /// RFC3339 到期时间；缺失视为已过期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_project_id: Option<String>,
}

impl OauthBlob {
    /// 序列化为 store blob。
    pub fn to_blob(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 由 store blob 解析；失败返回 None（透传策略，不 panic）。
    pub fn from_blob(blob: &str) -> Option<Self> {
        serde_json::from_str(blob).ok()
    }

    /// access 是否临近过期（含 60s 时钟偏移缓冲）。
    pub fn access_expired(&self, slack_ms: i64) -> bool {
        let Some(exp) = self.expires_at else {
            return true;
        };
        let now = Utc::now();
        let slack = chrono::Duration::milliseconds(slack_ms.max(0));
        exp <= now + slack + chrono::Duration::seconds(60)
    }

    /// 稳定主键：邮箱优先，否则 refresh 指纹。
    pub fn primary_id(&self) -> Option<String> {
        if let Some(email) = self
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(email.to_string());
        }
        if self.refresh_token.trim().is_empty() {
            return None;
        }
        Some(fingerprint(&self.refresh_token))
    }

    /// 展示标签：邮箱优先，否则 refresh 尾部指纹。
    pub fn label(&self) -> String {
        if let Some(email) = self
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return email.to_string();
        }
        let fp = fingerprint(&self.refresh_token);
        format!("ag-…{}", &fp[fp.len().saturating_sub(4)..])
    }
}

/// refresh token 指纹：`ag-` + SHA-256 前 16 hex（只存指纹，不存 secret）。
pub fn fingerprint(refresh: &str) -> String {
    let digest = Sha256::digest(refresh.as_bytes());
    let hex: String = digest.iter().take(8).fold(String::new(), |mut o, b| {
        use std::fmt::Write;
        let _ = write!(o, "{b:02x}");
        o
    });
    format!("ag-{hex}")
}

/// 由原生 keychain JSON（go-keyring-base64 解码后）构造 blob。
/// 输入形如 `{"token":{"access_token":..,"refresh_token":..,"expiry":..},"auth_method":..,"id_token_email":..}`。
pub fn from_native_keychain_json(v: &serde_json::Value) -> Option<OauthBlob> {
    let token = v.get("token")?;
    let refresh = token.get("refresh_token")?.as_str()?.trim();
    if refresh.is_empty() {
        return None;
    }
    let access = token
        .get("access_token")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_at = token
        .get("expiry")
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let email = v
        .get("id_token_email")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    Some(OauthBlob {
        refresh_token: refresh.to_string(),
        access_token: access,
        expires_at,
        email,
        project_id: None,
        managed_project_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_json_parses() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"token":{"access_token":"ya29.x","token_type":"Bearer","refresh_token":"1//abc","expiry":"2026-10-31T16:51:10.020985+08:00","expires_in":3599},"auth_method":"consumer","id_token_email":"a@gmail.com"}"#,
        )
        .unwrap();
        let b = from_native_keychain_json(&v).unwrap();
        assert_eq!(b.refresh_token, "1//abc");
        assert_eq!(b.access_token, "ya29.x");
        assert_eq!(b.email.as_deref(), Some("a@gmail.com"));
        assert_eq!(b.primary_id().as_deref(), Some("a@gmail.com"));
        assert!(b.expires_at.is_some());
    }

    #[test]
    fn fingerprint_stable() {
        assert_eq!(fingerprint("x"), fingerprint("x"));
        assert_ne!(fingerprint("x"), fingerprint("y"));
        assert!(fingerprint("x").starts_with("ag-"));
    }

    #[test]
    fn label_falls_back_to_fingerprint() {
        let b = OauthBlob {
            refresh_token: "1//abc".into(),
            access_token: "".into(),
            expires_at: None,
            email: None,
            project_id: None,
            managed_project_id: None,
        };
        assert!(b.label().starts_with("ag-…"));
        assert!(b.primary_id().unwrap().starts_with("ag-"));
    }
}
