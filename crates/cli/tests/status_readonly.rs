//! 只读 `subswap status` 的隔离回归测试。
//!
//! 这些测试只依赖临时 `SUBSWAP_HOME`、临时原生客户端路径与一次性 keychain，绝不读取
//! 真实账号、真实额度缓存或真实登录钥匙串。核心断言是：反复读取不写 registry / quota
//! cache / coordination / metrics，不启动 Provider 进程，也不访问网络；同时保持 Apple
//! 端 `SubswapSnapshot` 解码所需字段与原始观测时间。
//!
//! 与 `cli_surface.rs` 分开维护：本文件只覆盖只读状态面，不承载其它命令的登录/切换流程。

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};

/// Clearly invalid short synthetic label; exercises the `sk-` prefix and embedded-token
/// redaction without resembling a real credential.
const SYNTHETIC_SHORT_KEY: &str = "sk-dummy";

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
        // Windows 的系统目录解析不接受 XDG 覆盖；统一根目录确保三端都不触碰真实用户状态。
        .env("SUBSWAP_HOME", tmp.path().join("subswap"))
        .env("CLAUDE_CONFIG_DIR", tmp.path().join("claude"))
        .env("CODEX_HOME", tmp.path().join("codex"))
        .env("KIMI_CODE_HOME", tmp.path().join("kimi"))
        .env("SUBSWAP_OPENCODE_HOME", tmp.path().join("opencode"))
        .env("SUBSWAP_COMMANDCODE_HOME", tmp.path().join("commandcode"))
        .env("SUBSWAP_ANTIGRAVITY_HOME", tmp.path().join("antigravity"))
        .env("SUBSWAP_ANTIGRAVITY_BASE", "http://127.0.0.1:1")
        .env(
            "SUBSWAP_CURSOR_STATE_DB_PATH",
            tmp.path().join("cursor").join("state.vscdb"),
        )
        .env(
            "SUBSWAP_CURSOR_AGENT_AUTH_PATH",
            tmp.path().join("cursor-agent").join("auth.json"),
        )
        .env(
            "SUBSWAP_CURSOR_AGENT_CONFIG_PATH",
            tmp.path().join("cursor-agent").join("cli-config.json"),
        )
        // macOS：Claude / Cursor / Antigravity 的钥匙串读写必须指向一次性 keychain。
        .env("SUBSWAP_CLAUDE_KEYCHAIN_PATH", test_keychain_path(tmp))
        .env("SUBSWAP_CURSOR_KEYCHAIN_PATH", test_keychain_path(tmp))
        .env("SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH", test_keychain_path(tmp))
        // 显式覆盖 metrics 路径，防止外部测试运行器注入的
        // `SUBSWAP_REQUEST_METRICS_PATH` 把计数写到别的任务文件。
        .env("SUBSWAP_REQUEST_METRICS_PATH", metrics_path(tmp))
        .env("SUBSWAP_NO_DAEMON", "1");
    command
}

fn test_keychain_path(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("test.keychain-db")
}

/// macOS：创建供测试使用的一次性 keychain。非 macOS 为 no-op。
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

fn app_root(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("subswap")
}

fn config_dir(tmp: &tempfile::TempDir) -> PathBuf {
    app_root(tmp).join("config")
}

fn cache_dir(tmp: &tempfile::TempDir) -> PathBuf {
    app_root(tmp).join("cache")
}

fn data_dir(tmp: &tempfile::TempDir) -> PathBuf {
    app_root(tmp).join("data")
}

fn registry_path(tmp: &tempfile::TempDir) -> PathBuf {
    config_dir(tmp).join("registry.toml")
}

fn quota_cache_path(tmp: &tempfile::TempDir) -> PathBuf {
    cache_dir(tmp).join("quota_cache.json")
}

fn metrics_path(tmp: &tempfile::TempDir) -> PathBuf {
    data_dir(tmp).join("request_metrics.json")
}

