//! Codex 自定义 API 渠道：config.toml 投影、OAuth 恢复与本机密钥 helper。

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use subswap_core::error::{Error, Result};
use subswap_core::swap::SwapTarget;
use subswap_core::{Account, AccountId, AccountRegistry, BillingKind, CredentialStore};

use crate::PROVIDER_ID;

pub const META_KIND: &str = "kind";
pub const META_API_CONFIG: &str = "codex_api";
const PROVIDER_KEY: &str = "subswap";
const MARKER_FILE: &str = ".subswap-api.json";

/// 自定义 Codex 渠道的非敏感配置；API Key 单独保存在 CredentialStore 中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexApiConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default = "responses")]
    pub wire_api: String,
}

fn responses() -> String {
    "responses".into()
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiMarker {
    account_id: String,
    previous_model: Option<String>,
    previous_model_provider: Option<String>,
}

pub fn is_api_account(account: &Account) -> bool {
    account.extra.get(META_KIND).and_then(|v| v.as_str()) == Some("api")
}

pub fn config_for(account: &Account) -> Result<CodexApiConfig> {
    let value = account.extra.get(META_API_CONFIG).ok_or_else(|| {
        Error::Provider(format!(
            "Codex API account {} has no endpoint configuration",
            account.id
        ))
    })?;
    serde_json::from_value(value.clone())
        .map_err(|e| Error::Provider(format!("invalid Codex API configuration: {e}")))
}

/// 登记仅供手动选择的 Codex 自定义渠道，不修改当前客户端配置或激活账号。
pub fn add_api(
    store: &dyn CredentialStore,
    registry: &AccountRegistry,
    id: String,
    name: String,
    api_key: String,
    config: CodexApiConfig,
    billing: BillingKind,
) -> Result<Account> {
    validate_id(&id)?;
    if api_key.trim().is_empty() || api_key.chars().any(char::is_control) {
        return Err(Error::Provider(
            "API key must be non-empty and contain no control characters".into(),
        ));
    }
    if config.base_url.trim().is_empty() || config.model.trim().is_empty() {
        return Err(Error::Provider("endpoint and model cannot be empty".into()));
    }
    let existing = registry.find(PROVIDER_ID, &AccountId(id.clone()))?;
    if let Some(existing) = &existing {
        if !is_api_account(existing) {
            return Err(Error::Provider(format!(
                "Codex account {id} already exists as an OAuth account"
            )));
        }
    }
    let account_id = AccountId(id);
    let raw = serde_json::json!({ "OPENAI_API_KEY": api_key }).to_string();
    store.set(PROVIDER_ID, &account_id.0, "auth_json", &raw)?;
    let mut extra = serde_json::Map::new();
    extra.insert(META_KIND.into(), serde_json::json!("api"));
    extra.insert("manual_only".into(), serde_json::json!(true));
    extra.insert("billing".into(), serde_json::json!(billing.to_string()));
    extra.insert(
        META_API_CONFIG.into(),
        serde_json::to_value(config)
            .map_err(|e| Error::Provider(format!("serialize Codex API config failed: {e}")))?,
    );
    let account = Account {
        provider: PROVIDER_ID.into(),
        id: account_id,
        label: name,
        active: existing.as_ref().is_some_and(|a| a.active),
        created_at: existing
            .as_ref()
            .map(|a| a.created_at)
            .unwrap_or_else(Utc::now),
        last_used_at: existing.and_then(|a| a.last_used_at),
        priority: 100,
        extra,
    };
    registry.upsert(account.clone())?;
    Ok(account)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
    {
        return Err(Error::Provider(
            "id may contain only letters, numbers, '.', '@', '_' and '-'".into(),
        ));
    }
    Ok(())
}

pub fn active_account_id(home: &Path) -> Option<AccountId> {
    let marker = read_marker(home)?;
    let selected = read_config(home)
        .ok()?
        .get("model_provider")?
        .as_str()
        .map(str::to_string)?;
    if selected != PROVIDER_KEY {
        return None;
    }
    // registry 可能在 rm 后已删账号；调用方会再校验账号存在。
    Some(AccountId(marker.account_id))
}

fn read_marker(home: &Path) -> Option<ApiMarker> {
    let raw = fs::read_to_string(home.join(MARKER_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// 返回本次激活需要额外原子写入的 config/marker，以及是否还应覆盖 auth.json。
pub fn activation_targets<'a>(
    account: &Account,
    home: &Path,
) -> Result<(bool, Vec<SwapTarget<'a>>)> {
    if is_api_account(account) {
        let marker = read_marker(home);
        if marker.is_none() {
            let config_path = home.join("config.toml");
            if config_contains_managed_provider(&config_path)? {
                return Err(Error::Provider(format!(
                    "Codex provider `{PROVIDER_KEY}` is already configured; rename it before using Subswap"
                )));
            }
        }
        let (previous_model, previous_model_provider) = match marker.as_ref() {
            // 已处于自定义模式时保留首次切入前的值；None 也可能表示原先没有该键，
            // 不能回读当前自定义配置后误当成 OAuth 恢复值。
            Some(marker) => (
                marker.previous_model.clone(),
                marker.previous_model_provider.clone(),
            ),
            None => {
                let current = read_config(home).ok();
                (
                    current
                        .as_ref()
                        .and_then(|d| d.get("model")?.as_str().map(str::to_string)),
                    current
                        .as_ref()
                        .and_then(|d| d.get("model_provider")?.as_str().map(str::to_string)),
                )
            }
        };
        let next_marker = ApiMarker {
            account_id: account.id.0.clone(),
            previous_model,
            previous_model_provider,
        };
        let config_text = render_api_config(account, home)?;
        let marker_text = serde_json::to_string_pretty(&next_marker)
            .map_err(|e| Error::Provider(format!("serialize Codex API marker failed: {e}")))?;
        Ok((
            false,
            vec![
                text_target("codex_config", home.join("config.toml"), config_text),
                text_target("codex_api_marker", home.join(MARKER_FILE), marker_text),
            ],
        ))
    } else if read_marker(home).is_some() {
        Ok((true, restore_targets(home)?))
    } else {
        Ok((true, Vec::new()))
    }
}

fn config_contains_managed_provider(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let doc = fs::read_to_string(path)?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| Error::Provider(format!("parse {} failed: {e}", path.display())))?;
    Ok(doc
        .get("model_providers")
        .and_then(|v| v.get(PROVIDER_KEY))
        .is_some())
}

fn read_config(home: &Path) -> Result<toml::Value> {
    let raw = fs::read_to_string(home.join("config.toml"))?;
    toml::from_str(&raw).map_err(Into::into)
}

fn render_api_config(account: &Account, home: &Path) -> Result<String> {
    let config = config_for(account)?;
    if config.wire_api != "responses" {
        return Err(Error::Provider(
            "Codex custom API currently requires the Responses wire API".into(),
        ));
    }
    let path = home.join("config.toml");
    let mut doc = if path.exists() {
        fs::read_to_string(&path)?
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| Error::Provider(format!("parse {} failed: {e}", path.display())))?
    } else {
        toml_edit::DocumentMut::new()
    };
    // TOML 顶层标量必须位于首个 table 前；toml_edit 会负责序列化合法结构。
    doc["model"] = toml_edit::value(config.model);
    doc["model_provider"] = toml_edit::value(PROVIDER_KEY);
    doc["model_providers"][PROVIDER_KEY]["name"] = toml_edit::value(account.label.clone());
    doc["model_providers"][PROVIDER_KEY]["base_url"] =
        toml_edit::value(config.base_url.trim_end_matches('/'));
    doc["model_providers"][PROVIDER_KEY]["wire_api"] = toml_edit::value("responses");
    doc["model_providers"][PROVIDER_KEY]["auth"]["command"] =
        toml_edit::value(helper_executable()?.to_string_lossy().to_string());
    doc["model_providers"][PROVIDER_KEY]["auth"]["args"] = toml_edit::value(
        vec![
            "auth".to_string(),
            format!("{PROVIDER_ID}/{}", account.id.0),
            "--raw".to_string(),
        ]
        .into_iter()
        .collect::<toml_edit::Array>(),
    );
    doc["model_providers"][PROVIDER_KEY]["auth"]["timeout_ms"] = toml_edit::value(5_000);
    doc["model_providers"][PROVIDER_KEY]["auth"]["refresh_interval_ms"] = toml_edit::value(0);
    Ok(doc.to_string())
}

fn helper_executable() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SUBSWAP_CLI_PATH") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe() {
        if current.file_stem().and_then(|v| v.to_str()) == Some("subswap") {
            return Ok(current);
        }
        let sibling = current.with_file_name(if cfg!(windows) {
            "subswap.exe"
        } else {
            "subswap"
        });
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(if cfg!(windows) {
            "subswap.exe"
        } else {
            "subswap"
        });
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Error::Provider(
        "cannot locate the subswap executable for Codex authentication; set SUBSWAP_CLI_PATH"
            .into(),
    ))
}

