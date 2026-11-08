//! Codex Unix 控制通道的 WebSocket 客户端；不启动代理进程，也不强制刷新账号。

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async, WebSocketStream};

use crate::app_server::{rate_limits_to_usage, AccountMismatch, RpcFailure};

const RPC_TIMEOUT: Duration = Duration::from_secs(8);

pub(super) async fn fetch_usage(socket: &Path, expected_account_id: &str) -> Result<Value> {
    timeout(RPC_TIMEOUT, query(socket, expected_account_id))
        .await
        .context("Codex control socket query timed out")?
}

/// `auth --renew` 的 active socket 续期：复用官方 initialize/account-read 协调，
/// 预检 `account/read {refreshToken:false}` 的 `workspaceRouting.chatgptAccountId` 必须
/// 等于预期账号；已知不匹配或缺失不可证身份时拒绝强刷（防 stale 轮换）。
/// 强刷只发 `account/read {refreshToken:true}`，绝不查 `account/rateLimits/read`。
/// 成功后官方进程已更新 live 文件；调用方重读 live 并校验归属后再投影。
pub(super) async fn refresh_active(socket: &Path, expected_account_id: &str) -> Result<()> {
    timeout(RPC_TIMEOUT, refresh_query(socket, expected_account_id))
        .await
        .context("Codex control socket renewal timed out")?
}

