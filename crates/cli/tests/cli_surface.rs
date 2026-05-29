use std::process::Command;

fn subswap() -> Command {
    Command::new(env!("CARGO_BIN_EXE_subswap"))
}

#[test]
fn help_shows_only_current_commands() {
    let output = subswap().arg("--help").output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: subswap"));
    assert!(stdout.contains("login"));
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