fn restored_config(home: &Path) -> Result<String> {
    let marker = read_marker(home).ok_or_else(|| {
        Error::Provider("Codex API mode marker is missing; cannot restore configuration".into())
    })?;
    let path = home.join("config.toml");
    let mut doc = if path.exists() {
        fs::read_to_string(&path)?
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| Error::Provider(format!("parse {} failed: {e}", path.display())))?
    } else {
        toml_edit::DocumentMut::new()
    };
    restore_key(&mut doc, "model", marker.previous_model);
    restore_key(&mut doc, "model_provider", marker.previous_model_provider);
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(|v| v.as_table_mut())
    {
        providers.remove(PROVIDER_KEY);
        if providers.is_empty() {
            doc.remove("model_providers");
        }
    }
    Ok(doc.to_string())
}

fn restore_targets<'a>(home: &Path) -> Result<Vec<SwapTarget<'a>>> {
    Ok(vec![
        text_target(
            "codex_config",
            home.join("config.toml"),
            restored_config(home)?,
        ),
        SwapTarget {
            snapshot_name: "codex_api_marker",
            live_path: home.join(MARKER_FILE),
            writer: Box::new(|path| match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }),
        },
    ])
}

fn restore_key(doc: &mut toml_edit::DocumentMut, key: &str, value: Option<String>) {
    match value {
        Some(value) => {
            doc[key] = toml_edit::value(value);
        }
        None => {
            doc.remove(key);
        }
    }
}

