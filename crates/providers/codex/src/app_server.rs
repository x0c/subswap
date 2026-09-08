//! 通过官方 Codex app-server 查询当前账号额度，并为停用号委托官方刷新。
//!
//! 优先代理到现有控制通道，共享官方客户端进程内的最新认证状态；没有控制通道时也会短暂
//! 启动独立 app-server。若普通 Codex 正在运行，只调用官方额度查询，不再额外发起强制刷新；
//! 明确没有并发客户端时，认证失败才允许显式刷新并重试。该模块从不自行实现 OAuth。
//!
//! 停用号刷新见 [`refresh_parked_blob`]：把仓库里的**完整** `auth.json`（含 refresh）写入临时
//! `CODEX_HOME`，再跑官方 app-server；禁止套用清空 refresh 的 [`SanitizedHome`]（那只保护 live 并发）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use subswap_core::defaults::REFRESH_SLACK_MS;
use subswap_provider_common::{extract_access_token, extract_refresh_token, RefreshOutcome};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::timeout;

use crate::codex_files::access_token_needs_refresh;
use crate::paths::codex_home;

const RPC_TIMEOUT: Duration = Duration::from_secs(8);
const SESSION_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_BINARY_ENV: &str = "SUBSWAP_CODEX_BINARY";

/// 使用官方协议取得当前账号额度，并转换成旧解析器能消费的稳定字段。
pub async fn fetch_usage() -> Result<Value> {
    let home = codex_home();
    let socket = home
        .join("app-server-control")
        .join("app-server-control.sock");
    let binary = codex_binary();

    if tokio::fs::metadata(&socket).await.is_ok() {
        match query_command(
            &binary,
            &["app-server", "proxy", "--sock"],
            Some(&socket),
            &home,
            true,
        )
        .await
        {
            Ok(usage) => return Ok(usage),
            Err(error) if !allows_compat_fallback(&error) => return Err(error),
            Err(error) => tracing::debug!(
                socket = %socket.display(),
                error = %error,
                "Codex 控制通道查询失败"
            ),
        }
    }

    if no_codex_process_running_async().await {
        query_command(&binary, &["app-server", "--stdio"], None, &home, true).await
    } else {
        let sanitized = SanitizedHome::create(&home).await?;
        query_command(
            &binary,
            &["app-server", "--stdio"],
            None,
            sanitized.path(),
            false,
        )
        .await
    }
}

/// 只有通道不可用、认证失败或协议不兼容时才允许兼容回退；限流及其他官方服务错误原样返回。
pub fn allows_compat_fallback(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcFailure>()
        .map(|failure| {
            !failure.remote
                || failure.is_authentication_failure()
                || failure.is_method_unsupported()
        })
        .unwrap_or(true)
}

/// 停用号：完整 auth blob → 临时 `CODEX_HOME` → 官方 app-server 按需刷新 → 吸收轮换结果。
///
/// - access 仍在预刷新窗口外 → [`RefreshOutcome::Unsupported`]（不启进程）
/// - 无 refresh / 二进制不可用 / 传输失败 → `Unsupported`（降级，不拖垮整表）
/// - 官方认证明确失败 → [`RefreshOutcome::DeadToken`]
/// - auth 内 access/refresh 任一变化 → [`RefreshOutcome::Rotated`]
pub async fn refresh_parked_blob(blob: &str) -> Result<RefreshOutcome> {
    refresh_parked_blob_with_binary(blob, &codex_binary()).await
}

