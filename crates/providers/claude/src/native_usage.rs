//! 官方 Claude Code 结构化额度通道；不提交模型消息，也不自行轮换当前号凭据。

use std::path::Path;
use std::process::Stdio;

use serde_json::{json, Value};
use subswap_core::error::{Error, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::oauth::UsageResponse;

/// 只接受已确认具备实验性结构化 usage 通道的客户端。
fn supported_version(text: &str) -> bool {
    let Some(version) = text.split_whitespace().next() else {
        return false;
    };
    let numbers: Vec<_> = version.split('.').map(str::parse::<u32>).collect();
    matches!(numbers.as_slice(), [Ok(major), Ok(minor), Ok(patch)] if (*major, *minor, *patch) >= (2, 1, 169))
}

pub async fn fetch(home: &Path) -> Result<UsageResponse> {
    let version = Command::new("claude")
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|_| Error::QuotaFetch("Claude Code CLI unavailable".into()))?;
    if !version.status.success() || !supported_version(&String::from_utf8_lossy(&version.stdout)) {
        return Err(Error::QuotaFetch(
            "Claude Code structured usage requires version 2.1.169 or later".into(),
        ));
    }
    let mut command = Command::new("claude");
    command
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--settings",
            "{\"disableAllHooks\":true}",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
        ])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // 标准目录不能强设 CLAUDE_CONFIG_DIR，否则官方全局 .claude.json 会移到不同位置。
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some()
        || directories::UserDirs::new()
            .map(|d| d.home_dir().join(".claude"))
            .as_deref()
            != Some(home)
    {
        command.env("CLAUDE_CONFIG_DIR", home);
    }
    // 账号归属来自原生登录，不能让启动 subswap 的 shell 带入另一份 API/代理凭据。
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDECODE",
    ] {
        command.env_remove(key);
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| Error::QuotaFetch("start Claude Code usage query failed".into()))?;
    let result = query(&mut child).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

async fn query(child: &mut Child) -> Result<UsageResponse> {
    let mut input = child.stdin.take().ok_or_else(protocol_error)?;
    let mut output = BufReader::new(child.stdout.take().ok_or_else(protocol_error)?);
    // 按 SDK 顺序初始化后再查；跳过本地历史扫描，额度查询不需读取用户会话内容。
    request(
        &mut input,
        &mut output,
        "subswap-init",
        json!({"subtype":"initialize"}),
    )
    .await?;
    let body = request(
        &mut input,
        &mut output,
        "subswap-usage",
        json!({"subtype":"get_usage","skip_behaviors":true}),
    )
    .await?;
    parse_usage(&body)
}

async fn request(
    input: &mut ChildStdin,
    output: &mut BufReader<ChildStdout>,
    id: &str,
    request: Value,
) -> Result<Value> {
    let message = json!({"type":"control_request","request_id":id,"request":request});
    input.write_all(format!("{message}\n").as_bytes()).await?;
    input.flush().await?;
    loop {
        let mut line = String::new();
        if output.read_line(&mut line).await? == 0 || line.len() > 1_048_576 {
            return Err(protocol_error());
        }
        let Ok(envelope) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if envelope["type"] != "control_response" || envelope["response"]["request_id"] != id {
            continue;
        }
        let response = &envelope["response"];
        if response["subtype"] != "success" {
            // 不透传原生错误正文，避免其中包含本地信息或凭据。
            return Err(Error::QuotaFetch(
                "Claude Code structured usage request failed".into(),
            ));
        }
        return response.get("response").cloned().ok_or_else(protocol_error);
    }
}

fn protocol_error() -> Error {
    Error::QuotaFetch("Claude Code usage protocol unavailable".into())
}

fn parse_usage(body: &Value) -> Result<UsageResponse> {
    if body["rate_limits_available"] == false {
        return Err(Error::QuotaFetch(
            "Claude subscription usage unavailable for this login".into(),
        ));
    }
    let limits = body
        .get("rate_limits")
        .filter(|v| v.is_object())
        .ok_or_else(|| {
            Error::QuotaFetch("Claude usage unavailable; native client returned no limits".into())
        })?;
    serde_json::from_value(limits.clone()).map_err(|_| protocol_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_empty_usage_is_unknown_not_a_fabricated_429() {
        let error = parse_usage(&json!({"rate_limits_available":true,"rate_limits":null}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unavailable"));
        assert!(!error.contains("429"));
        assert!(parse_usage(
            &json!({"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":12.0}}})
        )
        .unwrap()
        .five_hour
        .is_some());
    }

    #[test]
    fn version_gate_accepts_installed_client_and_rejects_old_or_unknown() {
        assert!(supported_version("2.1.295 (Claude Code)"));
        assert!(!supported_version("2.1.168 (Claude Code)"));
        assert!(!supported_version("unknown"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_session_initializes_and_sends_only_usage_without_history_scan() {
        let script = r#"
import sys,json
a=json.loads(sys.stdin.readline())
assert a['request']['subtype']=='initialize'
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':a['request_id'],'response':{}}}),flush=True)
b=json.loads(sys.stdin.readline())
assert b['type']=='control_request' and b['request']=={'subtype':'get_usage','skip_behaviors':True}
print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':b['request_id'],'response':{'rate_limits_available':True,'rate_limits':{'five_hour':{'utilization':23}}}}}),flush=True)
"#;
        let mut child = Command::new("python3")
            .args(["-u", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let usage = query(&mut child).await.unwrap();
        assert_eq!(usage.five_hour.unwrap().utilization, Some(23.0));
        assert!(child.wait().await.unwrap().success());
    }
}
