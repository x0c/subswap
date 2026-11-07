use std::process::Command;

use base64::Engine;
use chrono::Utc;
use subswap_core::{Account, AccountId, AccountRegistry, CredentialStore, FileStore};

fn isolated_subswap(tmp: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_subswap"));
    command
        .env("HOME", tmp.path().join("home"))
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", tmp.path().join("xdg-data"))
        .env("XDG_STATE_HOME", tmp.path().join("xdg-state"))
        .env("XDG_CACHE_HOME", tmp.path().join("xdg-cache"))
        .env("SUBSWAP_HOME", tmp.path().join("subswap"))
        .env("CLAUDE_CONFIG_DIR", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("KIMI_CODE_HOME", tmp.path().join("kimi"))
        .env("SUBSWAP_OPENCODE_HOME", tmp.path().join("opencode"))
        .env("SUBSWAP_COMMANDCODE_HOME", tmp.path().join("commandcode"))
        .env("SUBSWAP_ANTIGRAVITY_HOME", tmp.path().join("antigravity"))
        .env(
            "SUBSWAP_CURSOR_STATE_DB_PATH",
            tmp.path().join("cursor").join("state.vscdb"),
        )
        .env(
            "SUBSWAP_CLAUDE_KEYCHAIN_PATH",
            tmp.path().join("test.keychain-db"),
        )
        .env(
            "SUBSWAP_CURSOR_KEYCHAIN_PATH",
            tmp.path().join("test.keychain-db"),
        )
        .env(
            "SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH",
            tmp.path().join("test.keychain-db"),
        )
        .env("SUBSWAP_NO_DAEMON", "1");
    command
}

fn stores(tmp: &tempfile::TempDir) -> (FileStore, AccountRegistry) {
    let root = tmp.path().join("subswap");
    (
        FileStore::new(root.join("data").join("credentials.json")),
        AccountRegistry::new(root.join("config").join("registry.toml")),
    )
}

fn account(provider: &str, id: &str, active: bool) -> Account {
    Account {
        provider: provider.into(),
        id: AccountId(id.into()),
        label: id.into(),
        active,
        created_at: Utc::now(),
        last_used_at: None,
        priority: 100,
        extra: serde_json::Map::new(),
    }
}

fn synthetic_jwt(exp: i64) -> String {
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.{}",
        encode(serde_json::json!({"alg":"none","typ":"JWT"})),
        encode(serde_json::json!({"exp":exp})),
        "test-signature"
    )
}

#[test]
fn codex_auth_projects_only_access_token_and_expiry() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, registry) = stores(&tmp);
    let access = synthetic_jwt(4_102_444_800);
    let raw = serde_json::json!({
        "tokens": {
            "access_token": access,
            "refresh_token": "never-export-this-refresh-secret",
            "account_id": "workspace-test"
        }
    })
    .to_string();
    store
        .set("codex", "alice@example.com", "auth_json", &raw)
        .unwrap();
    registry
        .upsert(account("codex", "alice@example.com", true))
        .unwrap();

    let output = isolated_subswap(&tmp)
        .args(["auth", "codex/alice@example.com"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("never-export-this-refresh-secret"));
    assert!(!stdout.contains("refresh_token"));
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["kind"], "credential");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["id"], "alice@example.com");
    assert_eq!(value["secret"], access);
    assert_eq!(value["expires_at_ms"], 4_102_444_800_000_i64);
}

#[test]
fn volcengine_auth_reuses_environment_projection() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, registry) = stores(&tmp);
    store
        .set("volcengine", "ark-test", "api_key", "test-volcengine-key")
        .unwrap();
    let mut account = account("volcengine", "ark-test", true);
    account.extra.insert(
        "api_config".into(),
        serde_json::json!({
            "base_url": "https://ark.cn-beijing.volces.com/api/coding",
            "auth_field": "ANTHROPIC_AUTH_TOKEN",
            "model": "ark-code-latest",
            "opus_model": "ark-code-latest",
            "sonnet_model": "ark-code-latest",
            "haiku_model": "ark-code-latest",
            "subagent_model": "ark-code-latest",
            "effort_level": ""
        }),
    );
    registry.upsert(account).unwrap();

    let output = isolated_subswap(&tmp)
        .args(["auth", "volcengine/ark-test"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "environment");
    assert_eq!(value["provider"], "volcengine");
    assert_eq!(value["env"]["ANTHROPIC_AUTH_TOKEN"], "test-volcengine-key");
    assert_eq!(value["env"]["ANTHROPIC_MODEL"], "ark-code-latest");
}
