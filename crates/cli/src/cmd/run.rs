//! `subswap run / shell / env`：账号隔离的私有环境。
//!
//! 不动全局活账号：把指定账号的凭证投影到私有目录，用环境变量让子进程只看自己的目录
//! （Codex 用 `CODEX_HOME`；Claude 用 `CLAUDE_CONFIG_DIR` + macOS `CLAUDE_SECURESTORAGE_CONFIG_DIR`），
//! 从而在不同终端用不同账号并行。设计见 docs/design/ACCOUNT_ISOLATION_DESIGN.md。
//!
//! - `run <provider> <id> [-- args]`：在隔离环境里启动该 provider 的原生 CLI。
//! - `shell <id>`：起一个导出好隔离环境变量的子 shell，交互里连跑多条命令；退出时吸收凭证。
//! - `env <id>`：打印 export 行供 `eval`。**注意**：eval 模式无法持锁、退出后不吸收凭证，属便捷/进阶用法。

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use subswap_core::checkout::Checkout;
use subswap_core::paths::AppPaths;
use subswap_core::Account;

use crate::app::AppContext;
use crate::cmd::resolve_account;

/// `subswap run <provider> <id> [-- args]`。
pub async fn run(
    ctx: &AppContext,
    provider: &str,
    id_input: &str,
    args: Vec<String>,
) -> Result<()> {
    let want = normalize_provider(provider)?;
    let acc = resolve_account(ctx, id_input)?;
    if acc.provider != want {
        bail!(
            "{}/{} is not a {want} account; `run {want}` only accepts {want} accounts",
            acc.provider,
            acc.id
        );
    }
    let program = native_cli(&acc.provider);
    launch(ctx, &acc, program, args).await
}

/// `subswap shell <id>`：起一个隔离子 shell。provider 从账号推断。
pub async fn shell(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    // 子 shell 走 inherit，交互可连跑多条命令；退出后照常吸收凭证。
    launch_program(ctx, &acc, shell, Vec::new()).await
}

/// `subswap env <id>`：打印 export 行供 `eval`。无法持锁、退出后不吸收（见模块文档）。
pub async fn env(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;
    warn_if_global_active(&acc);

    // API 账号：直接打印 env vars，无需物化目录与 checkout 锁。
    if acc.provider == "claude" {
        if let Some(api_vars) = ctx.claude.api_run_env_vars(&acc.id)? {
            for (k, v) in &api_vars {
                println!("export {k}={}", shell_quote(v));
            }
            return Ok(());
        }
    }

    let paths = AppPaths::resolve()?;
    // 仍走一次 checkout 校验「当前没有其他隔离会话占用」；函数返回即释放锁。
    let checkout = Checkout::acquire(&paths.data_dir, &acc.provider, acc.id.0.as_str())?;
    let env_dir = checkout.env_dir().to_path_buf();
    materialize(ctx, &acc, &env_dir)?;

    for (k, v) in env_vars(&acc.provider, &env_dir) {
        println!("export {k}={}", shell_quote(&v));
    }
    eprintln!(
        "note: eval mode does not hold a checkout lock and will NOT absorb rotated credentials \
         back into subswap after the session; prefer `subswap run`/`shell` for long sessions."
    );
    Ok(())
}

/// `run` 路径：在隔离环境里启动原生 CLI（启动前对账号是否匹配已校验）。
async fn launch(
    ctx: &AppContext,
    acc: &Account,
    program: &'static str,
    args: Vec<String>,
) -> Result<()> {
    launch_program(ctx, acc, program.to_string(), args).await
}

/// 隔离启动核心：acquire → materialize → spawn → absorb → release。`program` 可为原生 CLI 或 shell。
async fn launch_program(
    ctx: &AppContext,
    acc: &Account,
    program: String,
    args: Vec<String>,
) -> Result<()> {
    warn_if_global_active(acc);

    // API 账号：无 refresh token 轮换，直接注入 env vars，跳过 checkout 锁、物化与 absorb。
    if acc.provider == "claude" {
        if let Some(api_vars) = ctx.claude.api_run_env_vars(&acc.id)? {
            println!("run → {}/{} (api-key isolated)", acc.provider, acc.id);
            let status = spawn_isolated(program, args, api_vars).await?;
            propagate_exit(status);
            return Ok(());
        }
    }

    let paths = AppPaths::resolve()?;
    let checkout = Checkout::acquire(&paths.data_dir, &acc.provider, acc.id.0.as_str())?;
    let env_dir = checkout.env_dir().to_path_buf();

    materialize(ctx, acc, &env_dir)?;

    let envs = env_vars(&acc.provider, &env_dir);
    println!(
        "run → {}/{} (isolated {}={})",
        acc.provider,
        acc.id,
        primary_env_name(&acc.provider),
        env_dir.display()
    );

    let status = spawn_isolated(program, args, envs).await?;

    if let Err(e) = absorb(ctx, acc, &env_dir) {
        tracing::warn!(account = %acc.id, err = %e, "absorb rotated credentials failed");
    }
    drop(checkout);
    propagate_exit(status);
    Ok(())
}

