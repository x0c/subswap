//! `subswap status [--json]`：只读导出本机账号注册表与额度缓存。
//!
//! 该命令面向 macOS/iOS GUI、小组件与其它程序消费者：
//! - **绝不** sync 本地 active、查询额度、触发自动切换、拉起 daemon、刷新 token 或写任何
//!   状态文件；因此可以安全地反复调用（窗口重绘、widget timeline 刷新）。
//! - 不构造 Provider 实例：Provider 构造可能读取原生客户端文件或钥匙串（例如 Cursor），
//!   只读入口必须完全避开这些副作用。Provider 展示名来自本模块的静态元数据。
//! - `--json` 输出稳定 schema，保持与 Apple 端 `SubswapSnapshot` 解码器兼容；新增字段
//!   只能是追加式（Swift `Codable` 会忽略未知 key）。
//! - 账号的 `cached_at` 是**原始观测时间**，不是本次快照的 `generated_at`。快照新生成
//!   不代表额度数据新鲜；过期数据通过 `fetch_state = "stale"` 明确标记。
//! - 不输出疑似密钥的 label；API Key 类账号改用端点 host 或通用安全标签。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use subswap_core::{
    paths::AppPaths, request_metrics, settings, Account, AccountRegistry, Quota, QuotaCache,
};

/// 只读快照 schema 版本。
///
/// 1 为 Apple 端当前解码版本；新增字段保持追加式，不改变既有字段含义。
const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct StatusSnapshot {
    schema_version: u32,
    /// 本次快照生成时间。**不是**任何 quota 观测时间；观测时间见账号 `cached_at`。
    generated_at: DateTime<Utc>,
    daemon_running: bool,
    /// 超过该年龄的额度只用于展示，不再作为自动决策依据。
    stale_after_ms: i64,
    auto_swap: AutoSwapStatus,
    warn_pct: f64,
    exhausted_pct: f64,
    provider_order: Vec<String>,
    accounts: Vec<StatusAccount>,
    /// 本机直接请求/采集计数器的只读快照；缺失时仍是稳定结构（`available=false`）。
    /// 计数不包含官方客户端自身发出的、subswap 不可见的 HTTP 流量。
    /// `--no-metrics` 时整个字段省略，避免高频消费者搬运随分钟桶增长的诊断数据。
    #[serde(skip_serializing_if = "Option::is_none")]
    request_metrics: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct AutoSwapStatus {
    enabled: bool,
    threshold: f64,
}

#[derive(Serialize)]
struct StatusAccount {
    id: String,
    provider: String,
    provider_display_name: String,
    display_label: String,
    active: bool,
    billing: String,
    manual_only: bool,
    /// `ready` / `stale` / `failed` / `missing`。
    ///
    /// `failed` 与 `missing` 都表示当前没有可信额度；区别是前者有最近一次查询错误。
    fetch_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_at: Option<DateTime<Utc>>,
    quotas: Vec<Quota>,
}

/// 额度缓存文件中只读需要的原始观测时间。
///
/// `QuotaCache::get` 会过滤 reset 已过期的窗口；过滤后仍要保留原始观测时间，避免把
/// 「查过但窗口已 reset」伪装成「从未查过」。key 与 `QuotaCache` 的磁盘格式一致。
#[derive(Debug, Default, Deserialize)]
struct RawQuotaCache {
    #[serde(default)]
    entries: BTreeMap<String, RawCachedEntry>,
}

#[derive(Debug, Deserialize)]
struct RawCachedEntry {
    cached_at: DateTime<Utc>,
}

pub async fn run(json: bool, include_metrics: bool) -> Result<()> {
    // 只做本地文件读取与序列化；放进 blocking 线程避免文件 IO 阻塞 async runtime。
    let snapshot = tokio::task::spawn_blocking(move || build_snapshot(include_metrics)).await??;

    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print_human(&snapshot);
    }
    Ok(())
}

