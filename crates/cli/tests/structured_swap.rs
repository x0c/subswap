//! `subswap --json swap <target>` 结构化回执契约测试。
//!
//! 覆盖 adopted 契约（docs/CLI.md "Structured activation result"）：成功时 stdout
//! 恰好一个 JSON 对象 `{provider, id, notice?}`，其中 `notice` 取 Provider 能力
//! 声明（`post_swap_notice`），无提醒时省略；不查 quota、不打状态表、不做 human ack。
//! 失败时绝不打印伪造的成功对象；非 JSON 行为与无参帮助行为保持不变。
//!
//! 隔离：只用一次性 `SUBSWAP_HOME` 与各 Provider 原生 home 覆盖，绝不触碰真实
//! 登录钥匙串；codex/commandcode 均为文件型 fixture，无网络、无原生客户端调用。
//! `--json swap` 本身的零 quota 断言用本地计数 mock 证明（commandcode base 可覆盖）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::{fs, path::Path};

fn subswap() -> Command {
    Command::new(env!("CARGO_BIN_EXE_subswap"))
}

/// 明确不可达的 localhost：未注入 mock 的 setup 查询必须快速失败，绝不打生产端点。
const OFFLINE_COMMANDCODE_BASE: &str = "http://127.0.0.1:1";