/// 预建 AppPaths 会创建的目录，避免把「首次创建空目录」误判成状态写入。
fn prepare_dirs(tmp: &tempfile::TempDir) {
    for dir in [
        config_dir(tmp),
        cache_dir(tmp),
        data_dir(tmp),
        data_dir(tmp).join("state"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn account_toml(
    provider: &str,
    id: &str,
    label: &str,
    active: bool,
    extra: Option<&str>,
) -> String {
    let mut block = format!(
        "[[accounts]]\nprovider = {provider:?}\nid = {id:?}\nlabel = {label:?}\nactive = {active}\ncreated_at = \"2026-01-01T00:00:00Z\"\npriority = 100\n"
    );
    if let Some(extra) = extra {
        block.push_str(&format!("extra = {extra}\n"));
    }
    block
}

fn write_registry(tmp: &tempfile::TempDir, blocks: &[String]) {
    prepare_dirs(tmp);
    write(&registry_path(tmp), blocks.join("\n"));
}

fn quota_window(
    provider: &str,
    account_id: &str,
    window: &str,
    used: u64,
    limit: u64,
    reset_at: Option<DateTime<Utc>>,
    status: &str,
) -> Value {
    json!({
        "provider": provider,
        "account_id": account_id,
        "window": window,
        "used": used,
        "limit": limit,
        "reset_at": reset_at.map(|value| value.to_rfc3339()),
        "status": status,
        "note": null,
    })
}

fn cache_key(provider: &str, account_id: &str) -> String {
    format!("{provider}::{account_id}")
}

fn write_cache(tmp: &tempfile::TempDir, entries: Value, failures: Value) {
    prepare_dirs(tmp);
    write(
        &quota_cache_path(tmp),
        serde_json::to_vec(&json!({
            "entries": entries,
            "failures": failures,
        }))
        .unwrap(),
    );
}

fn write_metrics(tmp: &tempfile::TempDir, http_start: u64, cache_hit: u64) {
    prepare_dirs(tmp);
    let now = Utc::now();
    write(
        &metrics_path(tmp),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "updated_at": now.timestamp_millis(),
            "corrupt_resets": 0,
            "totals": {
                "requests": {
                    "http_start": http_start,
                    "http_complete": http_start,
                    "native_rpc_start": 0,
                    "native_rpc_complete": 0
                },
                "collections": {
                    "collection_start": 1,
                    "collection_success": 1,
                    "collection_failure": 0,
                    "cache_hit": cache_hit,
                    "coalesced": 0,
                    "deferred": 0
                },
                "statuses": {"200": http_start},
                "last_seen": now.timestamp()
            },
            "providers": {},
            "accounts": {},
            "minutes": []
        }))
        .unwrap(),
    );
}

/// 记录整个 app 根目录下的文件内容，用于证明反复 status 没有写任何状态。
fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

fn run_status_json(command: &mut Command) -> (Value, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = command.args(["status", "--json"]).output().unwrap();
    let stdout = String::from_utf8(stdout).unwrap();
    assert!(
        status.success(),
        "status failed\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&stderr)
    );
    let value = serde_json::from_str(&stdout).unwrap();
    (value, stdout)
}

fn account<'a>(snapshot: &'a Value, provider: &str, id: &str) -> &'a Value {
    snapshot["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["provider"] == provider && account["id"] == id)
        .unwrap_or_else(|| panic!("missing account {provider}/{id}"))
}

fn parse_time(value: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value.as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn empty_home_status_creates_no_directories_or_files() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    let fresh_root = tmp.path().join("fresh-subswap-home");
    assert!(!fresh_root.exists(), "fixture root must start absent");

    let mut command = isolated_subswap(&tmp);
    command.env("SUBSWAP_HOME", &fresh_root);
    let (snapshot, _) = run_status_json(&mut command);

    assert_eq!(snapshot["schema_version"], json!(1));
    assert_eq!(snapshot["accounts"], json!([]));
    assert!(
        !fresh_root.exists(),
        "status created a fresh SUBSWAP_HOME: {}",
        fresh_root.display()
    );

    teardown_test_keychain(&tmp);
}

#[test]
fn fresh_success_with_newer_failure_is_stale() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[account_toml(
            "claude",
            "alice@example.com",
            "alice",
            true,
            None,
        )],
    );
    let observed = Utc::now() - ChronoDuration::seconds(10);
    let failed_at = Utc::now() - ChronoDuration::seconds(3);
    write_cache(
        &tmp,
        json!({
            cache_key("claude", "alice@example.com"): {
                "quotas": [
                    quota_window(
                        "claude",
                        "alice@example.com",
                        "five_hour",
                        10,
                        100,
                        Some(Utc::now() + ChronoDuration::hours(4)),
                        "ok",
                    )
                ],
                "cached_at": observed.to_rfc3339(),
            }
        }),
        json!({
            cache_key("claude", "alice@example.com"): {
                "failed_at": failed_at.to_rfc3339(),
                "consecutive": 1,
                "error": "usage returned 429 Too Many Requests"
            }
        }),
    );

    let (snapshot, _) = run_status_json(&mut isolated_subswap(&tmp));

    let alice = account(&snapshot, "claude", "alice@example.com");
    assert_eq!(alice["fetch_state"], json!("stale"), "{alice:?}");
    assert_eq!(parse_time(&alice["cached_at"]), observed);
    assert_eq!(alice["quotas"].as_array().unwrap().len(), 1);
    assert!(
        alice["error"].as_str().unwrap_or_default().contains("429"),
        "newer failure error must be preserved: {alice:?}"
    );

    teardown_test_keychain(&tmp);
}