fn codex_binary() -> PathBuf {
    std::env::var_os(CODEX_BINARY_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"))
}

async fn refresh_parked_blob_with_binary(blob: &str, binary: &Path) -> Result<RefreshOutcome> {
    if extract_refresh_token(blob).is_none() {
        return Ok(RefreshOutcome::Unsupported);
    }
    let now = chrono::Utc::now().timestamp();
    let slack_secs = REFRESH_SLACK_MS / 1000;
    if !access_token_needs_refresh(blob, now, slack_secs) {
        return Ok(RefreshOutcome::Unsupported);
    }

    let parked = ParkedHome::create(blob).await?;
    let query = query_command(binary, &["app-server", "--stdio"], None, parked.path(), true).await;
    let after = match tokio::fs::read_to_string(parked.path().join("auth.json")).await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!(error = %error, "read parked Codex auth after app-server failed");
            return match query {
                Ok(_) => Ok(RefreshOutcome::Unsupported),
                Err(error) if is_authentication_error(&error) => Ok(RefreshOutcome::DeadToken),
                Err(error) if is_spawn_or_transport_error(&error) => {
                    tracing::debug!(error = %error, "Codex app-server unavailable for parked refresh");
                    Ok(RefreshOutcome::Unsupported)
                }
                Err(error) => Err(error),
            };
        }
    };

    if credentials_rotated(blob, &after) {
        // 即使额度解析失败，只要官方已轮换 token，也必须写回仓库，否则下次仍用死 access。
        if let Err(error) = &query {
            tracing::debug!(
                error = %error,
                "parked Codex app-server query failed after token rotation; absorbing auth anyway"
            );
        }
        return Ok(RefreshOutcome::Rotated(after));
    }

    match query {
        Ok(_) => Ok(RefreshOutcome::Unsupported),
        Err(error) if is_authentication_error(&error) => Ok(RefreshOutcome::DeadToken),
        Err(error) if is_spawn_or_transport_error(&error) => {
            tracing::debug!(error = %error, "Codex app-server unavailable for parked refresh");
            Ok(RefreshOutcome::Unsupported)
        }
        Err(error) => Err(error),
    }
}

fn credentials_rotated(before: &str, after: &str) -> bool {
    extract_access_token(before) != extract_access_token(after)
        || extract_refresh_token(before) != extract_refresh_token(after)
}

fn is_authentication_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcFailure>()
        .is_some_and(RpcFailure::is_authentication_failure)
        || {
            let message = error.to_string().to_ascii_lowercase();
            ["401", "unauthorized", "authentication", "not logged in"]
                .iter()
                .any(|needle| message.contains(needle))
        }
}

