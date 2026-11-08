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

/// 破损 `config.toml` 不得污染 stdout 单 JSON 契约：warn 走 stderr。
#[test]
fn auth_stdout_stays_single_json_when_config_broken() {
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
    // 写坏用户可编辑的 config.toml，触发启动 warn。
    let config_path = tmp
        .path()
        .join("subswap")
        .join("config")
        .join("config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, "[[[broken toml").unwrap();

    let output = isolated_subswap(&tmp)
        .args(["auth", "codex/alice@example.com"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // stdout 仍然恰好一行可解析 JSON。
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "stdout must be exactly one JSON line, got:\n{stdout}"
    );
    assert!(!stdout.contains("load config failed"));
    assert!(!stdout.contains("WARN"));
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["secret"], access);
    assert!(!stdout.contains("never-export-this-refresh-secret"));
    assert!(!stdout.contains("refresh_token"));
    // warn 应出现在 stderr，而不是 stdout。
    assert!(
        stderr.contains("load config failed"),
        "broken config warn must go to stderr, got:\n{stderr}"
    );
}

/// active-live 命中只读投影：返回 live access，但不回写 store。
#[test]
fn auth_does_not_repair_store_when_live_matches_active() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, registry) = stores(&tmp);
    let store_access = synthetic_jwt(4_102_444_800);
    let live_access = synthetic_jwt(4_111_081_600);
    let store_raw = serde_json::json!({
        "email": "alice@example.com",
        "tokens": {
            "access_token": store_access,
            "refresh_token": "store-refresh-marker",
            "account_id": "workspace-test"
        }
    })
    .to_string();
    let live_raw = serde_json::json!({
        "email": "alice@example.com",
        "tokens": {
            "access_token": live_access,
            "refresh_token": "live-refresh-marker",
            "account_id": "workspace-test"
        }
    })
    .to_string();
    store
        .set("codex", "alice@example.com", "auth_json", &store_raw)
        .unwrap();
    registry
        .upsert(account("codex", "alice@example.com", true))
        .unwrap();
    let codex_home = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(codex_home.join("auth.json"), &live_raw).unwrap();
    let creds_path = tmp
        .path()
        .join("subswap")
        .join("data")
        .join("credentials.json");
    let before = std::fs::read(&creds_path).ok();

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
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["secret"], live_access);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("refresh"));
    // store 字节级不变，live 文件也不动。
    let after = std::fs::read(&creds_path).ok();
    assert_eq!(before, after, "auth must not repair store copy");
    assert_eq!(
        std::fs::read_to_string(codex_home.join("auth.json")).unwrap(),
        live_raw
    );
}

/// legacy 回退只读投影：无 store 时仍能返回 legacy access，且不落盘。
#[test]
fn auth_legacy_fallback_projects_without_backfill() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, registry) = stores(&tmp);
    let legacy_access = synthetic_jwt(4_102_444_800);
    let legacy_raw = serde_json::json!({
        "email": "legacy@example.com",
        "tokens": {
            "access_token": legacy_access,
            "refresh_token": "legacy-refresh-marker",
            "account_id": "workspace-test"
        }
    })
    .to_string();
    registry
        .upsert(account("codex", "legacy@example.com", false))
        .unwrap();
    let accounts_dir = tmp.path().join("codex").join("accounts");
    std::fs::create_dir_all(&accounts_dir).unwrap();
    std::fs::write(accounts_dir.join("legacy-test.auth.json"), &legacy_raw).unwrap();
    let creds_path = tmp
        .path()
        .join("subswap")
        .join("data")
        .join("credentials.json");
    let before = std::fs::read(&creds_path).ok();

    let output = isolated_subswap(&tmp)
        .args(["auth", "codex/legacy@example.com"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["id"], "legacy@example.com");
    assert_eq!(value["secret"], legacy_access);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("legacy-refresh-marker"));
    let after = std::fs::read(&creds_path).ok();
    assert_eq!(before, after, "legacy fallback must not backfill store");
}

/// `--renew` 新鲜凭证直接投影：不打网络/子进程，store 字节级不变。
#[test]
fn auth_renew_fresh_projects_without_side_effects() {
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
        .upsert(account("codex", "alice@example.com", false))
        .unwrap();
    let creds_path = tmp
        .path()
        .join("subswap")
        .join("data")
        .join("credentials.json");
    let before = std::fs::read(&creds_path).ok();

    let output = isolated_subswap(&tmp)
        .args(["auth", "codex/alice@example.com", "--renew"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "stdout must be exactly one JSON line");
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["kind"], "credential");
    assert_eq!(value["secret"], access);
    assert_eq!(value["expires_at_ms"], 4_102_444_800_000_i64);
    assert!(!stdout.contains("never-export-this-refresh-secret"));
    assert!(!stdout.contains("refresh_token"));
    assert_eq!(
        std::fs::read(&creds_path).ok(),
        before,
        "fresh --renew must not rewrite store"
    );
}

/// `--renew` 环境投影回退：Volcengine 长效 key 直接返回 env，不报错。
#[test]
fn auth_renew_environment_falls_back_without_network() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, registry) = stores(&tmp);
    store
        .set("volcengine", "ark-test", "api_key", "test-volcengine-key")
        .unwrap();
    let mut acct = account("volcengine", "ark-test", true);
    acct.extra.insert(
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
    registry.upsert(acct).unwrap();

    let output = isolated_subswap(&tmp)
        .args(["auth", "volcengine/ark-test", "--renew"])
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
    assert_eq!(value["env"]["ANTHROPIC_AUTH_TOKEN"], "test-volcengine-key");
}

/// `--renew` 临近过期 parked 走既有 OAuth（不可达即降级当前投影），绝不泄漏 refresh。
#[test]
fn auth_renew_stale_parked_degrades_without_refresh_leak() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, registry) = stores(&tmp);
    // 已过期 JWT（exp=1_000_000_000，约 2001 年），触发续期窗。
    let access = synthetic_jwt(1_000_000_000);
    let raw = serde_json::json!({
        "tokens": {
            "access_token": access,
            "refresh_token": "never-export-this-refresh-secret",
            "account_id": "workspace-test"
        }
    })
    .to_string();
    store
        .set("codex", "stale@example.com", "auth_json", &raw)
        .unwrap();
    registry
        .upsert(account("codex", "stale@example.com", false))
        .unwrap();

    let output = isolated_subswap(&tmp)
        .env("SUBSWAP_CODEX_OAUTH_TOKEN_URL", "http://127.0.0.1:1")
        .args(["auth", "codex/stale@example.com", "--renew"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let value: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["kind"], "credential");
    assert_eq!(value["secret"], access);
    assert!(!stdout.contains("never-export-this-refresh-secret"));
    assert!(!stdout.contains("refresh_token"));
}
