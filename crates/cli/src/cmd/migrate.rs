//! `subswap migrate-local`（隐藏）：把旧版本地账号数据导入 subswap。
//!
//! 一次性迁移工具，未来某个版本会移除。所有解析与原工具行为对齐，注意：
//! - 旧版 Claude 账号目录把 credentials.json 整段做了 base64，并以 `.creds-<account>-<email>.enc` 命名；
//! - 旧版 Codex 账号目录用 base64url 编码 account_key 作 auth blob 文件名（无填充）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use subswap_provider_codex::CodexCompat;

use crate::app::AppContext;

pub async fn run(ctx: &AppContext) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot locate legacy account stores")?;

    let claude = migrate_legacy_claude(ctx, &home)?;
    let codex = migrate_legacy_codex(ctx, &home)?;
    println!("migrated claude={claude} codex={codex}");
    Ok(())
}

fn migrate_legacy_claude(ctx: &AppContext, home: &Path) -> Result<usize> {
    let root = home.join(".local/share").join(["claude", "swap"].join("-"));
    let config_dir = root.join("configs");
    let cred_dir = root.join("credentials");
    if !config_dir.exists() || !cred_dir.exists() {
        return Ok(0);
    }

    let mut imported = 0;
    for entry in std::fs::read_dir(&config_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw_config = std::fs::read_to_string(&path)?;
        let config: serde_json::Value = serde_json::from_str(&raw_config)?;
        let Some(oauth_account) = config.get("oauthAccount").cloned() else {
            continue;
        };
        let Some(email) = oauth_account
            .get("emailAddress")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };

        let cred_path = cred_dir.join(format!(
            ".creds-{}.enc",
            account_number_and_email(&path, &email)
        ));
        let cred_path = if cred_path.exists() {
            cred_path
        } else {
            let fallback = cred_dir.join(format!(".creds-{email}.enc"));
            if !fallback.exists() {
                tracing::warn!(email=%email, "legacy Claude credentials file missing; skipping account");
                continue;
            }
            fallback
        };

        let encoded = std::fs::read_to_string(&cred_path)?;
        let decoded = STANDARD
            .decode(encoded.trim())
            .context("decode legacy Claude credentials")?;
        let credentials_json =
            String::from_utf8(decoded).context("legacy Claude credentials utf8")?;
        let oauth_account_json = serde_json::to_string(&oauth_account)?;
        ctx.claude.import_from_raw_json(
            &credentials_json,
            &oauth_account_json,
            Some(email.clone()),
        )?;
        imported += 1;
    }

    if let Some(active_email) = legacy_claude_active_email(&root)? {
        let id = subswap_core::AccountId(active_email);
        if let Err(e) = ctx.registry.set_active("claude", &id) {
            tracing::warn!(err=%e, "failed to preserve legacy Claude active account");
        }
    }

    Ok(imported)
}

fn legacy_claude_active_email(root: &Path) -> Result<Option<String>> {
    let sequence_path = root.join("sequence.json");
    if !sequence_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(sequence_path)?;
    let sequence: serde_json::Value = serde_json::from_str(&raw)?;
    let Some(active_number) = sequence.get("activeAccountNumber").and_then(|v| v.as_i64()) else {
        return Ok(None);
    };
    Ok(sequence
        .get("accounts")
        .and_then(|v| v.get(active_number.to_string()))
        .and_then(|v| v.get("email"))
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

fn account_number_and_email(path: &Path, email: &str) -> String {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return email.to_string();
    };
    let marker = ".claude-config-";
    if let Some(rest) = name.strip_prefix(marker) {
        if let Some(number) = rest.split('-').next() {
            return format!("{number}-{email}");
        }
    }
    email.to_string()
}

fn migrate_legacy_codex(ctx: &AppContext, home: &Path) -> Result<usize> {
    let accounts_dir = home.join(".codex/accounts");
    let registry_path = accounts_dir.join("registry.json");
    if !registry_path.exists() {
        return Ok(0);
    }

    let registry_raw = std::fs::read_to_string(&registry_path)?;
    let registry: serde_json::Value = serde_json::from_str(&registry_raw)?;
    let active_key = registry
        .get("active_account_key")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let Some(accounts) = registry.get("accounts").and_then(|v| v.as_array()) else {
        return Ok(0);
    };

    let mut imported = 0;
    for account in accounts {
        let Some(account_key) = account.get("account_key").and_then(|v| v.as_str()) else {
            continue;
        };
        let auth_name = URL_SAFE_NO_PAD.encode(account_key.as_bytes());
        let auth_path = accounts_dir.join(format!("{auth_name}.auth.json"));
        if !auth_path.exists() {
            tracing::warn!(account=%account_key, "legacy Codex auth blob missing; skipping account");
            continue;
        }
        let raw_auth_json = std::fs::read_to_string(&auth_path)?;
        let active = active_key.as_deref() == Some(account_key);
        ctx.codex
            .import_raw_with_metadata(raw_auth_json, account.clone(), active)?;
        imported += 1;
    }

    if let Some(active_key) = active_key {
        let id = subswap_core::AccountId(active_key);
        if let Err(e) = ctx.registry.set_active("codex", &id) {
            tracing::warn!(err=%e, "failed to preserve legacy Codex active account");
        }
    }

    Ok(imported)
}