/// 把账号凭证物化进隔离 env 目录。
fn materialize(ctx: &AppContext, acc: &Account, env_dir: &Path) -> Result<()> {
    match acc.provider.as_str() {
        "codex" => {
            let blob = ctx
                .codex
                .export_auth_blob(&acc.id)
                .with_context(|| format!("export credentials for codex/{}", acc.id))?;
            write_private_file(&env_dir.join("auth.json"), &blob).with_context(|| {
                format!("materialize isolated CODEX_HOME at {}", env_dir.display())
            })?;
            copy_codex_config_best_effort(env_dir);
            Ok(())
        }
        "claude" => ctx
            .claude
            .materialize_isolated(&acc.id, env_dir)
            .with_context(|| {
                format!(
                    "materialize isolated CLAUDE_CONFIG_DIR at {}",
                    env_dir.display()
                )
            }),
        other => bail!("isolation not supported for provider {other}"),
    }
}

/// 会话结束后把（可能轮换过的）凭证吸收回凭证仓库。best-effort。
fn absorb(ctx: &AppContext, acc: &Account, env_dir: &Path) -> Result<()> {
    match acc.provider.as_str() {
        "codex" => {
            let raw = std::fs::read_to_string(env_dir.join("auth.json"))?;
            ctx.codex.absorb_auth_blob(&acc.id, &raw)
        }
        "claude" => ctx.claude.absorb_isolated(&acc.id, env_dir),
        other => bail!("isolation not supported for provider {other}"),
    }
    .map_err(anyhow::Error::from)
}

/// 该 provider 隔离会话需要导出的环境变量。
fn env_vars(provider: &str, env_dir: &Path) -> Vec<(String, String)> {
    let dir = env_dir.to_string_lossy().into_owned();
    match provider {
        "codex" => vec![("CODEX_HOME".into(), dir)],
        "claude" => {
            let mut v = vec![("CLAUDE_CONFIG_DIR".into(), dir.clone())];
            // macOS：显式设 SECURESTORAGE 目录，使钥匙串 service 名哈希源为我们已知的确切字符串，
            // 与 ClaudeProvider::materialize_isolated 端计算一致。
            if cfg!(target_os = "macos") {
                v.push(("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), dir));
            }
            v
        }
        _ => Vec::new(),
    }
}

fn primary_env_name(provider: &str) -> &'static str {
    match provider {
        "codex" => "CODEX_HOME",
        "claude" => "CLAUDE_CONFIG_DIR",
        _ => "ENV",
    }
}

fn native_cli(provider: &str) -> &'static str {
    match provider {
        "codex" => "codex",
        "claude" => "claude",
        _ => "",
    }
}

fn normalize_provider(provider: &str) -> Result<&'static str> {
    match provider {
        "codex" | "openai" | "chatgpt" => Ok("codex"),
        "claude" | "anthropic" => Ok("claude"),
        other => bail!("unknown provider: {other} (expected codex or claude)"),
    }
}

/// 对「全局 active」账号起隔离会话有 refresh token 轮换冲突风险（见 ACCOUNT_ISOLATION_DESIGN.md §5）。
/// 只告警不阻止：用户可能只在隔离环境里用它。
fn warn_if_global_active(acc: &Account) {
    if acc.active {
        eprintln!(
            "warning: {}/{} is the global active account; running it isolated while also using it \
             non-isolated may invalidate its refresh token. Consider `subswap swap` to another \
             account first.",
            acc.provider, acc.id
        );
    }
}

/// 在隔离环境变量下启动程序，继承终端 stdio，持锁等待退出。
async fn spawn_isolated(
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
) -> Result<std::process::ExitStatus> {
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&program);
        cmd.args(&args);
        for (k, v) in &envs {
            cmd.env(k, v);
        }
        cmd.status()
            .with_context(|| format!("failed to start `{program}`; is it on PATH?"))
    })
    .await
    .context("isolated session task failed")?
}

/// 交互式 CLI 的非零退出多为正常结束；只透传退出码，不当作 subswap 失败。
fn propagate_exit(status: std::process::ExitStatus) {
    if !status.success() {
        if let Some(code) = status.code() {
            std::process::exit(code);
        }
    }
}

/// 写私有凭证文件，Unix 下 `0600`。
fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    harden_file(path);
    Ok(())
}

/// best-effort 复制真实 `~/.codex/config.toml` 进隔离目录，让隔离会话沿用用户常规配置。
fn copy_codex_config_best_effort(env_dir: &Path) {
    let Some(dirs) = directories::UserDirs::new() else {
        return;
    };
    let src = dirs.home_dir().join(".codex").join("config.toml");
    if src.is_file() {
        let _ = std::fs::copy(&src, env_dir.join("config.toml"));
    }
}

/// 给 export 值做最小 shell 引用，避免路径含空格 / 特殊字符时被拆。
fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

#[cfg(unix)]
fn harden_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn harden_file(_path: &Path) {}
