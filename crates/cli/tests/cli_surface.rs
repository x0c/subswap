use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::{fs, path::Path};

fn subswap() -> Command {
    Command::new(env!("CARGO_BIN_EXE_subswap"))
}

fn isolated_subswap(tmp: &tempfile::TempDir) -> Command {
    let mut command = subswap();
    command
        .env("HOME", tmp.path().join("home"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        // Windows 的系统目录解析不接受 XDG 覆盖；统一根目录确保三端都不触碰真实用户状态，
        // 且每个 TempDir 天然隔离并行测试。
        .env("SUBSWAP_HOME", tmp.path().join("subswap"))
        .env("CLAUDE_CONFIG_DIR", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        // 隔离测试专用一次性目录，绝不碰真实 `~/.kimi-code`。
        .env("KIMI_CODE_HOME", tmp.path().join("kimi"))
        // 隔离测试专用一次性目录，绝不碰真实 `~/.local/share/opencode/auth.json`。
        .env("SUBSWAP_OPENCODE_HOME", tmp.path().join("opencode"))
        // Cursor 的平台默认路径不受 HOME/SUBSWAP_HOME 统一覆盖，必须显式指向临时目录。
        .env(
            "SUBSWAP_CURSOR_STATE_DB_PATH",
            tmp.path().join("cursor").join("state.vscdb"),
        )
        // macOS：把 Claude Code / Cursor 命令行钥匙串读写重定向到一次性 keychain，绝不碰用户真实登录钥匙串
        // （否则集成测试会弹授权框并污染本机凭证）。
        .env("SUBSWAP_CLAUDE_KEYCHAIN_PATH", test_keychain_path(tmp))
        .env("SUBSWAP_CURSOR_KEYCHAIN_PATH", test_keychain_path(tmp))
        .env("SUBSWAP_NO_DAEMON", "1");
    command
}

/// 一次性测试钥匙串文件路径（随 tmp 目录一起销毁）。
fn test_keychain_path(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("test.keychain-db")
}

/// macOS：创建供测试使用的一次性 keychain。非 macOS 为 no-op（凭证走 FileStore）。
fn setup_test_keychain(tmp: &tempfile::TempDir) {
    if cfg!(target_os = "macos") {
        let path = test_keychain_path(tmp);
        let _ = Command::new("/usr/bin/security")
            .args(["create-keychain", "-p", ""])
            .arg(&path)
            .status();
    }
}

/// macOS：删除测试 keychain。文件本身随 tmp 销毁，这里只是保险清理。
fn teardown_test_keychain(tmp: &tempfile::TempDir) {
    if cfg!(target_os = "macos") {
        let path = test_keychain_path(tmp);
        let _ = Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(&path)
            .status();
    }
}

#[cfg(target_os = "macos")]
fn write_test_keychain_credentials(tmp: &tempfile::TempDir, credentials: &str) {
    let status = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            "Claude Code-credentials",
            "-a",
            std::env::var("USER").unwrap().as_str(),
            "-w",
            credentials,
        ])
        .arg(test_keychain_path(tmp))
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(target_os = "macos")]
fn read_test_keychain_credentials(tmp: &tempfile::TempDir) -> String {
    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-a",
            std::env::var("USER").unwrap().as_str(),
            "-w",
        ])
        .arg(test_keychain_path(tmp))
        .output()
        .unwrap();
    assert_success(output).trim().to_owned()
}

fn assert_success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn app_config_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("subswap").join("config")
}

fn app_data_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("subswap").join("data")
}

#[test]
fn help_shows_only_current_commands() {
    let output = subswap().arg("--help").output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: subswap"));
    assert!(stdout.contains("login"));
    assert!(stdout.contains("add-api"));
    assert!(stdout.contains("swap"));
    assert!(stdout.contains("rm"));
    assert!(stdout.contains("doctor"));

    for removed in [
        "  add ",
        "  list ",
        "  quota ",
        "  refresh ",
        "  auto ",
        "  daemon ",
    ] {
        assert!(
            !stdout.contains(removed),
            "help should not expose removed command {removed:?}:\n{stdout}"
        );
    }
}