#[test]
fn repeated_reads_write_nothing_and_start_no_provider_work() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[account_toml(
            "claude",
            "alice@example.com",
            "alice",
            true,
            None,
        )],
    );
    let observed = Utc::now() - ChronoDuration::seconds(30);
    write_cache(
        &tmp,
        json!({
            cache_key("claude", "alice@example.com"): {
                "quotas": [
                    quota_window(
                        "claude",
                        "alice@example.com",
                        "five_hour",
                        10,
                        100,
                        Some(Utc::now() + ChronoDuration::hours(4)),
                        "ok",
                    )
                ],
                "cached_at": observed.to_rfc3339(),
            }
        }),
        json!({}),
    );
    write_metrics(&tmp, 3, 2);
    // 协调目录与 daemon pid 都是只读入口不得改写的旁路状态。
    write(
        &cache_dir(&tmp)
            .join("quota_collection")
            .join("sentinel.json"),
        br#"{"state":"success"}"#,
    );
    write(
        &data_dir(&tmp).join("state").join("subswapd.pid"),
        b"99999999\n",
    );

    // 预置假原生客户端：若 status 误启动 Provider 进程，会留下 marker。
    #[cfg(unix)]
    let marker = {
        use std::os::unix::fs::PermissionsExt;
        let bin = tmp.path().join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marker = tmp.path().join("provider-started");
        for name in [
            "claude",
            "codex",
            "kimi",
            "cursor-agent",
            "opencode",
            "command-code",
        ] {
            let script = bin.join(name);
            std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).unwrap();
        }
        Some((bin, marker))
    };
    #[cfg(not(unix))]
    let marker: Option<(PathBuf, PathBuf)> = None;

    // 本地监听器：任何 Provider 网络请求都会建立连接。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    let before = tree_snapshot(&app_root(&tmp));
    let mut first = isolated_subswap(&tmp);
    first
        .env("SUBSWAP_OPENCODE_CONSOLE_BASE", &base)
        .env("SUBSWAP_OPENCODE_GO_BASE", &base)
        .env("SUBSWAP_COMMANDCODE_BASE", &base);
    if let Some((bin, _)) = &marker {
        first.env("PATH", bin);
    }
    let (first_json, first_stdout) = run_status_json(&mut first);

    let mut second = isolated_subswap(&tmp);
    second
        .env("SUBSWAP_OPENCODE_CONSOLE_BASE", &base)
        .env("SUBSWAP_OPENCODE_GO_BASE", &base)
        .env("SUBSWAP_COMMANDCODE_BASE", &base);
    if let Some((bin, _)) = &marker {
        second.env("PATH", bin);
    }
    let (second_json, second_stdout) = run_status_json(&mut second);

    let after = tree_snapshot(&app_root(&tmp));
    assert_eq!(before, after, "status changed files under SUBSWAP_HOME");

    if let Some((_, marker)) = &marker {
        assert!(!marker.exists(), "status started a native provider process");
    }
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("status opened a provider network connection: {other:?}"),
    }

    // 两次读取的账号/额度内容一致；只有快照生成时间与指标快照时间允许变化。
    assert_eq!(first_json["accounts"], second_json["accounts"]);
    assert_eq!(first_json["stale_after_ms"], second_json["stale_after_ms"]);
    for text in [&first_stdout, &second_stdout] {
        assert!(!text.contains("sk-secret"), "secret label leaked");
    }

    teardown_test_keychain(&tmp);
}

