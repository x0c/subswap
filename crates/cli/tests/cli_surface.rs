use std::process::Command;
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
        // Cursor 的平台默认路径不受 HOME/SUBSWAP_HOME 统一覆盖，必须显式指向临时目录。
        .env(
            "SUBSWAP_CURSOR_STATE_DB_PATH",
            tmp.path().join("cursor").join("state.vscdb"),
        )
        // macOS：把 Claude Code 钥匙串读写重定向到一次性 keychain，绝不碰用户真实登录钥匙串
        // （否则集成测试会弹授权框并污染本机凭证）。
        .env("SUBSWAP_CLAUDE_KEYCHAIN_PATH", test_keychain_path(tmp))
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