#[test]
fn add_api_help_exposes_exactly_three_model_roles() {
    let output = subswap().args(["add-api", "--help"]).output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    for flag in ["--opus-model", "--sonnet-model", "--haiku-model"] {
        assert!(stdout.contains(flag), "missing {flag} in:\n{stdout}");
    }
    for removed in ["--model", "--subagent-model"] {
        assert!(
            !stdout.contains(removed),
            "add-api help must not expose {removed}:\n{stdout}"
        );
    }
}

#[test]
fn add_api_accepts_legacy_model_as_the_only_model_flag() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    let claude = tmp.path().join("claude");

    let stdout = assert_success(
        isolated_subswap(&tmp)
            .args([
                "add-api",
                "--preset",
                "custom",
                "--id",
                "legacy",
                "--name",
                "Legacy",
                "--endpoint",
                "https://example.com",
                "--api-key",
                "secret",
                "--auth",
                "bearer",
                "--model",
                "legacy-main",
                "--yes",
            ])
            .output()
            .unwrap(),
    );
    assert!(stdout.contains("added → claude/legacy"), "{stdout}");

    assert_success(
        isolated_subswap(&tmp)
            .args(["swap", "legacy"])
            .output()
            .unwrap(),
    );
    let active: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(active["env"]["ANTHROPIC_MODEL"], "legacy-main");
    assert_eq!(active["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "legacy-main");
    assert_eq!(
        active["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "legacy-main"
    );
    assert_eq!(
        active["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
        "legacy-main"
    );
    assert_eq!(active["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "legacy-main");

    teardown_test_keychain(&tmp);
}

#[test]
fn default_with_empty_home_is_quiet_and_does_not_probe_real_accounts() {
    let tmp = tempfile::tempdir().unwrap();
    let output = isolated_subswap(&tmp).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "No accounts. Sign in to a supported client, then run `subswap login <provider>`."
    );
    assert!(
        !stdout.contains("[degraded]"),
        "empty registry should stay quiet:\n{stdout}"
    );
}

#[test]
fn deepseek_api_can_be_added_manually_activated_and_switched_back_to_oauth() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    let claude = tmp.path().join("claude");
    let registry = app_config_dir(&tmp).join("registry.toml");
    let credentials = app_data_dir(&tmp).join("credentials.json");

    write(
        &registry,
        r#"[[accounts]]
provider = "claude"
id = "oauth@example.com"
label = "OAuth"
active = true
created_at = "2026-06-09T00:00:00Z"
priority = 100

[accounts.extra.oauth_account]
emailAddress = "oauth@example.com"
"#,
    );
    write(
        &credentials,
        r#"{"claude:oauth@example.com:credentials_json":"{\"claudeAiOauth\":{\"accessToken\":\"oauth-token\"}}"}"#,
    );
    write(
        &claude.join("settings.json"),
        r#"{"env":{"ANTHROPIC_MODEL":"old-model","KEEP":"yes"},"permissions":{"allow":["Read"]}}"#,
    );

    let stdout = assert_success(
        isolated_subswap(&tmp)
            .args([
                "add-api",
                "--preset",
                "deepseek",
                "--api-key",
                "deepseek-secret",
                "--yes",
            ])
            .output()
            .unwrap(),
    );
    assert!(stdout.contains("added → claude/deepseek"), "{stdout}");

    // 模拟同一 Claude 账号仍有隔离会话在运行；手动切换仍必须可用。
    fs::create_dir_all(
        app_data_dir(&tmp)
            .join("envs")
            .join("claude")
            .join("deepseek")
            .join("0"),
    )
    .unwrap();
    assert_success(
        isolated_subswap(&tmp)
            .args(["swap", "deepseek"])
            .output()
            .unwrap(),
    );
    let active: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        active["env"]["ANTHROPIC_BASE_URL"],
        "https://api.deepseek.com/anthropic"
    );
    assert_eq!(active["env"]["ANTHROPIC_AUTH_TOKEN"], "deepseek-secret");
    assert_eq!(active["env"]["KEEP"], "yes");
    assert!(claude.join(".subswap-api.json").exists());

    let remove_active = isolated_subswap(&tmp)
        .args(["rm", "deepseek"])
        .output()
        .unwrap();
    assert!(!remove_active.status.success());
    assert!(
        String::from_utf8_lossy(&remove_active.stderr).contains("swap away first"),
        "{}",
        String::from_utf8_lossy(&remove_active.stderr)
    );

    // API active 时运行默认入口，manual_only 语义必须阻止自动切回 OAuth。
    write(
        &app_config_dir(&tmp).join("config.toml"),
        "[quota]\nfetch_timeout_ms = 1\nfetch_retries = 0\n",
    );
    assert_success(isolated_subswap(&tmp).output().unwrap());
    assert!(claude.join(".subswap-api.json").exists());

    assert_success(
        isolated_subswap(&tmp)
            .args(["swap", "oauth@example.com"])
            .output()
            .unwrap(),
    );
    let restored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(restored["env"]["ANTHROPIC_MODEL"], "old-model");
    assert_eq!(restored["env"]["KEEP"], "yes");
    assert!(restored["env"].get("ANTHROPIC_BASE_URL").is_none());
    assert!(restored["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert_eq!(restored["permissions"]["allow"][0], "Read");
    assert!(!claude.join(".subswap-api.json").exists());

    teardown_test_keychain(&tmp);
}

#[cfg(target_os = "macos")]
#[test]
fn swapping_to_active_claude_account_preserves_live_keychain_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    let claude = tmp.path().join("claude");
    let registry = app_config_dir(&tmp).join("registry.toml");
    let credentials = app_data_dir(&tmp).join("credentials.json");
    let stale = r#"{"claudeAiOauth":{"accessToken":"stale-access","refreshToken":"stale-refresh","expiresAt":4102444800000}}"#;
    let live = r#"{"claudeAiOauth":{"accessToken":"live-access","refreshToken":"live-refresh","expiresAt":4102444800000}}"#;

    write(
        &registry,
        r#"[[accounts]]
provider = "claude"
id = "active@example.com"
label = "Active"
active = true
created_at = "2026-06-12T00:00:00Z"
priority = 100

[accounts.extra.oauth_account]
emailAddress = "active@example.com"
"#,
    );
    write(
        &credentials,
        &serde_json::json!({
            "claude:active@example.com:credentials_json": stale
        })
        .to_string(),
    );
    write(
        &claude.join(".claude.json"),
        r#"{"oauthAccount":{"emailAddress":"active@example.com"}}"#,
    );
    write(&claude.join(".credentials.json"), stale);
    write_test_keychain_credentials(&tmp, live);

    assert_success(
        isolated_subswap(&tmp)
            .args(["swap", "active@example.com"])
            .output()
            .unwrap(),
    );

    assert_eq!(read_test_keychain_credentials(&tmp), live);
    let stored: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(credentials).unwrap()).unwrap();
    assert_eq!(
        stored["claude:active@example.com:credentials_json"],
        serde_json::Value::String(live.into())
    );
    assert_eq!(
        fs::read_to_string(claude.join(".credentials.json")).unwrap(),
        stale
    );

    teardown_test_keychain(&tmp);
}

// --- `subswap run kimi` 隔离运行：注册表驱动 dispatch（Task 11） ---

#[test]
fn run_kimi_unknown_account_reports_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    // 命令面已注册 "kimi" provider（normalize_provider 接受），但账号不存在时应报「账号不存在」，
    // 而不是「unknown provider」或 clap 层面的用法错误——证明 `run kimi` 已完整接入命令面。
    let output = isolated_subswap(&tmp)
        .args(["run", "kimi", "ghost@example.com"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("account not found"),
        "expected account-not-found error, got: {stderr}"
    );
}

#[test]
fn run_kimi_materializes_isolated_credentials_via_generic_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = app_config_dir(&tmp).join("registry.toml");
    let credentials = app_data_dir(&tmp).join("credentials.json");

    write(
        &registry,
        r#"[[accounts]]
provider = "kimi"
id = "kimi-user"
label = "Kimi User"
active = false
created_at = "2026-07-01T00:00:00Z"
priority = 100
"#,
    );
    // KimiRuntime 用默认 store_field "blob"；key 格式 "{provider}:{account}:{field}"。
    write(
        &credentials,
        r#"{"kimi:kimi-user:blob":"{\"user_id\":\"kimi-user\",\"access_token\":\"AT\"}"}"#,
    );

    let output = isolated_subswap(&tmp)
        .args(["run", "kimi", "kimi-user"])
        .output()
        .unwrap();

    // 本机大概率没有 `kimi` 原生 CLI，预期最终在 spawn 阶段失败；但这必须发生在
    // materialize 成功、且已经通过 IsolatedProvider 算出 KIMI_CODE_HOME/native_cli 之后，
    // 证明 run.rs 的注册表驱动 dispatch（materialize/env_vars/native_cli 均查 ctx.isolated）
    // 对 kimi 完整生效，而不是像重构前那样落进 "isolation not supported for provider kimi"。
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("isolated KIMI_CODE_HOME="),
        "materialize/env_vars should have resolved KIMI_CODE_HOME via IsolatedProvider; stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("isolation not supported for provider kimi"),
        "kimi must be dispatched through ctx.isolated, not fall through to the unsupported branch: {stderr}"
    );
    if !output.status.success() {
        assert!(
            stderr.contains("failed to start `kimi`"),
            "expected native_cli dispatch to attempt spawning `kimi`; stderr: {stderr}"
        );
    }
}