fn build_snapshot(include_metrics: bool) -> Result<StatusSnapshot> {
    // 只读入口：不触发目录创建。全新 home 也必须返回空快照。
    let paths = AppPaths::resolve_readonly()?;
    let registry = AccountRegistry::new(paths.registry_file());
    let cache_path = paths.quota_cache_file();
    let cache = QuotaCache::load(&cache_path);
    let raw = raw_cache(&cache_path);
    let current = match settings::load_from(&paths.config_file()) {
        Ok(current) => current,
        Err(error) => {
            tracing::warn!(err = %error, "load config failed; using built-in defaults");
            settings::current()
        }
    };
    let now = Utc::now();
    let stale_after_ms = current.quota.decision_max_age_ms.max(0);

    let mut grouped: BTreeMap<String, Vec<Account>> = BTreeMap::new();
    for account in registry.load()? {
        grouped
            .entry(account.provider.clone())
            .or_default()
            .push(account);
    }

    let mut accounts = Vec::new();
    for (provider, provider_accounts) in &grouped {
        let display_name = provider_display_name(provider);
        for account in provider_accounts {
            accounts.push(status_account(
                &cache,
                &raw,
                account.clone(),
                &display_name,
                now,
                stale_after_ms,
            ));
        }
    }

    Ok(StatusSnapshot {
        schema_version: SCHEMA_VERSION,
        generated_at: now,
        daemon_running: daemon_running(&paths),
        stale_after_ms,
        auto_swap: AutoSwapStatus {
            enabled: current.auto_swap.enabled,
            threshold: current.auto_swap.threshold,
        },
        warn_pct: current.quota.warn_pct,
        exhausted_pct: current.quota.exhausted_pct,
        provider_order: grouped.into_keys().collect(),
        accounts,
        request_metrics: include_metrics.then(request_metrics::snapshot),
    })
}

fn status_account(
    cache: &QuotaCache,
    raw: &RawQuotaCache,
    account: Account,
    provider_display_name: &str,
    now: DateTime<Utc>,
    stale_after_ms: i64,
) -> StatusAccount {
    let provider = account.provider.clone();
    let id = account.id.0.clone();
    let failure = cache.failure(&provider, &id);
    let error = failure.map(|failure| compact_error(&failure.error));
    // 原始观测时间：成功缓存写盘时记录，不随窗口 reset 或读取次数变化。
    let observed_at = raw
        .entries
        .get(&cache_key(&provider, &id))
        .map(|entry| entry.cached_at);
    let valid = cache.get(&provider, &id);

    let (fetch_state, cached_at, quotas) = match valid {
        Some(entry) => {
            // 比缓存更新的失败（例如 fresh 成功数据之后又收到 429）说明最近一次真实
            // 查询已失败；旧数字只能展示为 stale，不能继续保持 ready。
            let newer_failure = failure.is_some_and(|failure| failure.failed_at >= entry.cached_at);
            let state = if is_fresh(now, entry.cached_at, stale_after_ms) && !newer_failure {
                "ready"
            } else {
                "stale"
            };
            (state, Some(entry.cached_at), entry.quotas)
        }
        None if error.is_some() => ("failed", observed_at, Vec::new()),
        // 曾成功查询过，但所有窗口都已 reset/过期：保留观测时间并明确 stale，
        // 不能让 UI 误以为该账号从未查过，也不能继续展示旧数字。
        None if observed_at.is_some() => ("stale", observed_at, Vec::new()),
        None => ("missing", None, Vec::new()),
    };

    StatusAccount {
        id,
        provider,
        provider_display_name: provider_display_name.to_string(),
        display_label: safe_label(&account),
        active: account.active,
        billing: account.billing().to_string(),
        manual_only: account.manual_only(),
        fetch_state,
        error,
        cached_at,
        quotas,
    }
}

/// 观测时间是否仍可标为 ready。
///
/// 未来时间戳（时钟回拨/异常写入）不当作新鲜数据，保守降级为 stale。
fn is_fresh(now: DateTime<Utc>, observed_at: DateTime<Utc>, stale_after_ms: i64) -> bool {
    let age_ms = now.signed_duration_since(observed_at).num_milliseconds();
    age_ms >= 0 && age_ms < stale_after_ms
}

fn raw_cache(path: &Path) -> RawQuotaCache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn cache_key(provider: &str, account_id: &str) -> String {
    format!("{provider}::{account_id}")
}

