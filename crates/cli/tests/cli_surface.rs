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
        .env("CLAUDE_CONFIG_DIR", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("SUBSWAP_NO_DAEMON", "1");
    command
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
        " add ",
        " list ",
        " quota ",
        " refresh ",
        " auto ",
        " daemon ",
    ] {
        assert!(
            !stdout.contains(removed),
            "help should not expose removed command {removed:?}:\n{stdout}"
        );
    }
}

#[test]
fn default_with_empty_home_is_quiet_and_does_not_probe_real_accounts() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    let state = tmp.path().join("state");
    let cache = tmp.path().join("cache");
    let claude = tmp.path().join("claude");
    let codex = tmp.path().join("codex");

    let output = subswap()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CACHE_HOME", &cache)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("CODEX_HOME", &codex)
        // 避免测试副作用:别拉起后台 daemon。
        .env("SUBSWAP_NO_DAEMON", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "No accounts. Log in to Claude Code or Codex CLI, then re-run `subswap`."
    );
    assert!(
        !stdout.contains("[degraded]"),
        "empty registry should stay quiet:\n{stdout}"
    );
}

#[test]
fn deepseek_api_can_be_added_manually_activated_and_switched_back_to_oauth() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude");
    let registry = tmp.path().join("config/subswap/registry.toml");
    let credentials = tmp.path().join("data/subswap/credentials.json");

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
        &tmp.path().join("config/subswap/config.toml"),
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
}