#[test]
fn rm_leaves_no_tombstone_signed_in_account_reappears_on_next_run() {
    // 2026-08-14 引入的删除墓碑会让「删过、但客户端还登录着」的账号永久消失且零提示——
    // 这正是用户反馈「Cursor 又不见了」的真正根因。墓碑已整体移除：
    // 只要客户端仍登录着，下次默认入口必须能把账号收回来；`rm` 也要如实说明这一点。
    let tmp = tempfile::tempdir().unwrap();
    let registry = app_config_dir(&tmp).join("registry.toml");
    let credentials = app_data_dir(&tmp).join("credentials.json");
    // Kimi 官方客户端真正落盘的「当前登录」凭证，独立于 subswap 自己的可切换副本。
    let live_cred = tmp
        .path()
        .join("kimi")
        .join("credentials")
        .join("kimi-code.json");

    write(
        &registry,
        r#"[[accounts]]
provider = "kimi"
id = "kimi-user"
label = "Kimi User"
active = true
created_at = "2026-07-01T00:00:00Z"
priority = 100
"#,
    );
    write(
        &credentials,
        r#"{"kimi:kimi-user:blob":"{\"user_id\":\"kimi-user\",\"access_token\":\"AT\"}"}"#,
    );
    // header.{"user_id":"kimi-user"}.sig,让 parse_metadata 能从 JWT 里解出 user_id。
    write(
        &live_cred,
        r#"{"access_token":"header.eyJ1c2VyX2lkIjogImtpbWktdXNlciJ9.sig"}"#,
    );

    let rm_stdout = assert_success(
        isolated_subswap(&tmp)
            .args(["rm", "kimi-user"])
            .output()
            .unwrap(),
    );
    assert!(rm_stdout.contains("removed kimi/kimi-user"), "{rm_stdout}");
    assert!(
        rm_stdout.contains("still signed in as this account")
            && rm_stdout.contains("picked up again on the next run"),
        "rm must say the account will come back, not that it's gone for good: {rm_stdout}"
    );

    let after_rm = fs::read_to_string(&registry).unwrap();
    assert!(
        !after_rm.contains("kimi-user"),
        "account must actually be removed from the registry: {after_rm}"
    );

    write(
        &app_config_dir(&tmp).join("config.toml"),
        "[quota]\nfetch_timeout_ms = 1\nfetch_retries = 0\n",
    );
    let default_stdout = assert_success(isolated_subswap(&tmp).output().unwrap());
    assert!(
        default_stdout.contains("kimi-user"),
        "no tombstone should block re-import on the very next run: {default_stdout}"
    );
}