/// 与 `cli_surface.rs::isolated_subswap` 同构的隔离环境（该文件不可见此 helper，
/// 故在此镜像；覆盖面保持一致，不得手工拼不完整的子集）。
fn isolated_subswap(tmp: &tempfile::TempDir) -> Command {
    let test_keychain = tmp.path().join("test.keychain-db");
    let mut command = subswap();
    command
        .env("HOME", tmp.path().join("home"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .env("SUBSWAP_HOME", tmp.path().join("subswap"))
        .env("CLAUDE_CONFIG_DIR", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("KIMI_CODE_HOME", tmp.path().join("kimi"))
        .env("SUBSWAP_OPENCODE_HOME", tmp.path().join("opencode"))
        .env("SUBSWAP_COMMANDCODE_HOME", tmp.path().join("commandcode"))
        .env("SUBSWAP_COMMANDCODE_BASE", OFFLINE_COMMANDCODE_BASE)
        .env("SUBSWAP_ANTIGRAVITY_HOME", tmp.path().join("antigravity"))
        .env("SUBSWAP_ANTIGRAVITY_BASE", OFFLINE_COMMANDCODE_BASE)
        .env(
            "SUBSWAP_CURSOR_STATE_DB_PATH",
            tmp.path().join("cursor").join("state.vscdb"),
        )
        .env("SUBSWAP_CLAUDE_KEYCHAIN_PATH", &test_keychain)
        .env("SUBSWAP_CURSOR_KEYCHAIN_PATH", &test_keychain)
        .env("SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH", &test_keychain)
        .env(
            "SUBSWAP_REQUEST_METRICS_PATH",
            tmp.path().join("request_metrics.json"),
        )
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

fn app_root(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("subswap")
}

fn app_config_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    app_root(tmp).join("config")
}

fn app_data_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    app_root(tmp).join("data")
}

fn write_fast_quota_timeout(tmp: &tempfile::TempDir) {
    write(
        &app_config_dir(tmp).join("config.toml"),
        "[quota]\nfetch_timeout_ms = 1\nfetch_retries = 0\nmin_refresh_interval_ms = 0\n",
    );
}

/// stdout 必须恰好是一个 JSON 对象：整体可解析，且去除首尾空白后没有多余行。
fn parse_single_json_object(stdout: &str) -> serde_json::Value {
    let trimmed = stdout.trim();
    assert!(
        !trimmed.is_empty(),
        "expected exactly one JSON object on stdout, got empty output"
    );
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not a single JSON object ({e}):\n{stdout}"));
    assert!(value.is_object(), "expected a JSON object, got: {value}");
    value
}

/// 成功回执绝不能混入 human 行或状态表。
fn assert_no_human_table(stdout: &str) {
    for marker in [
        "swap →",
        "No accounts",
        "five_hour",
        "seven_day",
        "┌",
        "│",
        "active",
    ] {
        assert!(
            !stdout.contains(marker),
            "JSON receipt must not contain human/table marker {marker:?}:\n{stdout}"
        );
    }
}

/// 计数型 HTTP 桩：只记录命中次数，`--json swap` 成功后必须保持零命中，
/// 证明结构化成功不依赖任何 quota/usage 网络请求。
struct CountingServer {
    base_url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CountingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_hits = Arc::clone(&hits);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        thread_hits.fetch_add(1, Ordering::Relaxed);
                        let mut buf = [0_u8; 4096];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url,
            hits,
            stop,
            handle: Some(handle),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

impl Drop for CountingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 双 codex 账号 fixture：`active_id` 为当前 live，`parked_id` 为切换目标。
/// 返回 live auth.json 路径，供调用方断言原生文件确被覆盖。
fn setup_two_codex_accounts(
    tmp: &tempfile::TempDir,
    active_id: &str,
    parked_id: &str,
) -> std::path::PathBuf {
    let live_a = format!(
        r#"{{"account_key":"{active_id}","email":"{active_id}@example.com","chatgpt_account_id":"ca-{active_id}"}}"#
    );
    let live_b = format!(
        r#"{{"account_key":"{parked_id}","email":"{parked_id}@example.com","chatgpt_account_id":"ca-{parked_id}"}}"#
    );
    let live_path = tmp.path().join("codex").join("auth.json");
    write(&live_path, &live_a);
    write(
        &app_config_dir(tmp).join("registry.toml"),
        &format!(
            r#"[[accounts]]
provider = "codex"
id = "{active_id}"
label = "{active_id}@example.com"
active = true
created_at = "2026-07-01T00:00:00Z"
priority = 100

[[accounts]]
provider = "codex"
id = "{parked_id}"
label = "{parked_id}@example.com"
active = false
created_at = "2026-07-02T00:00:00Z"
priority = 100
"#
        ),
    );
    write(
        &app_data_dir(tmp).join("credentials.json"),
        &serde_json::json!({
            format!("codex:{active_id}:auth_json"): live_a,
            format!("codex:{parked_id}:auth_json"): live_b,
        })
        .to_string(),
    );
    live_path
}

fn login_commandcode_key(tmp: &tempfile::TempDir, key: &str) -> String {
    let stdout = assert_success(
        isolated_subswap(tmp)
            .args(["login", "commandcode", "--", key])
            .env("SUBSWAP_COMMANDCODE_BASE", OFFLINE_COMMANDCODE_BASE)
            .output()
            .unwrap(),
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("login → commandcode/"))
        .unwrap_or_else(|| panic!("missing commandcode login id in: {stdout}"))
        .trim()
        .to_string()
}

/// Codex 有 `post_swap_notice`（官方客户端不热读，须重启）：`--json swap`
/// 必须原样带出该能力声明，且 stdout 恰好一个对象、无 human 行、无状态表。
#[test]
fn json_swap_codex_reports_canonical_provider_id_and_notice() {
    let tmp = tempfile::tempdir().unwrap();
    write_fast_quota_timeout(&tmp);
    let live_path = setup_two_codex_accounts(&tmp, "key-a", "key-b");

    let output = isolated_subswap(&tmp)
        .args(["--json", "swap", "codex/key-b"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "json swap failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_no_human_table(&stdout);
    let receipt = parse_single_json_object(&stdout);
    assert_eq!(receipt["provider"], "codex");
    assert_eq!(receipt["id"], "key-b");
    assert_eq!(
        receipt["notice"],
        "Restart running Codex CLI sessions to use this account."
    );

    // 原生激活确已发生：live 文件被覆盖，registry 标记新 active。
    let live_now = fs::read_to_string(&live_path).unwrap();
    assert!(
        live_now.contains("\"account_key\":\"key-b\""),
        "live auth.json must be switched to key-b: {live_now}"
    );
    let registry = fs::read_to_string(app_config_dir(&tmp).join("registry.toml")).unwrap();
    assert!(
        registry.contains("key-b"),
        "registry must track key-b: {registry}"
    );

    // 数字别名同样归一到 canonical id：从无参列表读出 key-a 的编号再切回去。
    let listing = assert_success(isolated_subswap(&tmp).args(["swap"]).output().unwrap());
    let number = listing
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let first = tokens.next()?;
            let (n, rest) = if first == "*" {
                (tokens.next()?, tokens.next()?)
            } else {
                (first, tokens.next()?)
            };
            (rest == "codex/key-a").then(|| n.to_string())
        })
        .next()
        .unwrap_or_else(|| panic!("key-a missing from swap listing:\n{listing}"));
    let back = assert_success(
        isolated_subswap(&tmp)
            .args(["--json", "swap", &number])
            .output()
            .unwrap(),
    );
    let receipt = parse_single_json_object(&back);
    assert_eq!(receipt["provider"], "codex");
    assert_eq!(receipt["id"], "key-a");
}

/// CommandCode 无 `post_swap_notice`：`notice` 字段省略（Apple 端可选解码），
/// 且 `--json swap` 成功不产生任何 usage 网络请求（计数桩保持零命中）。
#[test]
fn json_swap_commandcode_omits_notice_and_queries_no_quota() {
    let tmp = tempfile::tempdir().unwrap();
    write_fast_quota_timeout(&tmp);
    let first = login_commandcode_key(&tmp, "cc-test-json-0001");
    let _second = login_commandcode_key(&tmp, "cc-test-json-0002");

    let server = CountingServer::start();
    let output = isolated_subswap(&tmp)
        .args(["--json", "swap", &first])
        .env("SUBSWAP_COMMANDCODE_BASE", server.base_url.clone())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "json swap failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_no_human_table(&stdout);
    let receipt = parse_single_json_object(&stdout);
    assert_eq!(receipt["provider"], "commandcode");
    assert_eq!(receipt["id"], first);
    assert!(
        receipt.get("notice").is_none_or(serde_json::Value::is_null),
        "providers without post_swap_notice must omit/null the notice field: {receipt}"
    );
    assert_eq!(
        server.hits(),
        0,
        "structured swap success must not issue any usage/quota request"
    );

    // 原生激活确已发生：live auth.json 指回第一把 key。
    let live: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(tmp.path().join("commandcode").join("auth.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(live["apiKey"], "cc-test-json-0001");
}

/// 解析失败（未知目标）：非零退出，stdout 绝不出现伪造的成功对象。
#[test]
fn json_swap_unknown_target_reports_error_without_success_object() {
    let tmp = tempfile::tempdir().unwrap();
    write_fast_quota_timeout(&tmp);
    setup_two_codex_accounts(&tmp, "key-a", "key-b");

    let output = isolated_subswap(&tmp)
        .args(["--json", "swap", "ghost"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "swap to an unknown account must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("account not found"),
        "expected account-not-found error, got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_err()
            || !stdout.contains("\"provider\""),
        "failure must not print a success object:\n{stdout}"
    );
}

/// 激活失败（registry 有 parked 账号但 store 缺凭证）：非零退出，
/// stdout 绝不出现伪造的成功对象；错误归因到激活而非解析。
#[test]
fn json_swap_activation_failure_reports_error_without_success_object() {
    let tmp = tempfile::tempdir().unwrap();
    write_fast_quota_timeout(&tmp);
    let live_a =
        r#"{"account_key":"key-a","email":"key-a@example.com","chatgpt_account_id":"ca-key-a"}"#;
    write(&tmp.path().join("codex").join("auth.json"), live_a);
    write(
        &app_config_dir(&tmp).join("registry.toml"),
        r#"[[accounts]]
provider = "codex"
id = "key-a"
label = "key-a@example.com"
active = true
created_at = "2026-07-01T00:00:00Z"
priority = 100

[[accounts]]
provider = "codex"
id = "key-broken"
label = "key-broken@example.com"
active = false
created_at = "2026-07-02T00:00:00Z"
priority = 100
"#,
    );
    // store 故意缺 key-broken 的 blob：解析能过，激活必须失败。
    write(
        &app_data_dir(&tmp).join("credentials.json"),
        &serde_json::json!({ "codex:key-a:auth_json": live_a }).to_string(),
    );

    let output = isolated_subswap(&tmp)
        .args(["--json", "swap", "key-broken"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "swap with a missing stored credential must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed"),
        "expected an activation failure, got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_err()
            || !stdout.contains("\"provider\""),
        "failed activation must not print a success object:\n{stdout}"
    );
}

/// 非 JSON 行为保持不变：human ack 行仍在；无参 `swap`（无论是否带 `--json`）
/// 仍打印编号清单而非 JSON。
#[test]
fn non_json_swap_ack_and_noarg_listing_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    write_fast_quota_timeout(&tmp);
    let first = login_commandcode_key(&tmp, "cc-test-plain-0001");
    let _second = login_commandcode_key(&tmp, "cc-test-plain-0002");

    let plain = assert_success(
        isolated_subswap(&tmp)
            .args(["swap", &first])
            .output()
            .unwrap(),
    );
    assert!(
        plain.contains(&format!("swap → commandcode/{first}")),
        "non-JSON swap must keep the human acknowledgement:\n{plain}"
    );

    for extra in [false, true] {
        let mut cmd = isolated_subswap(&tmp);
        if extra {
            cmd.arg("--json");
        }
        let listing = assert_success(cmd.arg("swap").output().unwrap());
        assert!(
            listing.contains("Usage: subswap swap"),
            "no-arg swap must keep the numbered listing help (json={extra}):\n{listing}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(listing.trim()).is_err(),
            "no-arg swap must not emit a JSON object (json={extra}):\n{listing}"
        );
    }
}