/// Provider 只读展示名。
///
/// 正常命令从 `Provider::display_name()` 取；只读状态入口故意不构造 Provider，
/// 因此在这里保留静态展示元数据。新增 Provider 时必须同步（或改为由 core 提供
/// 无副作用的元数据表）。未知 Provider 回退到 id，避免输出为空。
fn provider_display_name(provider: &str) -> String {
    match provider {
        "claude" => "Claude / Anthropic",
        "codex" => "Codex / ChatGPT",
        "kimi" => "Kimi / Moonshot",
        "cursor" => "Cursor",
        "opencode" => "OpenCode",
        "opencode-api-key" => "OpenCode API Key",
        "commandcode" => "Command Code",
        other => other,
    }
    .to_string()
}

/// 输出到 UI / 日志前把疑似密钥的 label 换成安全标签。
///
/// 已有 `sk-…1234` 这类主动脱敏标签保持原样；API 账号优先显示 endpoint host。
fn safe_label(account: &Account) -> String {
    let label = account.label.trim();
    if label.is_empty() {
        return account.id.0.clone();
    }
    if label.contains('…') {
        return label.to_string();
    }
    if !contains_secret_token(label) {
        return label.to_string();
    }
    if let Some(host) = api_host(account) {
        return format!("API · {host}");
    }
    "API account".to_string()
}

/// label 本身或其中任一 token 像密钥时整体脱敏。
fn contains_secret_token(label: &str) -> bool {
    looks_like_secret(label) || label.split_whitespace().any(looks_like_secret)
}

fn looks_like_secret(text: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "sk-",
        "pk-",
        "rk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "Bearer ",
    ];
    if PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
        return true;
    }
    // 长、无空格、无 @，且只由 token 常见字符组成：按密钥处理。
    text.len() >= 32
        && !text.contains(char::is_whitespace)
        && !text.contains('@')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

fn api_host(account: &Account) -> Option<String> {
    let base_url = account
        .extra
        .get("api_config")?
        .get("base_url")?
        .as_str()?
        .trim();
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host = after_scheme.split('/').next()?.split('@').next_back()?;
    (!host.is_empty()).then(|| host.to_string())
}

fn compact_error(error: &str) -> String {
    let mut text = error.replace('\n', " ");
    if text.chars().count() > 160 {
        text = text.chars().take(157).collect::<String>() + "...";
    }
    text
}

fn daemon_running(paths: &AppPaths) -> bool {
    let pid_path = paths.daemon_pid_file();
    let Ok(raw) = std::fs::read_to_string(&pid_path) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    #[cfg(unix)]
    {
        // 信号 0 只探测进程是否存在；不向 daemon 发送任何信号。
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        pid_path.exists()
    }
}

fn print_human(snapshot: &StatusSnapshot) {
    println!(
        "subswap status · {} · daemon {} · auto-swap {} ({:.0}%)",
        snapshot.generated_at.to_rfc3339(),
        if snapshot.daemon_running {
            "running"
        } else {
            "offline"
        },
        if snapshot.auto_swap.enabled {
            "on"
        } else {
            "off"
        },
        snapshot.auto_swap.threshold * 100.0
    );

    let mut last_provider = "";
    for account in &snapshot.accounts {
        if account.provider != last_provider {
            if !last_provider.is_empty() {
                println!();
            }
            println!("{}", account.provider_display_name);
            last_provider = &account.provider;
        }
        let marker = if account.active { "*" } else { " " };
        let quotas = if account.quotas.is_empty() {
            match account.fetch_state {
                "failed" => "needs attention".to_string(),
                "missing" => "no quota data".to_string(),
                other => other.to_string(),
            }
        } else {
            account
                .quotas
                .iter()
                .map(|q| {
                    let used = if q.limit == 0 {
                        format!("{}", q.used)
                    } else {
                        format!("{:.0}%", q.used as f64 / q.limit as f64 * 100.0)
                    };
                    format!("{:?} {}", q.window, used)
                })
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let mut suffix = String::new();
        if account.manual_only {
            suffix.push_str(" · manual only");
        }
        if account.fetch_state == "stale" {
            suffix.push_str(" (stale)");
        }
        if account.fetch_state == "failed" {
            suffix.push_str(" (needs attention)");
        }
        println!(
            "  {marker} {:<36} {} · {}{}",
            account.display_label, account.billing, quotas, suffix
        );
    }
    if snapshot.accounts.is_empty() {
        println!("No accounts. Run `subswap login <provider>` first.");
    }
}