#[test]
fn json_contract_matches_apple_decoder_and_preserves_observation() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[
            account_toml("claude", "alice@example.com", "alice", true, None),
            account_toml("codex", "bob@example.com", "bob", false, None),
        ],
    );
    let observed = Utc::now() - ChronoDuration::seconds(45);
    let five_reset = Utc::now() + ChronoDuration::hours(4);
    let seven_reset = Utc::now() + ChronoDuration::days(6);
    write_cache(
        &tmp,
        json!({
            cache_key("claude", "alice@example.com"): {
                "quotas": [
                    quota_window(
                        "claude",
                        "alice@example.com",
                        "five_hour",
                        10,
                        100,
                        Some(five_reset),
                        "ok",
                    ),
                    quota_window(
                        "claude",
                        "alice@example.com",
                        "seven_day",
                        50,
                        100,
                        Some(seven_reset),
                        "warn",
                    )
                ],
                "cached_at": observed.to_rfc3339(),
            }
        }),
        json!({}),
    );

    let (snapshot, raw) = run_status_json(&mut isolated_subswap(&tmp));

    assert_eq!(snapshot["schema_version"], json!(1));
    for key in [
        "generated_at",
        "daemon_running",
        "stale_after_ms",
        "auto_swap",
        "warn_pct",
        "exhausted_pct",
        "provider_order",
        "accounts",
        "request_metrics",
    ] {
        assert!(
            snapshot.get(key).is_some(),
            "missing snapshot field {key}: {snapshot}"
        );
    }
    assert!(snapshot["auto_swap"]["enabled"].is_boolean());
    assert!(snapshot["auto_swap"]["threshold"].is_number());
    assert_eq!(snapshot["provider_order"], json!(["claude", "codex"]));
    assert_eq!(snapshot["request_metrics"]["available"], json!(false));

    let claude = account(&snapshot, "claude", "alice@example.com");
    for key in [
        "id",
        "provider",
        "provider_display_name",
        "display_label",
        "active",
        "billing",
        "manual_only",
        "fetch_state",
        "cached_at",
        "quotas",
    ] {
        assert!(
            claude.get(key).is_some(),
            "missing account field {key}: {claude}"
        );
    }
    assert_eq!(claude["provider_display_name"], json!("Claude / Anthropic"));
    assert_eq!(claude["fetch_state"], json!("ready"));
    assert_eq!(parse_time(&claude["cached_at"]), observed);
    assert_ne!(parse_time(&snapshot["generated_at"]), observed);

    let quotas = claude["quotas"].as_array().unwrap();
    assert_eq!(quotas.len(), 2);
    for quota in quotas {
        for key in [
            "provider",
            "account_id",
            "window",
            "used",
            "limit",
            "reset_at",
            "status",
            "note",
        ] {
            assert!(
                quota.get(key).is_some(),
                "missing quota field {key}: {quota}"
            );
        }
        assert_eq!(quota["account_id"], json!("alice@example.com"));
    }
    assert_eq!(quotas[0]["window"], json!("five_hour"));
    assert_eq!(parse_time(&quotas[0]["reset_at"]), five_reset);
    assert_eq!(quotas[1]["window"], json!("seven_day"));

    // generated_at 每次重绘都会变，但观测时间保持原始值。
    let (again, _) = run_status_json(&mut isolated_subswap(&tmp));
    let again_claude = account(&again, "claude", "alice@example.com");
    assert_eq!(parse_time(&again_claude["cached_at"]), observed);
    assert!(parse_time(&again["generated_at"]) > parse_time(&snapshot["generated_at"]));
    assert!(raw.contains("\"schema_version\": 1"));

    teardown_test_keychain(&tmp);
}

