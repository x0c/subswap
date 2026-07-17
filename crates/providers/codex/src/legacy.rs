//! Codex legacy 恢复：store/live 都拿不到凭证时，从旧版 `~/.codex/accounts/` 布局找回 auth blob；
//! 以及隔离会话物化时把真实 `config.toml` 拷进隔离目录。
//!
//! 迁移自旧版 `lib.rs` 的同名方法（当时是 `CodexProvider` 的 inherent method，签名改为接收
//! `home: &Path` / `account: &Account` 自由函数），逻辑不变，供
//! [`crate::runtime::CodexRuntime::recover_legacy`] / `materialize_extra` 调用。

use std::path::Path;

use subswap_core::Account;

use crate::codex_files::{parse_metadata, AuthMetadata};
use crate::META_CHATGPT_ACCOUNT_ID;

/// 在 store/live 都拿不到凭证时，尝试从 codex 旧版 `<home>/accounts/` 目录恢复。
pub(crate) fn recover_legacy_auth_for_account(home: &Path, account: &Account) -> Option<String> {
    let accounts_dir = home.join("accounts");
    if let Some(raw) = recover_legacy_auth_from_registry(&accounts_dir, account) {
        return Some(raw);
    }

    let entries = std::fs::read_dir(&accounts_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name.ends_with(".auth.json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let metadata = parse_metadata(&raw);
        if auth_metadata_matches_account(&metadata, account) {
            return Some(raw);
        }
    }
    None
}

fn recover_legacy_auth_from_registry(accounts_dir: &Path, account: &Account) -> Option<String> {
    let registry_raw = std::fs::read_to_string(accounts_dir.join("registry.json")).ok()?;
    let registry: serde_json::Value = serde_json::from_str(&registry_raw).ok()?;
    let accounts = registry.get("accounts")?.as_array()?;

    for legacy in accounts {
        if !legacy_account_matches_account(legacy, account) {
            continue;
        }
        let account_key = legacy.get("account_key")?.as_str()?;
        let auth_path = accounts_dir.join(format!("{}.auth.json", base64_url_no_pad(account_key)));
        if let Ok(raw) = std::fs::read_to_string(auth_path) {
            return Some(raw);
        }
    }
    None
}

fn legacy_account_matches_account(legacy: &serde_json::Value, account: &Account) -> bool {
    let legacy_email = legacy.get("email").and_then(|value| value.as_str());
    let legacy_account_key = legacy.get("account_key").and_then(|value| value.as_str());
    let legacy_chatgpt_account_id = legacy
        .get(META_CHATGPT_ACCOUNT_ID)
        .and_then(|value| value.as_str());
    let account_chatgpt_account_id = account_chatgpt_account_id(account);

    string_matches_account(legacy_email, account)
        || string_matches_account(legacy_account_key, account)
        || (legacy_chatgpt_account_id.is_some()
            && legacy_chatgpt_account_id == account_chatgpt_account_id)
}

fn auth_metadata_matches_account(metadata: &AuthMetadata, account: &Account) -> bool {
    let account_chatgpt_account_id = account_chatgpt_account_id(account);
    string_matches_account(metadata.primary_id().as_deref(), account)
        || string_matches_account(metadata.email.as_deref(), account)
        || string_matches_account(metadata.alias.as_deref(), account)
        || (metadata.chatgpt_account_id.as_deref().is_some()
            && metadata.chatgpt_account_id.as_deref() == account_chatgpt_account_id)
}

fn string_matches_account(value: Option<&str>, account: &Account) -> bool {
    let Some(value) = value else {
        return false;
    };
    value == account.id.0 || (!account.label.trim().is_empty() && value == account.label)
}

fn account_chatgpt_account_id(account: &Account) -> Option<&str> {
    account
        .extra
        .get(META_CHATGPT_ACCOUNT_ID)
        .and_then(|value| value.as_str())
}

/// base64url（无 padding）编码，与 codex 旧版 `accounts/<b64(account_key)>.auth.json` 命名一致。
pub(crate) fn base64_url_no_pad(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        }
    }
    out
}

/// best-effort 复制真实 `~/.codex/config.toml` 进隔离目录，让隔离会话沿用用户常规配置。
///
/// 与 `crates/cli/src/cmd/run.rs` 里同名函数逻辑一致，port 进 provider crate 供
/// [`crate::runtime::CodexRuntime::materialize_extra`] 这个 trait hook 使用。
pub(crate) fn copy_codex_config_best_effort(env_dir: &Path) {
    let Some(dirs) = directories::UserDirs::new() else {
        return;
    };
    let src = dirs.home_dir().join(".codex").join("config.toml");
    if src.is_file() {
        let _ = std::fs::copy(&src, env_dir.join("config.toml"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use subswap_core::AccountId;

    fn account_with_email_id(email: &str) -> Account {
        Account {
            provider: crate::PROVIDER_ID.into(),
            id: AccountId(email.into()),
            label: email.into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn active_auth_matches_registry_email_even_when_primary_id_is_account_key() {
        let metadata = AuthMetadata {
            account_key: Some("acc-stable-key".into()),
            email: Some("achesjeremy819@gmail.com".into()),
            ..AuthMetadata::default()
        };
        let account = account_with_email_id("achesjeremy819@gmail.com");

        assert!(auth_metadata_matches_account(&metadata, &account));
    }
}