#[test]
fn login_opencode_imports_go_key_and_preserves_other_providers() {
    let tmp = tempfile::tempdir().unwrap();
    let auth = tmp.path().join("opencode").join("auth.json");
    write(
        &auth,
        r#"{"openai":{"type":"api","key":"sk-keep"},"opencode-go":{"type":"api","key":"sk-test-key-1234"}}"#,
    );

    let stdout = assert_success(
        isolated_subswap(&tmp)
            .args(["login", "opencode"])
            .output()
            .unwrap(),
    );
    assert!(
        stdout.contains("login → opencode/go-"),
        "expected imported OpenCode Go account, got: {stdout}"
    );

    let live: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(live["openai"]["key"], "sk-keep");
    assert_eq!(live["opencode-go"]["key"], "sk-test-key-1234");
}

#[test]
fn run_opencode_unknown_account_reports_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let output = isolated_subswap(&tmp)
        .args(["run", "opencode", "ghost-go"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("account not found"),
        "expected account-not-found error, got: {stderr}"
    );
}

#[test]
fn run_opencode_materializes_isolated_auth_via_generic_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let stdout = assert_success(
        isolated_subswap(&tmp)
            .args(["login", "opencode", "--", "sk-test-key-1234"])
            .output()
            .unwrap(),
    );
    let id = stdout
        .trim()
        .strip_prefix("login → opencode/")
        .unwrap_or_else(|| panic!("unexpected login output: {stdout}"));

    let env_out = assert_success(isolated_subswap(&tmp).args(["env", id]).output().unwrap());
    assert!(
        env_out.contains("XDG_DATA_HOME="),
        "env should export XDG_DATA_HOME: {env_out}"
    );
    assert!(
        env_out.contains("OPENCODE_AUTH_CONTENT="),
        "env should export OPENCODE_AUTH_CONTENT: {env_out}"
    );
    assert!(
        env_out.contains("opencode-go"),
        "OPENCODE_AUTH_CONTENT should contain the Go slot: {env_out}"
    );

    let output = isolated_subswap(&tmp)
        .args(["run", "opencode", id, "--", "--version"])
        .output()
        .unwrap();
    let run_stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        run_stdout.contains("isolated XDG_DATA_HOME="),
        "materialize/env_vars should have resolved XDG_DATA_HOME via IsolatedProvider; stdout: {run_stdout}, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("isolation not supported for provider opencode"),
        "opencode must be dispatched through ctx.isolated: {stderr}"
    );
}