async fn refresh_query(socket: &Path, expected_account_id: &str) -> Result<()> {
    if expected_account_id.trim().is_empty() {
        return Err(anyhow!(
            "cannot prove Codex socket identity for renewal; refusing active refresh"
        ));
    }
    let stream = UnixStream::connect(socket)
        .await
        .context("connect Codex control socket")?;
    let (stream, _) = client_async("ws://localhost/", stream)
        .await
        .context("upgrade Codex control socket to WebSocket")?;
    let mut session = Session {
        stream,
        metrics_account: expected_account_id.to_string(),
    };
    session
        .request(
            1,
            "initialize",
            json!({
                "clientInfo": {"name": "subswap", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )
        .await?;
    session.send(json!({"method": "initialized"})).await?;

    match session
        .request(2, "account/read", json!({"refreshToken": false}))
        .await
    {
        Ok(account) => {
            match account
                .pointer("/workspaceRouting/chatgptAccountId")
                .and_then(Value::as_str)
            {
                Some(id) if id == expected_account_id => {}
                _ => return Err(AccountMismatch.into()),
            }
        }
        Err(_) => {
            // 预检失败（方法不支持/401/其他）都视为不可证身份：拒绝强刷。
            return Err(AccountMismatch.into());
        }
    }
    session
        .request(3, "account/read", json!({"refreshToken": true}))
        .await
        .map(|_| ())
        .map_err(|error| anyhow!(error.to_string()).context("Codex token refresh failed"))
}

async fn query(socket: &Path, expected_account_id: &str) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .await
        .context("connect Codex control socket")?;
    let (stream, _) = client_async("ws://localhost/", stream)
        .await
        .context("upgrade Codex control socket to WebSocket")?;
    let mut session = Session {
        stream,
        metrics_account: expected_account_id.to_string(),
    };
    session
        .request(
            1,
            "initialize",
            json!({
                "clientInfo": {"name": "subswap", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )
        .await?;
    session.send(json!({"method": "initialized"})).await?;

    // workspaceRouting 是官方可选身份；缺失/旧版不支持时，仍靠最终额度 accountId 兜底。
    match session
        .request(2, "account/read", json!({"refreshToken": false}))
        .await
    {
        Ok(account) => {
            if let Some(id) = account
                .pointer("/workspaceRouting/chatgptAccountId")
                .and_then(Value::as_str)
            {
                if id != expected_account_id {
                    return Err(AccountMismatch.into());
                }
            }
        }
        Err(error) => {
            // 方法不支持才跳过；401、429、服务错误保留原来的回退分类。
            if !error.is_method_unsupported() {
                return Err(error.into());
            }
        }
    }
    let result = session
        .request(3, "account/rateLimits/read", Value::Null)
        .await?;
    rate_limits_to_usage(result, expected_account_id)
}

struct Session {
    stream: WebSocketStream<UnixStream>,
    metrics_account: String,
}

impl Session {
    async fn send(&mut self, value: Value) -> Result<()> {
        self.stream
            .send(Message::Text(value.to_string().into()))
            .await?;
        Ok(())
    }

    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcFailure> {
        subswap_core::request_metrics::record(
            "codex",
            &self.metrics_account,
            "native_rpc_start",
            None,
        )
        .await;
        let result = self.request_inner(id, method, params).await;
        subswap_core::request_metrics::record(
            "codex",
            &self.metrics_account,
            "native_rpc_complete",
            None,
        )
        .await;
        result
    }

    async fn request_inner(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcFailure> {
        self.send(json!({"id": id, "method": method, "params": params}))
            .await
            .map_err(RpcFailure::transport)?;
        loop {
            let frame = self
                .stream
                .next()
                .await
                .ok_or_else(|| RpcFailure::transport(anyhow!("Codex control socket closed")))?
                .map_err(|error| RpcFailure::transport(error.into()))?;
            let message = match frame {
                Message::Text(text) => serde_json::from_str::<Value>(&text)
                    .map_err(|error| RpcFailure::transport(error.into()))?,
                Message::Ping(_) => {
                    // tungstenite 已排队对应的 pong，及时发送而不重启超时预算。
                    self.stream
                        .flush()
                        .await
                        .map_err(|error| RpcFailure::transport(error.into()))?;
                    continue;
                }
                Message::Close(_) => {
                    return Err(RpcFailure::transport(anyhow!(
                        "Codex control socket closed"
                    )))
                }
                _ => continue,
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(RpcFailure::remote(error.clone()));
            }
            return message.get("result").cloned().ok_or_else(|| {
                RpcFailure::transport(anyhow!("Codex control response missing result"))
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;
    use tokio_tungstenite::accept_async;

    async fn exercise(
        routing: Option<&str>,
        quota: Value,
        unsupported_preflight: bool,
    ) -> (Result<Value>, Vec<String>, Value) {
        let temp = tempfile::tempdir().unwrap();
        let metrics_path = temp.path().join("request_metrics.json");
        let socket = temp.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let routing = routing.map(str::to_owned);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let message: Value = serde_json::from_str(&text).unwrap();
                let method = message["method"].as_str().unwrap();
                methods.push(method.to_owned());
                let id = message["id"].clone();
                let reply = match method {
                    "initialize" => json!({"id": id, "result": {}}),
                    "initialized" => continue,
                    "account/read" => {
                        assert_eq!(message["params"]["refreshToken"], false);
                        if unsupported_preflight {
                            json!({"id": id, "error": {"code": -32601, "message": "method not found"}})
                        } else {
                            json!({"id": id, "result": {"workspaceRouting": routing.as_ref().map(|id| json!({"chatgptAccountId": id}))}})
                        }
                    }
                    "account/rateLimits/read" => {
                        ws.send(Message::Ping(vec![1, 2].into())).await.unwrap();
                        ws.send(Message::Text(
                            json!({"method": "account/updated", "params": {}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                        let mut reply = quota.clone();
                        reply["id"] = id;
                        reply
                    }
                    _ => panic!("unexpected request: {method}"),
                };
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            }
            methods
        });
        let result = subswap_core::request_metrics::with_sink(
            metrics_path.clone(),
            fetch_usage(&socket, "current"),
        )
        .await;
        let metrics = subswap_core::request_metrics::snapshot_at(&metrics_path);
        let methods = server.await.unwrap();
        (result, methods, metrics)
    }

    fn quota(account: Option<&str>) -> Value {
        json!({"result": {
            "accountId": account,
            "rateLimits": {"primary": {"usedPercent": 13, "windowDurationMins": 300}},
            "rateLimitResetCredits": {"availableCount": 2, "credits": [{
                "id": "reset", "status": "available", "expiresAt": 1800000000, "title": "Reset"
            }]}
        }})
    }

    #[tokio::test]
    async fn websocket_queries_matching_account_and_preserves_inline_details() {
        let (result, methods, metrics) =
            exercise(Some("current"), quota(Some("current")), false).await;
        let usage = result.unwrap();
        assert_eq!(usage["primary"]["used_percent"], 13);
        assert_eq!(usage["rate_limit_reset_credits"]["available_count"], 2);
        assert_eq!(
            usage["rate_limit_reset_credits"]["credits"][0]["expiresAt"],
            1800000000
        );
        assert_eq!(metrics["totals"]["requests"]["native_rpc_start"], 3);
        assert_eq!(metrics["totals"]["requests"]["native_rpc_complete"], 3);
        assert_eq!(
            metrics["totals"]["requests"]["http_start"], 0,
            "native RPC must not be reported as HTTP"
        );
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "account/read",
                "account/rateLimits/read"
            ]
        );
    }

    #[tokio::test]
    async fn stale_daemon_is_rejected_before_requesting_or_refreshing_quota() {
        let (result, methods, metrics) =
            exercise(Some("previous"), quota(Some("previous")), false).await;
        assert!(result.unwrap_err().is::<AccountMismatch>());
        assert_eq!(metrics["totals"]["requests"]["native_rpc_start"], 2);
        assert_eq!(metrics["totals"]["requests"]["native_rpc_complete"], 2);
        assert_eq!(methods, ["initialize", "initialized", "account/read"]);
    }

    #[tokio::test]
    async fn optional_preflight_never_replaces_final_account_validation() {
        for unsupported in [false, true] {
            for account in [None, Some("previous"), Some("current")] {
                let (result, _, _) = exercise(None, quota(account), unsupported).await;
                assert_eq!(result.is_ok(), account == Some("current"));
                if let Err(error) = result {
                    assert!(error.is::<AccountMismatch>());
                }
            }
        }
    }

    #[tokio::test]
    async fn remote_errors_preserve_fallback_and_refresh_boundaries() {
        for (message, allowed) in [
            ("HTTP 429 rate limited", false),
            ("HTTP 503 unavailable", false),
            ("HTTP 401 unauthorized", true),
        ] {
            let (result, methods, _) = exercise(
                Some("current"),
                json!({"error": {"code": -32000, "message": message}}),
                false,
            )
            .await;
            let error = result.unwrap_err();
            assert_eq!(crate::app_server::allows_compat_fallback(&error), allowed);
            assert_eq!(
                methods
                    .iter()
                    .filter(|method| *method == "account/rateLimits/read")
                    .count(),
                1
            );
            assert!(!methods.iter().any(|method| method == "account/login/start"));
        }
    }

    #[tokio::test]
    async fn silent_control_socket_has_one_bounded_budget() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ws = accept_async(stream).await.unwrap();
            std::future::pending::<()>().await;
        });
        let error = fetch_usage(&socket, "current").await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(crate::app_server::allows_compat_fallback(&error));
        server.abort();
    }

    async fn exercise_refresh(
        routing: Option<&str>,
        unsupported_preflight: bool,
    ) -> (Result<()>, Vec<String>, Vec<Value>) {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let routing = routing.map(str::to_owned);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            let mut params = Vec::new();
            let mut forced = false;
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let message: Value = serde_json::from_str(&text).unwrap();
                let method = message["method"].as_str().unwrap();
                methods.push(method.to_owned());
                params.push(message["params"].clone());
                let id = message["id"].clone();
                let is_forced_read = method == "account/read"
                    && message.get("params").and_then(|p| p.get("refreshToken"))
                        == Some(&Value::Bool(true));
                let reply = match method {
                    "initialize" => json!({"id": id, "result": {}}),
                    "initialized" => continue,
                    "account/read" => {
                        if is_forced_read {
                            forced = true;
                            json!({"id": id, "result": {"account": {"type": "chatgpt"}}})
                        } else if unsupported_preflight {
                            json!({"id": id, "error": {"code": -32601, "message": "method not found"}})
                        } else {
                            json!({"id": id, "result": {"workspaceRouting": routing.as_ref().map(|id| json!({"chatgptAccountId": id}))}})
                        }
                    }
                    _ => panic!("renewal must never request {method}"),
                };
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                if forced && methods.iter().filter(|m| *m == "account/read").count() == 2 {
                    break;
                }
            }
            (methods, params)
        });
        let result = refresh_active(&socket, "current").await;
        let (methods, params) = server.await.unwrap();
        (result, methods, params)
    }

    #[tokio::test]
    async fn renewal_forces_refresh_only_with_matching_preflight_identity() {
        let (result, methods, params) = exercise_refresh(Some("current"), false).await;
        result.unwrap();
        assert_eq!(
            methods,
            ["initialize", "initialized", "account/read", "account/read"]
        );
        assert_eq!(params[2]["refreshToken"], false);
        assert_eq!(params[3]["refreshToken"], true);
        assert!(
            !methods
                .iter()
                .any(|m| m.contains("rateLimits") || m.contains("rate_limits")),
            "续期绝不能查额度"
        );
    }

    #[tokio::test]
    async fn renewal_refuses_mismatched_or_unproven_identity_before_forced_refresh() {
        for (routing, unsupported) in [
            (Some("previous"), false),
            (None, false),
            (None, true),
            (Some("current"), true),
        ] {
            let (result, methods, params) = exercise_refresh(routing, unsupported).await;
            let error = result.unwrap_err();
            assert!(
                error.is::<AccountMismatch>()
                    || error.to_string().contains("refusing")
                    || error.to_string().contains("AccountMismatch")
                    || error.to_string().contains("does not match"),
                "不可证身份必须拒绝强刷，got: {error}"
            );
            assert_eq!(methods, ["initialize", "initialized", "account/read"]);
            assert!(
                !params
                    .iter()
                    .any(|p| p.get("refreshToken") == Some(&Value::Bool(true))),
                "拒绝路径绝不能发起 refreshToken:true"
            );
        }
    }
}
