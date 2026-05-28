//! subswap CLI entry.
//!
//! Surface (intentionally small):
//! - `subswap`          — default: sync local active accounts, fetch quotas, auto-swap if
//!   active is past threshold, render a one-screen status.
//! - `subswap login <provider>` — run the native provider login, then import/overwrite it.
//! - `subswap swap <id>` — escape hatch: force-swap to a specific account. Never depends on quota.
//! - `subswap rm <id>`   — remove an account (registry + keyring).
//! - `subswap doctor`    — environment self-check.
//!
//! `<id>` is the account id (email for claude / account_key for codex). When the same id
//! exists under multiple providers, disambiguate with `<provider>/<id>`.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use futures::future::join_all;
use subswap_core::{
    auto_decide, AccountId, AccountRegistry, AccountWithQuotas, AuditEvent, AuditLog,
    CredentialStore, KeyringStore, PolicyConfig, PolicyDecision, ProviderRegistry,
    ProviderSnapshot, Quota, QuotaStatus, QuotaWindow,
};
use subswap_provider_claude::ClaudeProvider;
use subswap_provider_codex::CodexProvider;

const QUOTA_LOADING: &str = "__subswap_quota_loading__";

#[derive(Parser)]
#[command(
    name = "subswap",
    version,
    about = "Manage and auto-swap between multiple AI subscription accounts.",
    long_about = "Run `subswap` with no arguments to sync local accounts, check quotas, \
                  and auto-swap if the active account is past threshold. \
                  Use `login`/`swap`/`rm`/`doctor` for explicit actions."
)]
struct Cli {
    /// Log level (equivalent to RUST_LOG).
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in through the native provider CLI, then import and activate that account.
    Login {
        /// Provider to log in: claude or codex.
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

    /// Swap to <id>. Use `<provider>/<id>` if the id is ambiguous across providers.
    Swap { id: String },

    /// Remove <id> from registry and keyring. Use `<provider>/<id>` if ambiguous.
    Rm { id: String },

    /// Environment self-check.
    Doctor,

    /// Import local data from claude-swap and codex-auth.
    #[command(hide = true)]
    MigrateLocal,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let ctx = AppContext::build()?;

    match cli.cmd {
        None => cmd_default(&ctx).await,
        Some(Cmd::Login {
            provider,
            email,
            sso,
            device_auth,
            args,
        }) => cmd_login(&ctx, &provider, email, sso, device_auth, args).await,
        Some(Cmd::Swap { id }) => cmd_swap(&ctx, &id).await,
        Some(Cmd::Rm { id }) => cmd_rm(&ctx, &id).await,
        Some(Cmd::Doctor) => cmd_doctor(&ctx).await,
        Some(Cmd::MigrateLocal) => cmd_migrate_local(&ctx).await,
    }
}

struct AppContext {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    claude: Arc<ClaudeProvider>,
    codex: Arc<CodexProvider>,
    providers: ProviderRegistry,
    audit: AuditLog,
}

impl AppContext {
    fn build() -> Result<Self> {
        let store: Arc<dyn CredentialStore> = Arc::new(KeyringStore::new());
        let registry = Arc::new(AccountRegistry::from_default_paths()?);

        let claude = Arc::new(ClaudeProvider::new(store.clone(), registry.clone()));
        let codex = Arc::new(CodexProvider::new(store.clone(), registry.clone()));

        let mut providers = ProviderRegistry::new();
        providers.register(claude.clone());
        providers.register(codex.clone());

        let audit = AuditLog::from_default_paths()?;

        Ok(Self {
            store,
            registry,
            claude,
            codex,
            providers,
            audit,
        })
    }
}

// ============================================================
// default (subswap with no args)
// ============================================================
async fn cmd_default(ctx: &AppContext) -> Result<()> {
    // 1. 自动 import 本地激活账号（如果没记录过）。
    sync_local_active(ctx);

    // 2. 先输出账号骨架，再随 quota 请求完成原地刷新。
    let interactive = io::stdout().is_terminal();
    let mut snapshots = build_loading_snapshots(&ctx.providers).await;
    let mut renderer = InlineRenderer::new(interactive);
    if interactive {
        renderer.render(&snapshots, &[])?;
    }
    fill_quotas_progressively(
        &ctx.providers,
        &mut snapshots,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
    )
    .await?;

    // 3. 应用 AutoSwapPolicy；只在完整 quota 返回后决策，避免半截数据乱切。
    let cfg = PolicyConfig::default();
    let mut auto_lines: Vec<AutoLine> = Vec::new();
    let mut activated: Vec<(String, AccountId)> = Vec::new();
    for snap in &snapshots {
        if snap.accounts.is_empty() {
            continue;
        }
        match auto_decide(snap, &cfg) {
            PolicyDecision::Swap { to, .. } => {
                let p = ctx.providers.get(&snap.provider)?;
                match p.activate(&to).await {
                    Ok(()) => {
                        auto_lines.push(AutoLine {
                            provider: snap.provider.clone(),
                            text: format!("auto: swapped to {}", account_ref(&to.0)),
                        });
                        ctx.audit.append(AuditEvent::ok(
                            "auto_swap",
                            &snap.provider,
                            Some(to.0.as_str()),
                        ));
                        activated.push((snap.provider.clone(), to));
                    }
                    Err(e) => {
                        auto_lines.push(AutoLine {
                            provider: snap.provider.clone(),
                            text: format!("auto: failed ({})", compact_error(&e.to_string())),
                        });
                        ctx.audit.append(AuditEvent::err(
                            "auto_swap",
                            &snap.provider,
                            Some(to.0.as_str()),
                            &e.to_string(),
                        ));
                    }
                }
            }
            PolicyDecision::Degraded { reason } => {
                tracing::debug!(
                    provider=%snap.provider,
                    reason=%compact_policy_reason(&reason),
                    "auto swap degraded"
                );
            }
            PolicyDecision::NoOp { .. } => {} // 沉默是金
        }
    }
    for (provider, id) in activated {
        mark_active(&mut snapshots, &provider, &id);
    }

    // 4. 最终渲染。交互场景刷新原输出块；非交互场景只输出最终版。
    renderer.render(&snapshots, &auto_lines)?;

    // 5. 后台保活:用户无感地拉起 subswapd(已经在跑则什么都不做)。
    //    失败仅 debug 日志,不影响默认命令的退出码。
    if let Err(e) = ensure_daemon_running() {
        tracing::debug!(err = %e, "ensure_daemon_running failed; continuing");
    }
    Ok(())
}

/// 扫本地 ~/.claude / ~/.codex；如果有当前激活账号则 import 到 registry（已存在时 upsert）。
/// 任一 provider 失败（用户没登录过）静默跳过。
fn sync_local_active(ctx: &AppContext) {
    if let Err(e) = ctx.claude.import_active(None) {
        tracing::debug!(err=%e, "skip claude auto-import");
    }
    if let Err(e) = ctx.codex.import_active(None) {
        tracing::debug!(err=%e, "skip codex auto-import");
    }
}

async fn build_loading_snapshots(registry: &ProviderRegistry) -> Vec<ProviderSnapshot> {
    let provider_tasks = registry.all().into_iter().map(|p| async move {
        let provider = p.id().to_string();
        let accounts = p.list_accounts().await.unwrap_or_default();
        ProviderSnapshot {
            provider,
            accounts: accounts
                .into_iter()
                .map(|account| AccountWithQuotas {
                    account,
                    quotas: Vec::new(),
                    fetch_error: Some(QUOTA_LOADING.into()),
                })
                .collect(),
        }
    });
    join_all(provider_tasks).await
}

struct QuotaUpdate {
    provider: String,
    account_id: AccountId,
    result: std::result::Result<Vec<Quota>, String>,
}

async fn fill_quotas_progressively(
    registry: &ProviderRegistry,
    snapshots: &mut [ProviderSnapshot],
    mut renderer: Option<&mut InlineRenderer>,
) -> Result<()> {
    let total: usize = snapshots.iter().map(|snap| snap.accounts.len()).sum();
    if total == 0 {
        return Ok(());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(total);
    for snap in snapshots.iter() {
        let provider = snap.provider.clone();
        let p = registry.get(&provider)?;
        for awq in &snap.accounts {
            let tx = tx.clone();
            let p = p.clone();
            let provider = provider.clone();
            let account_id = awq.account.id.clone();
            tokio::spawn(async move {
                let result = p.query_quota(&account_id).await.map_err(|e| e.to_string());
                let _ = tx
                    .send(QuotaUpdate {
                        provider,
                        account_id,
                        result,
                    })
                    .await;
            });
        }
    }
    drop(tx);

    while let Some(update) = rx.recv().await {
        apply_quota_update(snapshots, update);
        if let Some(renderer) = renderer.as_deref_mut() {
            renderer.render(snapshots, &[])?;
        }
    }
    Ok(())
}

fn apply_quota_update(snapshots: &mut [ProviderSnapshot], update: QuotaUpdate) {
    let Some(snap) = snapshots
        .iter_mut()
        .find(|snap| snap.provider == update.provider)
    else {
        return;
    };
    let Some(awq) = snap
        .accounts
        .iter_mut()
        .find(|awq| awq.account.id == update.account_id)
    else {
        return;
    };
    match update.result {
        Ok(quotas) => {
            awq.quotas = quotas;
            awq.fetch_error = None;
        }
        Err(err) => {
            awq.quotas.clear();
            awq.fetch_error = Some(err);
        }
    }
}

fn mark_active(snapshots: &mut [ProviderSnapshot], provider: &str, id: &AccountId) {
    for snap in snapshots {
        if snap.provider != provider {
            continue;
        }
        for awq in &mut snap.accounts {
            awq.account.active = awq.account.id == *id;
        }
    }
}

struct InlineRenderer {
    enabled: bool,
    rendered_lines: usize,
}

impl InlineRenderer {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            rendered_lines: 0,
        }
    }