const OPENCODE_EXHAUSTED_KEY: &str = "sk-test-exhausted-0000";
const OPENCODE_HEALTHY_KEY: &str = "sk-test-healthy-9999";
const OPENCODE_DEAD_KEY: &str = "sk-test-deadkey-1111";
const OPENCODE_EXHAUSTED_USAGE: &str = r#"{"usage":{"rolling":{"status":"rate-limited","percent":100},"weekly":{"status":"ok","percent":3},"monthly":{"status":"ok","percent":1}}}"#;
const OPENCODE_HEALTHY_USAGE: &str = r#"{"usage":{"rolling":{"status":"ok","percent":4},"weekly":{"status":"ok","percent":3},"monthly":{"status":"ok","percent":1}}}"#;

fn login_opencode_key(tmp: &tempfile::TempDir, key: &str) -> String {
    let stdout = assert_success(
        isolated_subswap(tmp)
            .args(["login", "opencode", "--", key])
            .output()
            .unwrap(),
    );
    stdout
        .trim()
        .strip_prefix("login → opencode/")
        .unwrap_or_else(|| panic!("unexpected login output: {stdout}"))
        .to_string()
}

#[test]
fn default_entry_auto_swaps_exhausted_opencode_go_and_keeps_neighbor_providers() {
    let tmp = tempfile::tempdir().unwrap();
    let auth = tmp.path().join("opencode").join("auth.json");
    write(&auth, r#"{"openai":{"type":"api","key":"sk-keep-other"}}"#);

    let exhausted_id = login_opencode_key(&tmp, OPENCODE_EXHAUSTED_KEY);
    let healthy_id = login_opencode_key(&tmp, OPENCODE_HEALTHY_KEY);
    assert_ne!(exhausted_id, healthy_id);

    assert_success(
        isolated_subswap(&tmp)
            .args(["swap", &format!("opencode/{exhausted_id}")])
            .output()
            .unwrap(),
    );

    let mut bodies = HashMap::new();
    bodies.insert(
        OPENCODE_EXHAUSTED_KEY.to_string(),
        (200_u16, OPENCODE_EXHAUSTED_USAGE.to_string()),
    );
    bodies.insert(
        OPENCODE_HEALTHY_KEY.to_string(),
        (200, OPENCODE_HEALTHY_USAGE.to_string()),
    );
    let server = KeyedUsageServer::start(bodies);

    write(
        &app_config_dir(&tmp).join("config.toml"),
        "[quota]\nmin_refresh_interval_ms = 0\nfetch_retries = 0\n",
    );

    let stdout = assert_success(
        isolated_subswap(&tmp)
            .env("SUBSWAP_OPENCODE_GO_BASE", server.base_url())
            .output()
            .unwrap(),
    );
    assert!(
        stdout.contains("auto: swapped to sk-…9999"),
        "exhausted 5h window must auto-swap to the healthy Go key: {stdout}"
    );

    let live: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(live["openai"]["key"], "sk-keep-other");
    assert_eq!(live["opencode-go"]["key"], OPENCODE_HEALTHY_KEY);
}

#[test]
fn default_entry_does_not_auto_swap_opencode_to_401_key() {
    let tmp = tempfile::tempdir().unwrap();
    let auth = tmp.path().join("opencode").join("auth.json");

    let exhausted_id = login_opencode_key(&tmp, OPENCODE_EXHAUSTED_KEY);
    let _dead_id = login_opencode_key(&tmp, OPENCODE_DEAD_KEY);
    assert_success(
        isolated_subswap(&tmp)
            .args(["swap", &format!("opencode/{exhausted_id}")])
            .output()
            .unwrap(),
    );

    let mut bodies = HashMap::new();
    bodies.insert(
        OPENCODE_EXHAUSTED_KEY.to_string(),
        (200_u16, OPENCODE_EXHAUSTED_USAGE.to_string()),
    );
    bodies.insert(
        OPENCODE_DEAD_KEY.to_string(),
        (401, r#"{"error":"invalid_api_key"}"#.to_string()),
    );
    let server = KeyedUsageServer::start(bodies);

    write(
        &app_config_dir(&tmp).join("config.toml"),
        "[quota]\nmin_refresh_interval_ms = 0\nfetch_retries = 0\n",
    );

    let stdout = assert_success(
        isolated_subswap(&tmp)
            .env("SUBSWAP_OPENCODE_GO_BASE", server.base_url())
            .output()
            .unwrap(),
    );
    assert!(
        !stdout.contains("auto: swapped"),
        "401 Go key must not become an auto-swap target: {stdout}"
    );

    let live: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(live["opencode-go"]["key"], OPENCODE_EXHAUSTED_KEY);
}

/// 按 Bearer API key 返回不同 `/usage` 响应；并发可重入，供默认入口同时查多个账号。
struct KeyedUsageServer {
    addr: std::net::SocketAddr,
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl KeyedUsageServer {
    fn start(bodies: HashMap<String, (u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let bodies = bodies.clone();
                        std::thread::spawn(move || serve_go_usage(stream, &bodies));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            base_url,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for KeyedUsageServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, std::time::Duration::from_millis(100));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_go_usage(mut stream: TcpStream, bodies: &HashMap<String, (u16, String)>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut buffer = [0_u8; 8192];
    let count = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..count]);
    let key = bearer_key(&request).unwrap_or_default();
    let (code, body) = bodies
        .get(&key)
        .cloned()
        .unwrap_or((401, r#"{"error":"unknown_key"}"#.into()));
    let reason = match code {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn bearer_key(request: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("authorization") {
            return None;
        }
        let value = value.trim();
        value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(|s| s.trim().to_string())
    })
}
