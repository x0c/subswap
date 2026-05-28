//! `subswap login <provider>`：调用厂商官方 CLI 走原生登录流程，再 import 到 subswap。
//!
//! 不复刻 OAuth 流程的动机见 docs/design/ARCHITECTURE.md §3.2。

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use subswap_core::AuditEvent;

use crate::app::AppContext;
use crate::render::account_ref;

pub async fn run(
    ctx: &AppContext,
    provider: &str,
    email: Option<String>,
    sso: bool,
    device_auth: bool,
    extra_args: Vec<String>,
) -> Result<()> {
    match provider {
        "claude" | "anthropic" => {
            if device_auth {
                bail!("--device-auth is only supported for codex login");
            }
            let mut args = vec!["auth".into(), "login".into(), "--claudeai".into()];
            if let Some(email) = email {
                args.push("--email".into());
                args.push(email);
            }
            if sso {
                args.push("--sso".into());
            }
            args.extend(extra_args);
            run_native_login("claude", args).await?;

            let account = ctx
                .claude
                .import_active(None)
                .context("import Claude login")?;
            ctx.registry
                .set_active("claude", &account.id)
                .context("mark Claude login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "claude",
                Some(account.id.0.as_str()),
            ));
            println!("login → claude/{}", account_ref(&account.id.0));
            Ok(())
        }
        "codex" | "openai" | "chatgpt" => {
            if email.is_some() || sso {
                bail!("--email and --sso are only supported for claude login");
            }
            let mut args = vec!["login".into()];
            if device_auth {
                args.push("--device-auth".into());
            }
            args.extend(extra_args);
            run_native_login("codex", args).await?;

            let account = ctx
                .codex
                .import_active(None)
                .context("import Codex login")?;
            ctx.registry
                .set_active("codex", &account.id)
                .context("mark Codex login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "codex",
                Some(account.id.0.as_str()),
            ));
            println!("login → codex/{}", account_ref(&account.id.0));
            Ok(())
        }
        other => bail!("unknown provider: {other} (expected claude or codex)"),
    }
}

async fn run_native_login(program: &'static str, args: Vec<String>) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let display = command_display(program, &args);

        // 直接打开控制终端,绕开 tokio runtime / tracing-subscriber 对父进程
        // fd 0/1/2 可能造成的状态污染(非阻塞标志、行缓冲等)。
        // 在没有控制终端的环境下(pipe/no-tty)退回到 Stdio::inherit。
        let (stdin, stdout, stderr) = open_controlling_tty_for_child();

        let status = Command::new(program)
            .args(&args)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .status()
            .with_context(|| format!("failed to start `{display}`"))?;
        if !status.success() {
            bail!("native login failed: `{display}` exited with {status}");
        }
        Ok(())
    })
    .await
    .context("native login task failed")?
}

/// 尽量让子进程拿到对 `/dev/tty` 的全新句柄。任何一步失败都安全退回到
/// `Stdio::inherit()`,这样在没有 TTY 的场景(CI / 管道)下行为不变。
fn open_controlling_tty_for_child() -> (Stdio, Stdio, Stdio) {
    let tty_in = std::fs::OpenOptions::new().read(true).open("/dev/tty").ok();
    let tty_out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok();
    let tty_err = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok();
    match (tty_in, tty_out, tty_err) {
        (Some(i), Some(o), Some(e)) => (Stdio::from(i), Stdio::from(o), Stdio::from(e)),
        _ => (Stdio::inherit(), Stdio::inherit(), Stdio::inherit()),
    }
}

fn command_display(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(program.to_string());
    parts.extend(args.iter().map(|arg| shellish_quote(arg)));
    parts.join(" ")
}

fn shellish_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("{value:?}")
    }
}