fn is_spawn_or_transport_error(error: &anyhow::Error) -> bool {
    if error
        .downcast_ref::<RpcFailure>()
        .is_some_and(|failure| !failure.remote)
    {
        return true;
    }
    let message = error.to_string().to_ascii_lowercase();
    [
        "start codex app-server",
        "no such file",
        "not found",
        "timed out",
        "broken pipe",
        "codex app-server closed",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// 停用号专用：写入**完整** auth（含 refresh），与清空 refresh 的 [`SanitizedHome`] 相对。
struct ParkedHome {
    directory: tempfile::TempDir,
}

impl ParkedHome {
    async fn create(blob: &str) -> Result<Self> {
        let _: Value = serde_json::from_str(blob).context("parked Codex auth is not valid JSON")?;
        let directory = tokio::task::spawn_blocking(tempfile::tempdir)
            .await
            .context("create parked Codex home task")?
            .context("create parked Codex home")?;
        let auth_path = directory.path().join("auth.json");
        tokio::fs::write(&auth_path, blob.as_bytes())
            .await
            .context("write parked Codex auth")?;
        set_owner_only_permissions(&auth_path).await?;
        Ok(Self { directory })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }
}

struct SanitizedHome {
    directory: tempfile::TempDir,
}

impl SanitizedHome {
    async fn create(live_home: &Path) -> Result<Self> {
        let raw = tokio::fs::read(live_home.join("auth.json"))
            .await
            .context("read live Codex auth for isolated query")?;
        let mut auth: Value =
            serde_json::from_slice(&raw).context("parse live Codex auth for isolated query")?;
        let tokens = auth
            .get_mut("tokens")
            .and_then(Value::as_object_mut)
            .context("live Codex auth has no token object")?;
        // 官方 TokenData 接受字符串；保留字段并清空值，比删除字段更兼容旧版本。
        tokens.insert("refresh_token".into(), Value::String(String::new()));

        let directory = tokio::task::spawn_blocking(tempfile::tempdir)
            .await
            .context("create isolated Codex home task")?
            .context("create isolated Codex home")?;
        let auth_path = directory.path().join("auth.json");
        tokio::fs::write(&auth_path, serde_json::to_vec(&auth)?)
            .await
            .context("write isolated Codex auth")?;
        set_owner_only_permissions(&auth_path).await?;
        Ok(Self { directory })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }
}

#[cfg(unix)]
async fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .context("protect isolated Codex auth")
}

#[cfg(not(unix))]
async fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

async fn query_command(
    binary: &Path,
    args: &[&str],
    socket: Option<&Path>,
    home: &Path,
    allow_refresh: bool,
) -> Result<Value> {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(socket) = socket {
        command.arg(socket);
    }

    let mut session = AppServerSession::spawn(command)?;
    let result = match timeout(SESSION_TIMEOUT, session.query_rate_limits(allow_refresh)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Codex app-server session timed out")),
    };
    session.close().await;
    result
}

/// 只有明确确认没有普通 Codex 进程时才允许额外请求官方服务强制刷新；检测失败按有并发处理。
async fn no_codex_process_running_async() -> bool {
    tokio::task::spawn_blocking(no_codex_process_running)
        .await
        .unwrap_or(false)
}

#[cfg(unix)]
fn no_codex_process_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "codex"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn no_codex_process_running() -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq codex.exe", "/NH"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| !output.to_ascii_lowercase().contains("codex.exe"))
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn no_codex_process_running() -> bool {
    false
}

struct AppServerSession {
    child: Child,
    stdin: tokio::process::ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl AppServerSession {
    fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command.spawn().context("start Codex app-server")?;
        let stdin = child.stdin.take().context("open Codex app-server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("open Codex app-server stdout")?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        })
    }

    async fn query_rate_limits(&mut self, allow_refresh: bool) -> Result<Value> {
        self.request(
            1,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "subswap",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {}
            }),
        )
        .await?;
        self.notify("initialized").await?;

        match self
            .request(2, "account/rateLimits/read", Value::Null)
            .await
        {
            Ok(result) => rate_limits_to_usage(result),
            Err(error) if error.is_authentication_failure() && allow_refresh => {
                self.request(3, "account/read", json!({ "refreshToken": true }))
                    .await
                    .context("Codex token refresh failed")?;
                let result = self
                    .request(4, "account/rateLimits/read", Value::Null)
                    .await
                    .context("Codex rate-limit retry failed")?;
                rate_limits_to_usage(result)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> RpcResult<Value> {
        self.write_json(&json!({ "id": id, "method": method, "params": params }))
            .await
            .map_err(RpcFailure::transport)?;

        loop {
            let line = timeout(RPC_TIMEOUT, self.lines.next_line())
                .await
                .map_err(|_| RpcFailure::transport(anyhow!("Codex app-server response timed out")))?
                .map_err(|error| RpcFailure::transport(anyhow!(error)))?
                .ok_or_else(|| RpcFailure::transport(anyhow!("Codex app-server closed stdout")))?;
            let message: Value = serde_json::from_str(&line)
                .map_err(|error| RpcFailure::transport(anyhow!("invalid Codex JSONL: {error}")))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(result) = message.get("result") {
                return Ok(result.clone());
            }
            if let Some(error) = message.get("error") {
                return Err(RpcFailure::remote(error.clone()));
            }
            return Err(RpcFailure::transport(anyhow!(
                "Codex app-server response missing result"
            )));
        }
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        self.write_json(&json!({ "method": method })).await
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let mut encoded = serde_json::to_vec(value)?;
        encoded.push(b'\n');
        self.stdin.write_all(&encoded).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn close(mut self) {
        drop(self.stdin);
        if timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}

type RpcResult<T> = std::result::Result<T, RpcFailure>;

#[derive(Debug)]
struct RpcFailure {
    message: String,
    remote: bool,
}

impl RpcFailure {
    fn transport(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string(),
            remote: false,
        }
    }

    fn remote(error: Value) -> Self {
        Self {
            message: error.to_string(),
            remote: true,
        }
    }

    fn is_authentication_failure(&self) -> bool {
        if !self.remote {
            return false;
        }
        let message = self.message.to_ascii_lowercase();
        ["401", "unauthorized", "authentication", "not logged in"]
            .iter()
            .any(|needle| message.contains(needle))
    }

    fn is_method_unsupported(&self) -> bool {
        if !self.remote {
            return false;
        }
        let message = self.message.to_ascii_lowercase();
        message.contains("-32601") || message.contains("method not found")
    }
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RpcFailure {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResponse {
    rate_limits: RateLimitSnapshot,
}

#[derive(Deserialize)]
struct RateLimitSnapshot {
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    used_percent: i64,
    window_duration_mins: Option<u64>,
    resets_at: Option<i64>,
}

fn rate_limits_to_usage(result: Value) -> Result<Value> {
    let response: RateLimitsResponse = serde_json::from_value(result)
        .context("Codex rate-limit response has an unsupported shape")?;
    let mut usage = serde_json::Map::new();
    if let Some(primary) = response.rate_limits.primary {
        usage.insert("primary".into(), window_to_usage(primary));
    }
    if let Some(secondary) = response.rate_limits.secondary {
        usage.insert("secondary".into(), window_to_usage(secondary));
    }
    if usage.is_empty() {
        return Err(anyhow!("Codex rate-limit response contains no windows"));
    }
    Ok(Value::Object(usage))
}

fn window_to_usage(window: RateLimitWindow) -> Value {
    json!({
        "used_percent": window.used_percent,
        "window_minutes": window.window_duration_mins,
        "resets_at": window.resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;

    #[test]
    fn parses_primary_and_secondary_windows() {
        let usage = rate_limits_to_usage(json!({
            "rateLimits": {
                "primary": {
                    "usedPercent": 17,
                    "windowDurationMins": 300,
                    "resetsAt": 1_800_000_000
                },
                "secondary": {
                    "usedPercent": 31,
                    "windowDurationMins": 10_080,
                    "resetsAt": 1_800_100_000
                }
            }
        }))
        .unwrap();
        assert_eq!(usage["primary"]["used_percent"], 17);
        assert_eq!(usage["primary"]["window_minutes"], 300);
        assert_eq!(usage["secondary"]["used_percent"], 31);
        assert_eq!(usage["secondary"]["window_minutes"], 10_080);
    }

    #[test]
    fn fallback_policy_blocks_rate_limit_and_remote_service_errors() {
        let rate_limited = anyhow::Error::new(RpcFailure::remote(json!({
            "code": -32000,
            "message": "HTTP 429 rate limited"
        })));
        assert!(!allows_compat_fallback(&rate_limited));

        let unavailable = anyhow!("Codex binary unavailable");
        assert!(allows_compat_fallback(&unavailable));

        let unauthorized = anyhow::Error::new(RpcFailure::remote(json!({
            "code": -32000,
            "message": "HTTP 401 unauthorized"
        })));
        assert!(allows_compat_fallback(&unauthorized));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_codex_refreshes_once_after_authentication_failure() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("codex");
        fs::write(
            &script,
            r#"#!/bin/sh
initialized=0
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}' ;;
    *'"method":"initialized"'*) initialized=1 ;;
    *'"id":2'*)
      if [ "$initialized" = 1 ]; then
        printf '%s\n' '{"id":2,"error":{"code":-32000,"message":"HTTP 401 unauthorized"}}'
      else
        printf '%s\n' '{"id":2,"error":{"code":-32001,"message":"initialized notification missing"}}'
      fi
      ;;
    *'"id":3'*'"refreshToken":true'*) printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}' ;;
    *'"id":4'*) printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":9,"windowDurationMins":300,"resetsAt":1800000000},"secondary":{"usedPercent":23,"windowDurationMins":10080,"resetsAt":1800100000}}}}' ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let usage = query_command(&script, &["app-server", "--stdio"], None, temp.path(), true)
            .await
            .unwrap();
        assert_eq!(usage["primary"]["used_percent"], 9);
        assert_eq!(usage["secondary"]["used_percent"], 23);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_codex_still_queries_but_does_not_force_refresh_after_401() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("codex");
        let refresh_marker = temp.path().join("forced-refresh");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}}}' ;;
    *'"id":2'*) printf '%s\n' '{{"id":2,"error":{{"code":-32000,"message":"HTTP 401 unauthorized"}}}}' ;;
    *'"id":3'*) : > '{}' ; printf '%s\n' '{{"id":3,"result":{{}}}}' ;;
  esac
