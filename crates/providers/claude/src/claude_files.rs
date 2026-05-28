//! 读写 ~/.claude 下的实际激活文件。
//!
//! 核心约束：
//! - 所有写入都是「写 tmp → fsync → rename」，避免半截文件。
//! - 切换 oauthAccount 字段时，保留其他全局字段（projects、history 等）。
//! - 文件锁：在切换流程外层加 fs2 flock；本模块自身只关心序列化与原子写。

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use subswap_core::error::{Error, Result};

/// `~/.claude/.credentials.json` 的整体结构。我们只关心 `claudeAiOauth` 部分。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    pub oauth: ClaudeOauth,

    /// 其他不识别的字段透传保留，避免覆写时丢失上游新增字段。
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeOauth {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken", skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,

    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// `~/.claude.json` 中我们关心的 oauthAccount 子树。其他字段透传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthAccount {
    #[serde(rename = "emailAddress")]
    pub email_address: String,
    #[serde(rename = "accountUuid", skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(rename = "organizationUuid", skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
    #[serde(rename = "organizationName", skip_serializing_if = "Option::is_none")]
    pub organization_name: Option<String>,

    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// 读取整个 credentials.json。
pub fn read_credentials(path: &Path) -> Result<CredentialsFile> {
    let raw = fs::read_to_string(path)
        .map_err(|e| Error::Provider(format!("read {} failed: {e}", path.display())))?;
    let parsed: CredentialsFile = serde_json::from_str(&raw)?;
    Ok(parsed)
}

/// 写入 credentials.json（原子）。
pub fn write_credentials(path: &Path, value: &CredentialsFile) -> Result<()> {
    let serialized = serde_json::to_string_pretty(value)?;
    atomic_write(path, &serialized, true)?;
    Ok(())
}

/// 读取全局配置文件；不存在时返回空 Map。
pub fn read_global_config(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    Ok(serde_json::from_str(&raw)?)
}

/// 把 `oauthAccount` 字段替换/插入到全局配置，并原子写回。
pub fn write_oauth_account_into_global(path: &Path, oauth_account: &OauthAccount) -> Result<()> {
    let mut root = read_global_config(path)?;
    let obj = root.as_object_mut().ok_or_else(|| {
        Error::Provider(format!(
            "global config {} root is not a JSON object",
            path.display()
        ))
    })?;
    obj.insert("oauthAccount".into(), serde_json::to_value(oauth_account)?);
    let serialized = serde_json::to_string_pretty(&root)?;
    atomic_write(path, &serialized, false)?;
    Ok(())
}

/// 取出全局配置里的 oauthAccount（用于导入当前激活账号）。
pub fn read_oauth_account(path: &Path) -> Result<Option<OauthAccount>> {
    let root = read_global_config(path)?;
    let Some(val) = root.get("oauthAccount") else {
        return Ok(None);
    };
    let acc: OauthAccount = serde_json::from_value(val.clone())?;
    Ok(Some(acc))
}

/// 原子写：写到 `<path>.<pid>.tmp` → rename。
/// `restrict_perm` = true 时设置 0o600（仅 Unix）。
fn atomic_write(path: &Path, contents: &str, restrict_perm: bool) -> Result<()> {
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
    if restrict_perm {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = restrict_perm;

    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_roundtrip_preserves_extra_fields() {
        let raw = r#"{
            "claudeAiOauth": {
                "accessToken": "tok",
                "refreshToken": "ref",
                "expiresAt": 12345,
                "scopes": ["user:inference"],
                "subscriptionType": "pro"
            },
            "futureField": 42
        }"#;
        let parsed: CredentialsFile = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.oauth.access_token, "tok");
        assert_eq!(parsed.oauth.refresh_token.as_deref(), Some("ref"));
        // 未识别的子字段必须被透传保留。
        assert!(parsed.oauth.other.contains_key("subscriptionType"));
        assert!(parsed.other.contains_key("futureField"));
    }

    #[test]
    fn global_config_oauth_account_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude.json");
        fs::write(
            &path,
            r#"{ "projects": [{"name":"foo"}], "oauthAccount": {"emailAddress":"old@x.com"} }"#,
        )
        .unwrap();

        let new = OauthAccount {
            email_address: "new@x.com".into(),
            account_uuid: Some("uuid-x".into()),
            organization_uuid: None,
            organization_name: Some("Personal".into()),
            other: serde_json::Map::new(),
        };
        write_oauth_account_into_global(&path, &new).unwrap();

        let v = read_global_config(&path).unwrap();
        assert_eq!(v["oauthAccount"]["emailAddress"], "new@x.com");
        // 其他字段必须保留。
        assert_eq!(v["projects"][0]["name"], "foo");
    }
}
