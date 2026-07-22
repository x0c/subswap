//! subswap CLI entry.
//!
//! Surface (intentionally small):
//! - `subswap`          — default: sync local active accounts, fetch quotas, auto-swap if
//!   active is past threshold, render a one-screen status.
//! - `subswap login <provider>` — run the native provider login, then import/overwrite it.
//! - `subswap add-api` — interactively add a Claude Code compatible API endpoint.
//! - `subswap swap [<id|N>]` — escape hatch: force-swap. Never depends on quota.
//!   With no argument, prints the numbered listing instead of swapping.
//! - `subswap rm <id|N>`     — remove an account (registry + keyring).
//! - `subswap doctor`        — environment self-check.
//!
//! `<id>` is the account id (email for claude / account_key for codex), label, or
//! `<provider>/<id>` to disambiguate. `<N>` is the global index shown by `subswap` (1-based).

mod app;
mod cmd;
mod daemon_spawn;
mod render;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app::AppContext;

#[derive(Parser)]
#[command(
    name = "subswap",
    version,
    about = "Manage and auto-swap between multiple AI subscription accounts.",
    long_about = "Run `subswap` with no arguments to sync local accounts, check quotas, \
                  and auto-swap if the active account is past threshold. \
                  Use `add-api`/`login`/`swap`/`rm`/`doctor` for explicit actions."
)]
struct Cli {
    /// Log level (equivalent to RUST_LOG).
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    /// On the default entry (no subcommand), emit accounts + quota snapshot as JSON
    /// (including each window's reset_at) for programmatic consumers.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
// clap 子命令只在进程启动时构造一次；为 add-api 的向导参数逐字段装箱只会增加样板。
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Add a Claude Code compatible API endpoint without activating it.
    AddApi {
        /// Preset: deepseek, kimi or custom.
        #[arg(long)]
        preset: Option<String>,

        /// Stable account id used by `subswap swap`.
        #[arg(long)]
        id: Option<String>,

        /// Display name.
        #[arg(long)]
        name: Option<String>,

        /// Anthropic-compatible API base URL.
        #[arg(long)]
        endpoint: Option<String>,

        /// API key. Prefer the interactive prompt to avoid shell history.
        #[arg(long)]
        api_key: Option<String>,

        /// Authentication mode: bearer or api-key.
        #[arg(long)]
        auth: Option<String>,

        /// Opus role model.
        #[arg(long)]
        opus_model: Option<String>,

        /// Sonnet role model.
        #[arg(long)]
        sonnet_model: Option<String>,

        /// Haiku role model.
        #[arg(long)]
        haiku_model: Option<String>,

        /// Deprecated compatibility alias for --sonnet-model.
        #[arg(long, hide = true)]
        model: Option<String>,

        /// Deprecated compatibility alias for --haiku-model.
        #[arg(long, hide = true)]
        subagent_model: Option<String>,

        /// Claude Code effort level.
        #[arg(long)]
        effort: Option<String>,

        /// Billing mode: flat (fixed-rate subscription), metered (pay-per-use), or unlimited.
        /// Determines auto-switch priority in downstream consumers like OpenConductor.
        /// Defaults to metered when not specified.
        #[arg(long)]
        billing: Option<String>,

        /// Skip the final confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Log in through the native provider CLI, then import and activate that account.
    Login {
        /// Provider to log in: claude, codex, kimi, or cursor.
        provider: String,

        /// Pre-populate Claude login email.
        #[arg(long)]
        email: Option<String>,

        /// Force Claude SSO login flow.
        #[arg(long)]
        sso: bool,

        /// Use Codex device auth flow.
        #[arg(long)]
        device_auth: bool,

        /// Extra arguments passed to the native login command after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Swap to <id|N>. With no argument, prints numbered accounts and exits.
    Swap {
        /// Account index (e.g. `3`), id, label, or `<provider>/<id>`.
        id: Option<String>,
    },

    /// Launch a provider CLI in an account-isolated environment without changing the global active account.
    Run {
        /// Provider to launch: codex or claude.
        provider: String,

        /// Account index (e.g. `3`), id, label, or `<provider>/<id>`.
        id: String,

        /// Arguments passed to the native CLI after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Open a subshell with an account's isolated environment exported. Provider is inferred from the account.
    Shell {
        /// Account index (e.g. `3`), id, label, or `<provider>/<id>`.
        id: String,
    },

    /// Print `export` lines for an account's isolated environment (for `eval`). No lock / no absorb.
    Env {
        /// Account index (e.g. `3`), id, label, or `<provider>/<id>`.
        id: String,
    },

    /// Remove <id|N> from registry and keyring. Use `<provider>/<id>` if ambiguous.
    Rm { id: String },

    /// Show or change autoswap state. No argument prints current state; 'on'/'off' to change.
    Autoswap {
        /// 'on' to enable, 'off' to disable.
        toggle: Option<String>,
    },

    /// Environment self-check.
    Doctor,

    /// Import local data from legacy account stores.
    #[command(hide = true)]
    MigrateLocal,

    /// Internal daemon entry used on macOS to keep Keychain access tied to the `subswap` binary.
    #[command(name = "__daemon", hide = true)]
    InternalDaemon,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if matches!(cli.cmd, Some(Cmd::InternalDaemon)) {
        return subswap_daemon::run().await;
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // 启动时加载 config.toml（缺失 / 解析失败时沿用默认值 + warn）。
    if let Err(e) = subswap_core::settings::reload_from_file() {
        tracing::warn!(err = %e, "load config failed; using built-in defaults");
    }

    let ctx = AppContext::build()?;

    match cli.cmd {
        None => cmd::default::run(&ctx, cli.json).await,
        Some(Cmd::AddApi {
            preset,
            id,
            name,
            endpoint,
            api_key,
            auth,
            opus_model,
            sonnet_model,
            haiku_model,
            model,
            subagent_model,
            effort,
            billing,
            yes,
        }) => cmd::add_api::run(
            &ctx,
            cmd::add_api::AddApiOptions {
                preset,
                id,
                name,
                endpoint,
                api_key,
                auth,
                opus_model,
                sonnet_model,
                haiku_model,
                model,
                subagent_model,
                effort,
                billing,
                yes,
            },
        ),
        Some(Cmd::Login {
            provider,
            email,
            sso,
            device_auth,
            args,
        }) => cmd::login::run(&ctx, &provider, email, sso, device_auth, args).await,
        Some(Cmd::Swap { id }) => cmd::swap::run(&ctx, id.as_deref()).await,
        Some(Cmd::Run { provider, id, args }) => cmd::run::run(&ctx, &provider, &id, args).await,
        Some(Cmd::Shell { id }) => cmd::run::shell(&ctx, &id).await,
        Some(Cmd::Env { id }) => cmd::run::env(&ctx, &id).await,
        Some(Cmd::Rm { id }) => cmd::rm::run(&ctx, &id).await,
        Some(Cmd::Autoswap { toggle }) => cmd::autoswap::run(toggle.as_deref()),
        Some(Cmd::Doctor) => cmd::doctor::run(&ctx).await,
        Some(Cmd::MigrateLocal) => cmd::migrate::run(&ctx).await,
        Some(Cmd::InternalDaemon) => unreachable!("handled before CLI context initialization"),
    }
}