#[test]
fn expired_reset_windows_stay_stale_without_fake_freshness() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[
            account_toml("claude", "expired@example.com", "expired", true, None),
            account_toml("claude", "old@example.com", "old", false, None),
            account_toml("codex", "failed@example.com", "failed", false, None),
            account_toml(
                "claude",
                "api@example.com",
                "api-key",
                false,
                Some(
                    r#"{ manual_only = true, billing = "metered", api_config = { base_url = "https://api.deepseek.com/anthropic" } }"#,
                ),
            ),
            account_toml("kimi", "future@example.com", "future", false, None),
        ],
    );
    let observed = Utc::now() - ChronoDuration::seconds(10);
    let old_observed = Utc::now() - ChronoDuration::minutes(10);
    let future_observed = Utc::now() + ChronoDuration::minutes(5);
    write_cache(
        &tmp,
        json!({
            // 刚查过，但唯一窗口已 reset：保留观测时间并标记 stale，不展示旧数字。
            cache_key("claude", "expired@example.com"): {
                "quotas": [
                    quota_window(
                        "claude",
                        "expired@example.com",
                        "five_hour",
                        100,
                        100,
                        Some(Utc::now() - ChronoDuration::minutes(1)),
                        "exhausted",
                    )
                ],
                "cached_at": observed.to_rfc3339(),
            },
            // 窗口未 reset，但观测已超过 decision age：仍展示数字但标记 stale。
            cache_key("claude", "old@example.com"): {
                "quotas": [
                    quota_window(
                        "claude",
                        "old@example.com",
                        "seven_day",
                        40,
                        100,
                        Some(Utc::now() + ChronoDuration::days(3)),
                        "ok",
                    )
                ],
                "cached_at": old_observed.to_rfc3339(),
            },
            // 未来时间戳按 stale 处理，不能伪造新鲜数据。
            cache_key("kimi", "future@example.com"): {
                "quotas": [
                    quota_window(
                        "kimi",
                        "future@example.com",
                        "five_hour",
                        1,
                        100,
                        Some(Utc::now() + ChronoDuration::hours(2)),
                        "ok",
                    )
                ],
                "cached_at": future_observed.to_rfc3339(),
            }
        }),
        json!({
            cache_key("codex", "failed@example.com"): {
                "failed_at": (Utc::now() - ChronoDuration::seconds(5)).to_rfc3339(),
                "consecutive": 2,
                "error": "usage returned 401 Unauthorized"
            }
        }),
    );

    let (snapshot, _) = run_status_json(&mut isolated_subswap(&tmp));

    let expired = account(&snapshot, "claude", "expired@example.com");
    assert_eq!(expired["fetch_state"], json!("stale"));
    assert_eq!(parse_time(&expired["cached_at"]), observed);
    assert_eq!(expired["quotas"], json!([]));

    let old = account(&snapshot, "claude", "old@example.com");
    assert_eq!(old["fetch_state"], json!("stale"));
    assert_eq!(parse_time(&old["cached_at"]), old_observed);
    assert_eq!(old["quotas"].as_array().unwrap().len(), 1);

    let failed = account(&snapshot, "codex", "failed@example.com");
    assert_eq!(failed["fetch_state"], json!("failed"));
    assert!(failed["error"].as_str().unwrap().contains("401"));
    assert!(failed.get("cached_at").is_none());

    let api = account(&snapshot, "claude", "api@example.com");
    assert_eq!(api["manual_only"], json!(true));
    assert_eq!(api["fetch_state"], json!("missing"));
    assert_eq!(api["display_label"], json!("api-key"));

    let future = account(&snapshot, "kimi", "future@example.com");
    assert_eq!(future["fetch_state"], json!("stale"));
    assert_eq!(parse_time(&future["cached_at"]), future_observed);

    teardown_test_keychain(&tmp);
}

#[test]
fn secret_labels_are_redacted_and_safe_labels_are_kept() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    let embedded_secret_label = format!("fixture {SYNTHETIC_SHORT_KEY}");
    let bare_token = "q".repeat(40);
    write_registry(
        &tmp,
        &[
            account_toml(
                "claude",
                "secret@example.com",
                &embedded_secret_label,
                false,
                Some(
                    r#"{ manual_only = true, api_config = { base_url = "https://api.deepseek.com/anthropic" } }"#,
                ),
            ),
            account_toml("codex", "bare@example.com", &bare_token, false, None),
            account_toml("kimi", "safe@example.com", "Work account", false, None),
        ],
    );

    let (snapshot, raw) = run_status_json(&mut isolated_subswap(&tmp));

    let secret = account(&snapshot, "claude", "secret@example.com");
    assert_eq!(secret["display_label"], json!("API · api.deepseek.com"));
    let bare = account(&snapshot, "codex", "bare@example.com");
    assert_eq!(bare["display_label"], json!("API account"));
    let safe = account(&snapshot, "kimi", "safe@example.com");
    assert_eq!(safe["display_label"], json!("Work account"));

    assert!(
        !raw.contains(SYNTHETIC_SHORT_KEY),
        "synthetic key label leaked into status JSON"
    );
    assert!(
        !raw.contains(&bare_token),
        "bare token leaked into status JSON"
    );

    teardown_test_keychain(&tmp);
}