    fn render(&mut self, snapshots: &[ProviderSnapshot], auto_lines: &[AutoLine]) -> Result<()> {
        let output = render_to_string(snapshots, auto_lines);
        if self.enabled && self.rendered_lines > 0 {
            print!("\x1b[{}A\x1b[J", self.rendered_lines);
        }
        print!("{output}");
        io::stdout().flush()?;
        self.rendered_lines = output.lines().count();
        Ok(())
    }
}

struct AutoLine {
    provider: String,
    text: String,
}

fn render_to_string(snapshots: &[ProviderSnapshot], auto_lines: &[AutoLine]) -> String {
    let mut out = String::new();
    let has_any = snapshots.iter().any(|s| !s.accounts.is_empty());
    if !has_any {
        out.push_str("No accounts. Log in to Claude Code or Codex CLI, then re-run `subswap`.\n");
        return out;
    }

    for snap in snapshots {
        if snap.accounts.is_empty() {
            continue;
        }
        out.push_str(&format!("{}\n", snap.provider));

        for line in auto_lines
            .iter()
            .filter(|line| line.provider == snap.provider)
        {
            out.push_str(&format!("  ! {}\n", line.text));
        }

        let name_width = snap
            .accounts
            .iter()
            .map(|a| account_name(a).chars().count())
            .max()
            .unwrap_or(0)
            .clamp(16, 36);
        for awq in &snap.accounts {
            let star = if awq.account.active { "*" } else { " " };
            let name = account_name(awq);
            if let Some(err) = &awq.fetch_error {
                if err == QUOTA_LOADING {
                    out.push_str(&format!(
                        "  {star} {:<name_width$}  quota loading",
                        truncate_to_width(&name, name_width)
                    ));
                    out.push('\n');
                    continue;
                }
                out.push_str(&format!(
                    "  {star} {:<name_width$}  quota {}",
                    truncate_to_width(&name, name_width),
                    compact_error(err)
                ));
                out.push('\n');
                continue;
            }
            if awq.quotas.is_empty() {
                out.push_str(&format!(
                    "  {star} {:<name_width$}",
                    truncate_to_width(&name, name_width)
                ));
                out.push('\n');
                continue;
            }
            let parts: Vec<String> = awq
                .quotas
                .iter()
                .filter(|q| quota_has_display_value(q))
                .map(format_quota_compact)
                .collect();
            if parts.is_empty() {
                out.push_str(&format!(
                    "  {star} {:<name_width$}  quota unknown",
                    truncate_to_width(&name, name_width)
                ));
                out.push('\n');
                continue;
            }
            out.push_str(&format!(
                "  {star} {:<name_width$}  {}",
                truncate_to_width(&name, name_width),
                parts.join("  ")
            ));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn account_name(awq: &AccountWithQuotas) -> String {
    if awq.account.label.trim().is_empty() {
        account_ref(&awq.account.id.0)
    } else {
        awq.account.label.clone()
    }
}

fn account_ref(value: &str) -> String {
    value
        .rsplit_once("::")
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_else(|| value.to_string())
}

fn truncate_to_width(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    let keep = width - 1;
    format!("{}…", value.chars().take(keep).collect::<String>())
}

fn compact_policy_reason(reason: &str) -> String {
    if reason.contains("no swap candidate") {
        "no candidate".into()
    } else if reason.contains("quota fetch failed") {
        "quota unavailable".into()
    } else {
        compact_error(reason)
    }
}

fn compact_error(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("invalid authentication credentials")
    {
        return "401 auth failed".into();
    }
    if lower.contains("429") || lower.contains("rate limit") {
        return "429 rate limited".into();
    }
    if lower.contains("timeout") {
        return "timeout".into();
    }
    if lower.contains("network") || lower.contains("request ") {
        return "network error".into();
    }
    if lower.contains("parse") || lower.contains("not json") {
        return "bad response".into();
    }
    if lower.contains("missing") {
        return "missing metadata".into();
    }
    err.split(':').next().unwrap_or("error").trim().to_string()
}

fn format_quota_compact(q: &Quota) -> String {
    let w = match q.window {
        QuotaWindow::FiveHour => "5h",
        QuotaWindow::SevenDay => "7d",
        QuotaWindow::Month => "mo",
        QuotaWindow::Custom => "--",
    };
    let s = match q.status {
        QuotaStatus::Ok => "ok",
        QuotaStatus::Warn => "warn",
        QuotaStatus::Exhausted => "full",
        QuotaStatus::Unknown => "--",
    };
    let usage = if q.limit > 0 {
        format!("{:>3}%", q.used)
    } else {
        "--".into()
    };
    let reset = q
        .reset_at
        .map(format_reset_at)
        .unwrap_or_else(|| "--".into());
    format!("{w:<2} [{usage:>4} {s:<4} reset {reset:<6}]")
}

fn quota_has_display_value(q: &Quota) -> bool {
    q.limit > 0 || q.reset_at.is_some() || !matches!(q.status, QuotaStatus::Unknown)
}

fn format_reset_at(reset_at: DateTime<Utc>) -> String {
    let delta = reset_at.signed_duration_since(Utc::now());
    let seconds = delta.num_seconds();
    if seconds <= 0 {
        "now".into()
    } else if seconds < 90 * 60 {
        format!("in {}m", (seconds + 59) / 60)
    } else if seconds < 48 * 60 * 60 {
        format!("in {}h", (seconds + 3599) / 3600)
    } else {
        format!("in {}d", (seconds + 86_399) / 86_400)
    }
}

// ============================================================
// login
// ============================================================
async fn cmd_login(
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

// ============================================================
// daemon auto-spawn
// ============================================================

/// 用户无感地拉起 subswapd:已经在跑则什么都不做;否则 fork+setsid 一个 detached 子进程。
///
/// 设计要点:
/// - 通过 PID 文件上的 fs2 排他锁判断「是否已经有实例在跑」(不依赖 kill -0 / PID 复用问题)。
/// - 拉起方式:fork(由 std::process::Command 完成) + 在 pre_exec 里 setsid + stdio 重定向到日志。
/// - 不等待子进程,父进程退出后子进程被 init 收养,作为正常后台进程持续跑。
/// - 找 subswapd 二进制:优先 current_exe 同目录,其次 PATH。
/// - 非 Unix 平台:暂不自动拉起(M4 只承诺 Linux / macOS)。
pub fn ensure_daemon_running() -> Result<()> {
    // 测试 / 用户禁用逃生口:SUBSWAP_NO_DAEMON=1 时不拉。
    if std::env::var_os("SUBSWAP_NO_DAEMON").is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use subswap_core::paths::AppPaths;

        let paths = AppPaths::resolve()?;
        let pid_path = paths.daemon_pid_file();
        if daemon_alive(&pid_path)? {
            return Ok(());
        }
        let binary = locate_subswapd().context(
            "subswapd binary not found next to subswap or on PATH; daemon auto-start skipped",
        )?;
        let log_path = paths.daemon_log_file();
        spawn_detached_daemon(&binary, &log_path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tracing::debug!("daemon auto-start not supported on this platform; run subswapd manually");
        Ok(())
    }
}

#[cfg(unix)]
fn daemon_alive(pid_path: &Path) -> Result<bool> {
    use fs2::FileExt;
    if !pid_path.exists() {
        return Ok(false);
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pid_path)
        .with_context(|| format!("open pid file {}", pid_path.display()))?;
    // 锁能拿到 → 没人在跑;拿不到 → 已有 daemon。
    match f.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&f);
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

#[cfg(unix)]
fn locate_subswapd() -> Option<PathBuf> {
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            let candidate = dir.join("subswapd");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("subswapd");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn spawn_detached_daemon(binary: &Path, log_path: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log_out.try_clone().context("dup daemon log fd")?;

    // SAFETY: pre_exec 里只调用 async-signal-safe 的 setsid;不分配,不取锁。
    let mut cmd = Command::new(binary);
    cmd.stdin(Stdio::null()).stdout(log_out).stderr(log_err);
    unsafe {
        cmd.pre_exec(|| {
            // 脱离当前 session/process group,这样:
            // 1. 父进程退出不会带着 daemon 一起死(SIGHUP 不会发到 daemon);
            // 2. daemon 不再持有控制终端,不抢 stdin。
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // spawn 后不 wait;Child 句柄 drop 默认就是 detach(不发 SIGKILL)。
    let _child = cmd
        .spawn()
        .with_context(|| format!("spawn detached daemon {}", binary.display()))?;
    Ok(())
}

// ============================================================
// swap
// ============================================================
async fn cmd_swap(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = ctx
        .registry
        .find_unique(id_input)?
        .with_context(|| format!("account not found: {id_input}"))?;
    let p = ctx.providers.get(&acc.provider)?;
    let res = p.activate(&acc.id).await;
    match res {
        Ok(()) => {
            ctx.audit.append(AuditEvent::ok(
                "activate",
                &acc.provider,
                Some(acc.id.0.as_str()),
            ));
            println!("swap → {}/{}", acc.provider, acc.id);
            Ok(())
        }
        Err(e) => {
            ctx.audit.append(AuditEvent::err(
                "activate",
                &acc.provider,
                Some(acc.id.0.as_str()),
                &e.to_string(),
            ));
            Err(anyhow::Error::from(e).context(format!("swap {}/{} failed", acc.provider, acc.id)))
        }
    }
}

// ============================================================
// rm
// ============================================================
async fn cmd_rm(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = ctx
        .registry
        .find_unique(id_input)?
        .with_context(|| format!("account not found: {id_input}"))?;

    ctx.registry.remove(&acc.provider, &acc.id)?;

    let fields: &[&str] = match acc.provider.as_str() {
        "claude" => &["credentials_json"],
        "codex" => &["auth_json"],
        _ => &[],
    };
    for f in fields {
        if let Err(e) = ctx.store.delete(&acc.provider, acc.id.0.as_str(), f) {
            tracing::warn!(err=%e, field=%f, "keyring delete failed (continuing)");
        }
    }

    ctx.audit
        .append(AuditEvent::ok("rm", &acc.provider, Some(acc.id.0.as_str())));
    println!("removed {}/{}", acc.provider, acc.id);
    Ok(())
}

// ============================================================
// doctor
// ============================================================
async fn cmd_doctor(ctx: &AppContext) -> Result<()> {
    println!("subswap doctor");
    println!("------------------------------------------------------------");
    match subswap_core::paths::AppPaths::resolve() {
        Ok(p) => {
            println!("[ok ] config dir   {}", p.config_dir.display());
            println!("[ok ] data dir     {}", p.data_dir.display());
            println!("[ok ] state dir    {}", p.state_dir.display());
            println!("[ok ] cache dir    {}", p.cache_dir.display());
            println!("[ok ] registry     {}", p.registry_file().display());
            println!("[ok ] audit log    {}", p.audit_log().display());
        }
        Err(e) => println!("[err] resolve paths: {e}"),
    }

    let store = KeyringStore::new();
    let probe_field = "doctor_probe";
    match store.set("subswap", "_doctor", probe_field, "ok") {
        Ok(()) => {
            let _ = store.delete("subswap", "_doctor", probe_field);
            println!("[ok ] system keyring");
        }
        Err(e) => println!("[err] system keyring: {e}"),
    }

    for p in ctx.providers.all() {
        println!();
        println!("[{}] {}", p.id(), p.display_name());
        for t in p.client_targets() {
            let tag = if t.probe_path.exists() { "ok " } else { "mis" };
            println!(
                "  [{tag}] {:<24} {}",
                t.display_name,
                t.probe_path.display()
            );
        }
    }
    Ok(())
}

// ============================================================
// hidden local migration
// ============================================================
async fn cmd_migrate_local(ctx: &AppContext) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot locate legacy account stores")?;

    let claude = migrate_claude_swap(ctx, &home)?;
    let codex = migrate_codex_auth(ctx, &home)?;
    println!("migrated claude={claude} codex={codex}");
    Ok(())
}

fn migrate_claude_swap(ctx: &AppContext, home: &Path) -> Result<usize> {
    let root = home.join(".local/share/claude-swap");
    let config_dir = root.join("configs");
    let cred_dir = root.join("credentials");
    if !config_dir.exists() || !cred_dir.exists() {
        return Ok(0);
    }

    let mut imported = 0;
    for entry in std::fs::read_dir(&config_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw_config = std::fs::read_to_string(&path)?;
        let config: serde_json::Value = serde_json::from_str(&raw_config)?;
        let Some(oauth_account) = config.get("oauthAccount").cloned() else {
            continue;
        };
        let Some(email) = oauth_account
            .get("emailAddress")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };

        let cred_path = cred_dir.join(format!(
            ".creds-{}.enc",
            account_number_and_email(&path, &email)
        ));
        let cred_path = if cred_path.exists() {
            cred_path
        } else {
            let fallback = cred_dir.join(format!(".creds-{email}.enc"));
            if !fallback.exists() {
                tracing::warn!(email=%email, "claude-swap credentials file missing; skipping account");
                continue;
            }
            fallback
        };

        let encoded = std::fs::read_to_string(&cred_path)?;
        let decoded = STANDARD
            .decode(encoded.trim())
            .context("decode claude-swap credentials")?;
        let credentials_json =
            String::from_utf8(decoded).context("claude-swap credentials utf8")?;
        let oauth_account_json = serde_json::to_string(&oauth_account)?;
        ctx.claude.import_from_raw_json(
            &credentials_json,
            &oauth_account_json,
            Some(email.clone()),
        )?;
        imported += 1;
    }

    if let Some(active_email) = claude_swap_active_email(&root)? {
        let id = subswap_core::AccountId(active_email);
        if let Err(e) = ctx.registry.set_active("claude", &id) {
            tracing::warn!(err=%e, "failed to preserve claude-swap active account");
        }
    }

    Ok(imported)
}

fn claude_swap_active_email(root: &Path) -> Result<Option<String>> {
    let sequence_path = root.join("sequence.json");
    if !sequence_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(sequence_path)?;
    let sequence: serde_json::Value = serde_json::from_str(&raw)?;
    let Some(active_number) = sequence.get("activeAccountNumber").and_then(|v| v.as_i64()) else {
        return Ok(None);
    };
    Ok(sequence
        .get("accounts")
        .and_then(|v| v.get(active_number.to_string()))
        .and_then(|v| v.get("email"))
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

fn account_number_and_email(path: &Path, email: &str) -> String {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return email.to_string();
    };
    let marker = ".claude-config-";
    if let Some(rest) = name.strip_prefix(marker) {
        if let Some(number) = rest.split('-').next() {
            return format!("{number}-{email}");
        }
    }
    email.to_string()
}

fn migrate_codex_auth(ctx: &AppContext, home: &Path) -> Result<usize> {
    let accounts_dir = home.join(".codex/accounts");
    let registry_path = accounts_dir.join("registry.json");
    if !registry_path.exists() {
        return Ok(0);
    }

    let registry_raw = std::fs::read_to_string(&registry_path)?;
    let registry: serde_json::Value = serde_json::from_str(&registry_raw)?;
    let active_key = registry
        .get("active_account_key")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let Some(accounts) = registry.get("accounts").and_then(|v| v.as_array()) else {
        return Ok(0);
    };

    let mut imported = 0;
    for account in accounts {
        let Some(account_key) = account.get("account_key").and_then(|v| v.as_str()) else {
            continue;
        };
        let auth_name = URL_SAFE_NO_PAD.encode(account_key.as_bytes());
        let auth_path = accounts_dir.join(format!("{auth_name}.auth.json"));
        if !auth_path.exists() {
            tracing::warn!(account=%account_key, "codex-auth blob missing; skipping account");
            continue;
        }
        let raw_auth_json = std::fs::read_to_string(&auth_path)?;
        let active = active_key.as_deref() == Some(account_key);
        ctx.codex
            .import_raw_with_metadata(raw_auth_json, account.clone(), active)?;
        imported += 1;
    }

    if let Some(active_key) = active_key {
        let id = subswap_core::AccountId(active_key);
        if let Err(e) = ctx.registry.set_active("codex", &id) {
            tracing::warn!(err=%e, "failed to preserve codex-auth active account");
        }
    }

    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(window: QuotaWindow, used: u64, limit: u64, status: QuotaStatus) -> Quota {
        Quota {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            window,
            used,
            limit,
            reset_at: Some(Utc::now() + chrono::Duration::hours(2)),
            status,
            note: None,
        }
    }

    #[test]
    fn quota_format_is_block_like() {
        let text = format_quota_compact(&quota(QuotaWindow::FiveHour, 6, 100, QuotaStatus::Ok));
        assert!(text.starts_with("5h [  6% ok"));
        assert!(text.contains("reset in 2h"));
    }

    #[test]
    fn unknown_quota_without_data_is_hidden() {
        let q = Quota {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            window: QuotaWindow::Month,
            used: 0,
            limit: 0,
            reset_at: None,
            status: QuotaStatus::Unknown,
            note: None,
        };
        assert!(!quota_has_display_value(&q));
    }
}
