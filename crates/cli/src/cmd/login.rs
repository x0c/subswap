//! `subswap login <provider>`：调用厂商官方 CLI 走原生登录流程，再 import 到 subswap。
//!
//! 不复刻 OAuth 流程的动机见 docs/design/ARCHITECTURE.md §3.2。

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use subswap_core::{AuditEvent, Provider};

use crate::app::AppContext;
use crate::cmd::default::print_status_overview;
use crate::render::account_ref;

pub async fn run(
    ctx: &AppContext,
    provider: &str,
    email: Option<String>,
    sso: bool,
    device_auth: bool,
    extra_args: Vec<String>,
    json: bool,
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
            if let Err(e) = subswap_core::record_manual_swap("claude") {
                tracing::warn!(err = %e, provider = "claude", "record manual hold failed");
            }
            return finish(ctx, json).await;
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
            ctx.codex
                .activate(&account.id)
                .await
                .context("activate Codex login and restore OAuth configuration")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "codex",
                Some(account.id.0.as_str()),
            ));
            println!("login → codex/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("codex") {
                tracing::warn!(err = %e, provider = "codex", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "kimi" | "moonshot" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for kimi login");
            }
            // Kimi 登录是交互式 TUI：约定用户先在 kimi 里登录好，这里只导入当前登录的凭证。
            let account = ctx
                .kimi
                .import_active(None)
                .context("import Kimi login; run `kimi` and sign in first")?;
            ctx.registry
                .set_active("kimi", &account.id)
                .context("mark Kimi login active")?;
            ctx.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            println!("login → kimi/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("kimi") {
                tracing::warn!(err = %e, provider = "kimi", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "opencode" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for opencode login");
            }
            if !extra_args.is_empty() {
                bail!("API keys use `subswap login opencode-api-key -- <key>`");
            }
            let home = ctx.opencode.go_engine().home();
            let signed_in = tokio::task::spawn_blocking(move || {
                subswap_provider_opencode::console::read_console_live(&home)
                    .map(|live| live.is_some())
            })
            .await
            .context("read OpenCode Console login task failed")?
            .context("read OpenCode Console login")?;
            if !signed_in {
                // 未登录：调官方命令走原生登录流程，再导入 Console 账号。
                let major = tokio::task::spawn_blocking(
                    subswap_provider_opencode::console::detect_major_version,
                )
                .await
                .context("detect OpenCode version task failed")?
                .context("detect OpenCode version")?;
                let args = subswap_provider_opencode::console::login_args(major);
                run_native_login("opencode", args).await?;
            }
            let console = ctx.opencode.clone();
            let account = tokio::task::spawn_blocking(move || console.import_console_active(None))
                .await
                .context("import OpenCode Console task failed")?
                .context("import OpenCode Console login")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "opencode",
                Some(account.id.0.as_str()),
            ));
            println!("login → opencode/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("opencode") {
                tracing::warn!(err = %e, provider = "opencode", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "opencode-api-key" | "opencode-go" => {
            if email.is_some() || sso || device_auth {
                bail!("login options are not supported for opencode-api-key");
            }
            if extra_args.len() > 1 {
                bail!("expected one OpenCode API key after `--`");
            }
            let account = if let Some(key) = extra_args
                .first()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let blob = subswap_provider_opencode::blob_from_key(key);
                let key_provider = ctx.opencode_api_key.clone();
                let account = tokio::task::spawn_blocking(move || {
                    key_provider.import_raw(blob, None, Some(false))
                })
                .await
                .context("import OpenCode API key task failed")?
                .context("import OpenCode API key")?;
                ctx.opencode_api_key
                    .activate(&account.id)
                    .await
                    .context("select OpenCode API key")?;
                Some(account)
            } else {
                let key_provider = ctx.opencode_api_key.clone();
                let accounts =
                    tokio::task::spawn_blocking(move || key_provider.sync_accounts(None))
                        .await
                        .context("import OpenCode API keys task failed")?
                        .context("import OpenCode API keys from the official client")?;
                if accounts.is_empty() {
                    bail!("no OpenCode API key found; run `opencode auth login opencode-go` or pass a key after `--`");
                }
                accounts.into_iter().find(|a| a.active)
            };
            if let Some(account) = account {
                ctx.audit.append(AuditEvent::ok(
                    "login",
                    "opencode-api-key",
                    Some(account.id.0.as_str()),
                ));
                println!("login → opencode-api-key/{}", account_ref(&account.id.0));
                if let Err(e) = subswap_core::record_manual_swap("opencode-api-key") {
                    tracing::warn!(err = %e, provider = "opencode-api-key", "record manual hold failed");
                }
            } else {
                ctx.audit
                    .append(AuditEvent::ok("login", "opencode-api-key", None));
                println!("imported OpenCode API keys (none active)");
            }
            return finish(ctx, json).await;
        }
        "commandcode" | "command-code" | "cmd" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for commandcode login");
            }
            let account = if let Some(key) = extra_args
                .first()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let blob = subswap_provider_commandcode::blob_from_key(key);
                let account = ctx
                    .commandcode
                    .import_raw(blob, None, Some(true))
                    .context("import Command Code API key")?;
                ctx.commandcode
                    .activate(&account.id)
                    .await
                    .context("write Command Code key into auth.json")?;
                account
            } else {
                ctx.commandcode.import_active(None).context(
                    "import Command Code login; run `command-code login` or pass the API key after `--`",
                )?
            };
            ctx.registry
                .set_active("commandcode", &account.id)
                .context("mark Command Code login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "commandcode",
                Some(account.id.0.as_str()),
            ));
            println!("login → commandcode/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("commandcode") {
                tracing::warn!(err = %e, provider = "commandcode", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "cursor" => {
            if email.is_some() || sso || device_auth || !extra_args.is_empty() {
                bail!("login options are not supported for cursor login");
            }
            let account = ctx
                .cursor
                .import_active(None)
                .await
                .context("import Cursor login; sign in to Cursor first")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "cursor",
                Some(account.id.0.as_str()),
            ));
            println!("login → cursor/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("cursor") {
                tracing::warn!(err = %e, provider = "cursor", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "antigravity" | "gemini" | "agy" => {
            if email.is_some() || sso || device_auth || !extra_args.is_empty() {
                bail!("login options are not supported for antigravity login; run `agy` and sign in first, then `subswap login antigravity`");
            }
            // agy 登录是交互式（`agy` TUI 内完成 Google OAuth）：约定用户先在 agy 里登录好，
            // 这里只导入当前钥匙串凭证。
            let account = ctx
                .antigravity
                .import_active(None)
                .context("import Antigravity login; run `agy` and sign in first")?;
            ctx.registry
                .set_active("antigravity", &account.id)
                .context("mark Antigravity login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "antigravity",
                Some(account.id.0.as_str()),
            ));
            println!("login → antigravity/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("antigravity") {
                tracing::warn!(err = %e, provider = "antigravity", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "volcengine" | "ark" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for volcengine login");
            }
            let (inference_key, access_key_id, secret_access_key) =
                parse_volcengine_login_args(&extra_args)?;
            let account = if let Some(key) = inference_key {
                let imported = ctx.volcengine.import_key(&key, None)?;
                if let (Some(ak), Some(sk)) = (access_key_id.clone(), secret_access_key.clone()) {
                    ctx.volcengine
                        .set_quota_credentials(&imported.id, &ak, &sk)
                        .context("save Volcengine quota AK/SK")?;
                }
                ctx.volcengine
                    .activate(&imported.id)
                    .await
                    .context("activate Volcengine Coding Plan account")?;
                imported
            } else if access_key_id.is_some() || secret_access_key.is_some() {
                // 只更新 AK/SK：优先当前 live 账号，否则取 registry 中 active 的 volcengine 账号。
                let access_key_id = access_key_id
                    .context("missing --ak-id (usage: `-- --ak-id <AK> --ak-secret <SK>`)")?;
                let secret_access_key = secret_access_key
                    .context("missing --ak-secret (usage: `-- --ak-id <AK> --ak-secret <SK>`)")?;
                let target_id = match ctx.volcengine.live_account_id() {
                    Ok(id) => id,
                    Err(_) => ctx
                        .registry
                        .list_by_provider("volcengine")
                        .context("list Volcengine accounts")?
                        .into_iter()
                        .find(|account| account.active)
                        .map(|account| account.id)
                        .context(
                            "no active Volcengine account; pass an API key after `--` first",
                        )?,
                };
                ctx.volcengine
                    .set_quota_credentials(&target_id, &access_key_id, &secret_access_key)
                    .context("save Volcengine quota AK/SK")?;
                ctx.registry
                    .find("volcengine", &target_id)?
                    .context("Volcengine account did not persist")?
            } else {
                let imported = ctx.volcengine.import_active().context(
                    "no Volcengine Coding Plan configuration in Claude settings; pass an API key after `--`",
                )?;
                ctx.volcengine
                    .activate(&imported.id)
                    .await
                    .context("activate Volcengine Coding Plan account")?;
                imported
            };
            ctx.registry
                .find("volcengine", &account.id)?
                .context("Volcengine login did not persist")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "volcengine",
                Some(account.id.0.as_str()),
            ));
            println!("login → volcengine/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("volcengine") {
                tracing::warn!(err = %e, provider = "volcengine", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        other => {
            bail!("unknown provider: {other} (expected claude, codex, kimi, cursor, opencode, opencode-api-key, commandcode, volcengine or antigravity)")
        }
    }
}

async fn finish(ctx: &AppContext, json: bool) -> Result<()> {
    if !json {
        print_status_overview(ctx).await?;
    }
    Ok(())
}

/// 解析 volcengine 登录参数：至多一个位置参数（推理 Key）+ 可选 AK/SK flags。
///
/// 用法：
/// - `subswap login volcengine -- <key>`
/// - `subswap login volcengine -- <key> --ak-id <AK> --ak-secret <SK>`
/// - `subswap login volcengine -- --ak-id <AK> --ak-secret <SK>`（只补 AK/SK）
fn parse_volcengine_login_args(
    args: &[String],
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let mut inference_key: Option<String> = None;
    let mut access_key_id: Option<String> = None;
    let mut secret_access_key: Option<String> = None;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let trimmed = arg.trim();
        if let Some(value) = trimmed
            .strip_prefix("--ak-id=")
            .or_else(|| trimmed.strip_prefix("--access-key-id="))
            .or_else(|| trimmed.strip_prefix("--ak="))
        {
            access_key_id = Some(value.trim().to_string());
            continue;
        }
        if let Some(value) = trimmed
            .strip_prefix("--ak-secret=")
            .or_else(|| trimmed.strip_prefix("--secret-access-key="))
            .or_else(|| trimmed.strip_prefix("--sk="))
            .or_else(|| trimmed.strip_prefix("--secret="))
        {
            secret_access_key = Some(value.trim().to_string());
            continue;
        }
        match trimmed {
            "--ak-id" | "--access-key-id" | "--ak" => {
                let value = iter.next().context(format!("{trimmed} requires a value"))?;
                let value = value.trim();
                if value.is_empty() || value.starts_with("--") {
                    bail!("{trimmed} requires a value");
                }
                access_key_id = Some(value.to_string());
            }
            "--ak-secret" | "--secret-access-key" | "--sk" | "--secret" => {
                let value = iter.next().context(format!("{trimmed} requires a value"))?;
                let value = value.trim();
                if value.is_empty() || value.starts_with("--") {
                    bail!("{trimmed} requires a value");
                }
                secret_access_key = Some(value.to_string());
            }
            _ => {
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with("--") {
                    bail!(
                        "unknown flag `{trimmed}` (expected --ak-id / --ak-secret or one API key)"
                    );
                }
                if inference_key.is_some() {
                    bail!("expected at most one Volcengine Coding Plan API key after `--`");
                }
                inference_key = Some(trimmed.to_string());
            }
        }
    }
    if let Some(key) = inference_key.as_ref() {
        if key.trim().is_empty() {
            inference_key = None;
        }
    }
    if access_key_id
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        access_key_id = None;
    }
    if secret_access_key
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        secret_access_key = None;
    }
    if access_key_id.is_some() ^ secret_access_key.is_some() {
        bail!("--ak-id and --ak-secret must be provided together");
    }
    Ok((inference_key, access_key_id, secret_access_key))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_key_only() {
        let (key, ak, sk) = parse_volcengine_login_args(&args(&["my-key"])).unwrap();
        assert_eq!(key.as_deref(), Some("my-key"));
        assert!(ak.is_none() && sk.is_none());
    }

    #[test]
    fn parses_key_with_aksk_flags() {
        let (key, ak, sk) =
            parse_volcengine_login_args(&args(&["my-key", "--ak-id", "AK", "--ak-secret", "SK"]))
                .unwrap();
        assert_eq!(key.as_deref(), Some("my-key"));
        assert_eq!(ak.as_deref(), Some("AK"));
        assert_eq!(sk.as_deref(), Some("SK"));
    }

    #[test]
    fn parses_aksk_only_and_equals_forms() {
        let (key, ak, _) =
            parse_volcengine_login_args(&args(&["--ak-id", "AK", "--ak-secret", "SK"])).unwrap();
        assert!(key.is_none());
        assert_eq!(ak.as_deref(), Some("AK"));

        let (_, ak, sk) =
            parse_volcengine_login_args(&args(&["--ak-id=AK2", "--ak-secret=SK2"])).unwrap();
        assert_eq!(ak.as_deref(), Some("AK2"));
        assert_eq!(sk.as_deref(), Some("SK2"));
    }

    #[test]
    fn rejects_half_aksk_unknown_flag_and_flag_like_value() {
        assert!(parse_volcengine_login_args(&args(&["--ak-id", "AK"])).is_err());
        assert!(parse_volcengine_login_args(&args(&["--unknown"])).is_err());
        assert!(parse_volcengine_login_args(&args(&["--ak-id", "--ak-secret"])).is_err());
        assert!(parse_volcengine_login_args(&args(&["k1", "k2"])).is_err());
    }
}