#[test]
fn metrics_are_exported_read_only_without_claiming_observation_freshness() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[account_toml(
            "claude",
            "alice@example.com",
            "alice",
            true,
            None,
        )],
    );
    let observed = Utc::now() - ChronoDuration::minutes(8);
    write_cache(
        &tmp,
        json!({
            cache_key("claude", "alice@example.com"): {
                "quotas": [
                    quota_window(
                        "claude",
                        "alice@example.com",
                        "five_hour",
                        70,
                        100,
                        Some(Utc::now() + ChronoDuration::hours(2)),
                        "ok",
                    )
                ],
                "cached_at": observed.to_rfc3339(),
            }
        }),
        json!({}),
    );
    write_metrics(&tmp, 7, 4);
    let before = std::fs::read(metrics_path(&tmp)).unwrap();

    let (snapshot, _) = run_status_json(&mut isolated_subswap(&tmp));

    let metrics = &snapshot["request_metrics"];
    assert_eq!(metrics["available"], json!(true));
    assert_eq!(metrics["schema_version"], json!(1));
    assert_eq!(metrics["totals"]["requests"]["http_start"], json!(7));
    assert_eq!(metrics["totals"]["collections"]["cache_hit"], json!(4));
    assert_eq!(metrics["window_minutes"], json!(60));
    assert!(metrics["generated_at"].is_string());

    // 观测时间仍是缓存的 8 分钟前；快照与 metrics 的新 generated_at 不能抬高它。
    let claude = account(&snapshot, "claude", "alice@example.com");
    assert_eq!(claude["fetch_state"], json!("stale"));
    assert_eq!(parse_time(&claude["cached_at"]), observed);
    assert!(parse_time(&snapshot["generated_at"]) > observed);

    assert_eq!(std::fs::read(metrics_path(&tmp)).unwrap(), before);
    // 指标文件存在时也不得出现 lock 文件。
    assert!(!data_dir(&tmp).join("request_metrics.json.lock").exists());

    teardown_test_keychain(&tmp);
}

#[test]
fn non_json_output_stays_human_readable_and_never_fetches() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[account_toml(
            "claude",
            "api@example.com",
            SYNTHETIC_SHORT_KEY,
            false,
            Some(
                r#"{ manual_only = true, api_config = { base_url = "https://api.deepseek.com/anthropic" } }"#,
            ),
        )],
    );

    let before = tree_snapshot(&app_root(&tmp));
    let output = isolated_subswap(&tmp).arg("status").output().unwrap();
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Claude / Anthropic"));
    assert!(stdout.contains("manual only"));
    assert!(stdout.contains("API · api.deepseek.com"));
    assert!(!stdout.contains(SYNTHETIC_SHORT_KEY));
    assert_eq!(before, tree_snapshot(&app_root(&tmp)));

    teardown_test_keychain(&tmp);
}

/// `--no-metrics` 供 GUI/小组件高频轮询：省掉随分钟桶增长的诊断计数器，
/// 且不能改变其余字段的只读语义。完整接口仍保留 `request_metrics`。
#[test]
fn status_no_metrics_omits_request_counters_only() {
    let tmp = tempfile::tempdir().unwrap();
    setup_test_keychain(&tmp);
    write_registry(
        &tmp,
        &[account_toml(
            "claude",
            "alice@example.com",
            "alice@example.com",
            true,
            None,
        )],
    );
    write_metrics(&tmp, 7, 4);

    let before = tree_snapshot(&app_root(&tmp));
    let Output {
        status,
        stdout,
        stderr,
    } = isolated_subswap(&tmp)
        .args(["status", "--json", "--no-metrics"])
        .output()
        .unwrap();
    assert!(
        status.success(),
        "status --no-metrics failed\nstderr:\n{}",
        String::from_utf8_lossy(&stderr)
    );
    let compact = String::from_utf8(stdout).unwrap();
    let snapshot: Value = serde_json::from_str(&compact).unwrap();

    assert!(snapshot.get("request_metrics").is_none());
    for key in [
        "schema_version",
        "generated_at",
        "daemon_running",
        "stale_after_ms",
        "auto_swap",
        "warn_pct",
        "exhausted_pct",
        "provider_order",
        "accounts",
    ] {
        assert!(
            snapshot.get(key).is_some(),
            "missing snapshot field {key}: {snapshot}"
        );
    }
    assert_eq!(snapshot["accounts"][0]["id"], json!("alice@example.com"));

    // 默认接口保持不变，消费者仍可取到计数器。
    let (full, full_raw) = run_status_json(&mut isolated_subswap(&tmp));
    assert_eq!(full["request_metrics"]["available"], json!(true));
    assert_eq!(
        full["request_metrics"]["totals"]["requests"]["http_start"],
        json!(7)
    );
    assert!(full_raw.len() > compact.len());

    assert_eq!(before, tree_snapshot(&app_root(&tmp)));
    assert!(!data_dir(&tmp).join("request_metrics.json.lock").exists());
    teardown_test_keychain(&tmp);
}
