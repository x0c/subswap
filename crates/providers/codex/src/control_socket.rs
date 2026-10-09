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

async fn query(socket: &Path, expected_account_id: &str) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .await
        .context("connect Codex control socket")?;
    let (stream, _) = client_async("ws://localhost/", stream)
        .await
        .context("upgrade Codex control socket to WebSocket")?;
    let mut session = Session { stream };
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
    ) -> (Result<Value>, Vec<String>) {
        let temp = tempfile::tempdir().unwrap();
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
        let result = fetch_usage(&socket, "current").await;
        let methods = server.await.unwrap();
        (result, methods)
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
        let (result, methods) = exercise(Some("current"), quota(Some("current")), false).await;
        let usage = result.unwrap();
        assert_eq!(usage["primary"]["used_percent"], 13);
        assert_eq!(usage["rate_limit_reset_credits"]["available_count"], 2);
        assert_eq!(
            usage["rate_limit_reset_credits"]["credits"][0]["expiresAt"],
            1800000000
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
        let (result, methods) = exercise(Some("previous"), quota(Some("previous")), false).await;
        assert!(result.unwrap_err().is::<AccountMismatch>());
        assert_eq!(methods, ["initialize", "initialized", "account/read"]);
    }

    #[tokio::test]
    async fn optional_preflight_never_replaces_final_account_validation() {
        for unsupported in [false, true] {
            for account in [None, Some("previous"), Some("current")] {
                let (result, _) = exercise(None, quota(account), unsupported).await;
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
            let (result, methods) = exercise(
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
}