done
"#,
                refresh_marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let error = query_command(
            &script,
            &["app-server", "--stdio"],
            None,
            temp.path(),
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("401 unauthorized"));
        assert!(
            !refresh_marker.exists(),
            "并发 Codex 存在时不得额外发起强制刷新"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_query_uses_sanitized_home_and_preserves_live_auth() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let live = tempfile::tempdir().unwrap();
        let live_auth = live.path().join("auth.json");
        fs::write(
            &live_auth,
            r#"{"auth_mode":"chatgpt","tokens":{"id_token":"id","access_token":"access","refresh_token":"live-secret","account_id":"account"}}"#,
        )
        .unwrap();

        let sanitized = SanitizedHome::create(live.path()).await.unwrap();
        let script = live.path().join("codex");
        fs::write(
            &script,
            r#"#!/bin/sh
if ! grep -q '"refresh_token":""' "$CODEX_HOME/auth.json"; then
  exit 41
fi
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}' ;;
    *'"id":2'*) printf '%s\n' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":12,"windowDurationMins":300,"resetsAt":1800000000}}}}' ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let usage = query_command(
            &script,
            &["app-server", "--stdio"],
            None,
            sanitized.path(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(usage["primary"]["used_percent"], 12);
        assert_eq!(
            fs::metadata(sanitized.path().join("auth.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let unchanged: Value = serde_json::from_slice(&fs::read(live_auth).unwrap()).unwrap();
        assert_eq!(
            unchanged["tokens"]["refresh_token"],
            Value::String("live-secret".into())
        );
    }

    fn fake_access_jwt(exp: i64) -> String {
        let payload = base64_url_encode(format!(r#"{{"exp":{exp},"email":"parked@example.com"}}"#).as_bytes());
        format!("hdr.{payload}.sig")
    }

    fn base64_url_encode(input: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut i = 0;
        while i < input.len() {
            let b0 = input[i];
            let b1 = input.get(i + 1).copied();
            let b2 = input.get(i + 2).copied();
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
            if b1.is_none() {
                break;
            }
            out.push(TABLE[(((b1.unwrap() & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char);
            if b2.is_none() {
                break;
            }
            out.push(TABLE[(b2.unwrap() & 0x3f) as usize] as char);
            i += 3;
        }
        out
    }

    fn parked_auth_blob(access: &str, refresh: &str) -> String {
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "id",
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct"
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn parked_refresh_skips_when_access_still_fresh() {
        let access = fake_access_jwt(chrono::Utc::now().timestamp() + 3600);
        let blob = parked_auth_blob(&access, "refresh-secret");
        let outcome = refresh_parked_blob(&blob).await.unwrap();
        assert!(matches!(outcome, RefreshOutcome::Unsupported));
    }

    #[tokio::test]
    async fn parked_refresh_skips_without_refresh_token() {
        let access = fake_access_jwt(chrono::Utc::now().timestamp() - 10);
        let blob = parked_auth_blob(&access, "");
        let outcome = refresh_parked_blob(&blob).await.unwrap();
        assert!(matches!(outcome, RefreshOutcome::Unsupported));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parked_refresh_absorbs_rotated_auth_from_isolated_home() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("codex");
        let new_access = fake_access_jwt(chrono::Utc::now().timestamp() + 7200);
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{{"id":1,"result":{{"codexHome":"/tmp","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}}}' ;;
    *'"id":2'*)
      printf '%s\n' '{{"auth_mode":"chatgpt","tokens":{{"id_token":"id","access_token":"{new_access}","refresh_token":"rotated-refresh","account_id":"acct"}}}}' > "$CODEX_HOME/auth.json"
      printf '%s\n' '{{"id":2,"result":{{"rateLimits":{{"primary":{{"usedPercent":4,"windowDurationMins":300,"resetsAt":1800000000}}}}}}}}'
      ;;
  esac
done
"#
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let old_access = fake_access_jwt(chrono::Utc::now().timestamp() - 60);
        let blob = parked_auth_blob(&old_access, "old-refresh");
        let outcome = refresh_parked_blob_with_binary(&blob, &script)
            .await
            .unwrap();

        match outcome {
            RefreshOutcome::Rotated(new_blob) => {
                assert!(new_blob.contains(&new_access));
                assert!(new_blob.contains("rotated-refresh"));
            }
            other => panic!(
                "expected Rotated, got {}",
                match other {
                    RefreshOutcome::DeadToken => "DeadToken",
                    RefreshOutcome::Unsupported => "Unsupported",
                    RefreshOutcome::Rotated(_) => "Rotated",
                }
            ),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parked_refresh_marks_dead_token_when_official_auth_fails() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("codex");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{"codexHome":"/tmp","platformFamily":"unix","platformOs":"linux","userAgent":"fake"}}' ;;
    *'"id":2'*) printf '%s\n' '{"id":2,"error":{"code":-32000,"message":"HTTP 401 unauthorized"}}' ;;
    *'"id":3'*) printf '%s\n' '{"id":3,"error":{"code":-32000,"message":"HTTP 401 unauthorized"}}' ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let old_access = fake_access_jwt(chrono::Utc::now().timestamp() - 60);
        let blob = parked_auth_blob(&old_access, "dead-refresh");
        let outcome = refresh_parked_blob_with_binary(&blob, &script)
            .await
            .unwrap();
        assert!(matches!(outcome, RefreshOutcome::DeadToken));
    }
}
