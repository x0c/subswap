//! macOS 钥匙串：Antigravity 原生 item 经 `/usr/bin/security` 读写。
//!
//! - service = `gemini`，account = `antigravity`（Go keyring 固定值，与 agy 一致）。
//! - 内容 = `go-keyring-base64:<base64 JSON>`，JSON 形如
//!   `{"token":{"access_token":..,"refresh_token":..,"expiry":..},"auth_method":..,"id_token_email":..}`。
//! - **禁止用 `keyring` crate 写**：会把 item ACL 重置成「仅 subswap」，导致 agy（同样 fork
//!   `security` 读）每次切换后反复弹授权框。沿用 Claude Provider 的 ACL 结论。

use base64::Engine;
use subswap_core::error::{Error, Result};

use crate::auth::OauthBlob;

const SERVICE: &str = "gemini";
const ACCOUNT: &str = "antigravity";
const PREFIX: &str = "go-keyring-base64:";

#[cfg(target_os = "macos")]
fn run_security(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("/usr/bin/security")
        .args(args)
        .output()
        .map_err(|e| Error::Credential(format!("run /usr/bin/security failed: {e}")))
}

/// 测试隔离：`SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH` 指向显式 keychain 文件时，
/// 所有 security 子命令只作用于它，不碰真实登录钥匙串。
#[cfg(target_os = "macos")]
fn keychain_override() -> Option<String> {
    std::env::var("SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH")
        .ok()
        .filter(|p| !p.is_empty())
}

#[cfg(target_os = "macos")]
fn run_security_on_keychain(base: &[&str]) -> Result<std::process::Output> {
    match keychain_override() {
        Some(path) => {
            let mut args: Vec<&str> = base.to_vec();
            args.push(path.as_str());
            run_security(&args)
        }
        None => run_security(base),
    }
}

/// 读原生 keychain 明文（找不到返回 Ok(None)）。
#[cfg(target_os = "macos")]
pub fn find_password() -> Result<Option<String>> {
    let output =
        run_security_on_keychain(&["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])?;
    if !output.status.success() {
        return Ok(None);
    }
    let mut raw = String::from_utf8(output.stdout)
        .map_err(|e| Error::Credential(format!("Antigravity keychain non-UTF8: {e}")))?;
    if raw.ends_with('\n') {
        raw.pop();
    }
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(raw))
}

/// 写原生 keychain：先 `-U` 原地更新，失败则删除重建（ACL 复位，与 Claude 一致）。
#[cfg(target_os = "macos")]
pub fn set_password(value: &str) -> Result<()> {
    let update = run_security_on_keychain(&[
        "add-generic-password",
        "-U",
        "-s",
        SERVICE,
        "-a",
        ACCOUNT,
        "-w",
        value,
    ])?;
    if update.status.success() {
        return Ok(());
    }
    let _ = run_security_on_keychain(&["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT])?;
    let add = run_security_on_keychain(&[
        "add-generic-password",
        "-s",
        SERVICE,
        "-a",
        ACCOUNT,
        "-w",
        value,
    ])?;
    if add.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&add.stderr);
    Err(Error::Credential(format!(
        "write Antigravity keychain failed: {stderr}"
    )))
}

/// 删除原生 item（幂等）。
#[cfg(target_os = "macos")]
pub fn delete_password() -> Result<()> {
    let _ = run_security_on_keychain(&["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT])?;
    Ok(())
}

/// 快照当前 keychain 原值（回滚用）。
#[cfg(target_os = "macos")]
pub fn snapshot() -> Result<Option<String>> {
    find_password()
}

/// 回滚到快照（None = 删到无 item）。
#[cfg(target_os = "macos")]
pub fn restore(backup: Option<String>) -> Result<()> {
    match backup {
        Some(raw) => set_password(&raw),
        None => delete_password(),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn find_password() -> Result<Option<String>> {
    Ok(None)
}
#[cfg(not(target_os = "macos"))]
pub fn set_password(_: &str) -> Result<()> {
    Err(Error::Credential(
        "Antigravity keychain write is only supported on macOS in this build".into(),
    ))
}
#[cfg(not(target_os = "macos"))]
pub fn delete_password() -> Result<()> {
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub fn snapshot() -> Result<Option<String>> {
    Ok(None)
}
#[cfg(not(target_os = "macos"))]
pub fn restore(_: Option<String>) -> Result<()> {
    Ok(())
}

/// 由 OauthBlob 合成 keychain 明文（`go-keyring-base64:` + base64）。
pub fn compose_keychain_value(blob: &OauthBlob) -> String {
    let expiry = blob
        .expires_at
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
        .unwrap_or_default();
    let v = serde_json::json!({
        "token": {
            "access_token": blob.access_token,
            "token_type": "Bearer",
            "refresh_token": blob.refresh_token,
            "expiry": expiry,
            "expires_in": 3599,
        },
        "auth_method": "consumer",
        "id_token_email": blob.email.clone().unwrap_or_default(),
    });
    let raw = serde_json::to_string(&v).unwrap_or_default();
    format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

/// 解析 keychain 明文为 OauthBlob（含 project 透传：调用方在外层合并）。
pub fn parse_keychain_value(raw: &str) -> Option<OauthBlob> {
    let b64 = raw.strip_prefix(PREFIX).unwrap_or(raw).trim();
    if b64.is_empty() {
        return None;
    }
    // 兼容裸 JSON（无前缀的旧写）。
    if b64.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(b64).ok()?;
        return crate::auth::from_native_keychain_json(&v);
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    crate::auth::from_native_keychain_json(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_parse_roundtrip() {
        let blob = OauthBlob {
            refresh_token: "1//test-refresh".into(),
            access_token: "ya29.test".into(),
            expires_at: Some(
                "2026-10-31T16:51:10+08:00"
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap(),
            ),
            email: Some("a@gmail.com".into()),
            project_id: None,
            managed_project_id: None,
        };
        let raw = compose_keychain_value(&blob);
        assert!(raw.starts_with(PREFIX));
        let back = parse_keychain_value(&raw).unwrap();
        assert_eq!(back.refresh_token, "1//test-refresh");
        assert_eq!(back.access_token, "ya29.test");
        assert_eq!(back.email.as_deref(), Some("a@gmail.com"));
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_keychain_value("").is_none());
        assert!(parse_keychain_value(PREFIX).is_none());
    }
}