fn text_target<'a>(name: &'a str, path: PathBuf, text: String) -> SwapTarget<'a> {
    SwapTarget {
        snapshot_name: name,
        live_path: path,
        writer: Box::new(move |path| {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
            fs::write(&tmp, text)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
            }
            fs::rename(tmp, path)?;
            Ok(())
        }),
    }
}

pub fn disconnect_active(home: &Path, account: &Account) -> Result<bool> {
    if !is_api_account(account) || active_account_id(home).as_ref() != Some(&account.id) {
        return Ok(false);
    }
    subswap_core::swap::swap_with_snapshot_prepared(
        PROVIDER_ID,
        home,
        || {
            if active_account_id(home).as_ref() != Some(&account.id) {
                return Ok(Vec::new());
            }
            restore_targets(home)
        },
        || Ok(()),
    )?;
    Ok(true)
}

/// 隔离运行同样选择该自定义渠道；auth.json 由共享引擎物化，helper 仍从 Subswap 只读取 Key。
pub fn materialize_isolated(account: &Account, source_home: &Path, env_dir: &Path) -> Result<()> {
    if !is_api_account(account) {
        crate::legacy::copy_codex_config_best_effort(env_dir);
        // 全局自定义 API 不能污染 OAuth 的隔离会话：只在私有目录恢复原始模型和 Provider。
        if source_home.join(MARKER_FILE).exists() {
            fs::write(env_dir.join("config.toml"), restored_config(source_home)?)?;
        }
        return Ok(());
    }
    let source = source_home.join("config.toml");
    if source.exists() {
        fs::copy(source, env_dir.join("config.toml"))?;
    }
    let rendered = render_api_config(account, env_dir)?;
    fs::write(env_dir.join("config.toml"), rendered)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> Account {
        let mut extra = serde_json::Map::new();
        extra.insert(META_KIND.into(), serde_json::json!("api"));
        extra.insert(
            META_API_CONFIG.into(),
            serde_json::json!({"base_url":"https://gateway.example/v1","model":"gpt-test","wire_api":"responses"}),
        );
        Account {
            provider: PROVIDER_ID.into(),
            id: AccountId(id.into()),
            label: "Test gateway".into(),
            active: false,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra,
        }
    }

    #[test]
    fn api_activation_preserves_oauth_model_and_provider_for_restore() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SUBSWAP_CLI_PATH", "/tmp/subswap");
        fs::write(
            tmp.path().join("config.toml"),
            "model = \"gpt-original\"\nmodel_provider = \"openai\"\n\n[features]\nfoo = true\n",
        )
        .unwrap();
        let (write_auth, _) = activation_targets(&account("one"), tmp.path()).unwrap();
        assert!(!write_auth);
        let rendered = render_api_config(&account("one"), tmp.path()).unwrap();
        assert!(rendered.contains("model_provider = \"subswap\""));
        assert!(rendered.contains("base_url = \"https://gateway.example/v1\""));
        assert!(rendered.contains("[features]"));
        assert!(rendered.contains("--raw"));
        let (_, targets) = activation_targets(&account("one"), tmp.path()).unwrap();
        for target in targets {
            (target.writer)(&target.live_path).unwrap();
        }
        let (_, targets) = activation_targets(&account("two"), tmp.path()).unwrap();
        for target in targets {
            (target.writer)(&target.live_path).unwrap();
        }
        assert_eq!(active_account_id(tmp.path()), Some(AccountId("two".into())));
        let restored = restored_config(tmp.path()).unwrap();
        assert!(restored.contains("model = \"gpt-original\""));
        assert!(restored.contains("model_provider = \"openai\""));
        assert!(!restored.contains("[model_providers.subswap]"));

        let isolated = tempfile::tempdir().unwrap();
        let mut oauth = account("oauth");
        oauth.extra.clear();
        materialize_isolated(&oauth, tmp.path(), isolated.path()).unwrap();
        assert_eq!(
            fs::read_to_string(isolated.path().join("config.toml")).unwrap(),
            restored
        );
        assert_eq!(active_account_id(tmp.path()), Some(AccountId("two".into())));

        // 首次切入前不存在的键，在 A→B→OAuth 后必须仍不存在。
        let no_defaults = tempfile::tempdir().unwrap();
        fs::write(
            no_defaults.path().join("config.toml"),
            "[features]\nfoo = true\n",
        )
        .unwrap();
        for id in ["one", "two"] {
            let (_, targets) = activation_targets(&account(id), no_defaults.path()).unwrap();
            for target in targets {
                (target.writer)(&target.live_path).unwrap();
            }
        }
        let restored = restored_config(no_defaults.path()).unwrap();
        let doc: toml::Value = restored.parse().unwrap();
        assert!(doc.get("model").is_none());
        assert!(doc.get("model_provider").is_none());
        assert_eq!(doc["features"]["foo"].as_bool(), Some(true));
        std::env::remove_var("SUBSWAP_CLI_PATH");
    }
}
