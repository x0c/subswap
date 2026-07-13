//! `subswap`（无参）默认入口。
//!
//! 流程：sync_local_active → 先渲染骨架 → 并发拉 quota 渐进刷新 → per-provider AutoSwapPolicy → 最终渲染。
//! 详见 docs/design/ARCHITECTURE.md §3.1。

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::Result;
use futures::future::join_all;
use subswap_core::{
    auto_decide, paths::AppPaths, query_quota_with_retry, settings, AccountId, AccountWithQuotas,
    AuditEvent, AuditLog, PolicyConfig, PolicyDecision, ProviderRegistry, ProviderSnapshot, Quota,
    QuotaCache, QuotaFetchState,
};

use crate::app::AppContext;
use crate::daemon_spawn::ensure_daemon_running;
use crate::render::{compact_error, compact_policy_reason, AutoLine, AutoLineKind, InlineRenderer};

pub async fn run(ctx: &AppContext, json: bool) -> Result<()> {
    // 1. 自动 import 本地激活账号（如果没记录过）。
    sync_local_active(ctx);

    // 2. 先输出账号骨架，再随 quota 请求完成原地刷新。
    // JSON 模式强制走非交互路径（不渲染 ANSI 骨架），最后统一以 JSON 输出。
    let interactive = !json && io::stdout().is_terminal();
    let mut snapshots = build_loading_snapshots(&ctx.providers).await;
    let mut renderer = InlineRenderer::new(interactive);
    if interactive {
        renderer.render(&snapshots, &[])?;
    }
    let cfg = PolicyConfig::default();
    let mut auto_lines: Vec<AutoLine> = Vec::new();
    let cache_path = AppPaths::resolve()
        .map(|p| p.quota_cache_file())
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/subswap_quota_cache.json"));
    fill_quotas_progressively(
        &ctx.providers,
        &ctx.audit,
        &mut snapshots,
        &cfg,
        &mut auto_lines,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
        &cache_path,
    )
    .await?;

    // 3. 最终输出。JSON 模式吐结构化快照供程序消费；否则人类渲染（交互刷新原块 / 非交互出最终版）。
    if json {
        print_quota_json(&snapshots)?;
    } else {
        renderer.render(&snapshots, &auto_lines)?;
    }

    // 4. 后台保活:用户无感地拉起 daemon(已经在跑则什么都不做)。
    //    失败仅 debug 日志,不影响默认命令的退出码。
    if let Err(e) = ensure_daemon_running() {
        tracing::debug!(err = %e, "ensure_daemon_running failed; continuing");
    }
    Ok(())
}

/// JSON 输出用 DTO：每个账号一条，含额度窗口与各自 reset_at，供程序（如 OpenConductor）消费。
#[derive(serde::Serialize)]
struct AccountQuotaJson {
    id: String,
    provider: String,
    label: String,
    active: bool,
    /// 计费方式：flat（订阅固定费率）| metered（按量计费）| unlimited（不限量）。
    /// 给 OpenConductor 等下游消费者判断"是否真花钱"并据此排权重。
    billing: String,
    /// quota 拉取状态：ready | loading | failed | stale。
    fetch_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// 各窗口快照（Quota 自身可序列化，含 window / used / limit / reset_at / status）。
    quotas: Vec<Quota>,
}

/// 把账号 + 额度快照以 JSON 数组打到 stdout。
fn print_quota_json(snapshots: &[ProviderSnapshot]) -> Result<()> {
    let mut accounts = Vec::new();
    for snap in snapshots {
        for awq in &snap.accounts {
            let (fetch_state, error) = match &awq.fetch_state {
                QuotaFetchState::Loading => ("loading", None),
                QuotaFetchState::Ready => ("ready", None),
                QuotaFetchState::Failed(e) => ("failed", Some(e.clone())),
                QuotaFetchState::Stale { error, .. } => ("stale", Some(error.clone())),
            };
            accounts.push(AccountQuotaJson {
                id: awq.account.id.0.clone(),
                provider: awq.account.provider.clone(),
                label: awq.account.label.clone(),
                active: awq.account.active,
                billing: awq.account.billing().to_string(),
                fetch_state,
                error,
                quotas: awq.quotas.clone(),
            });
        }
    }
    println!("{}", serde_json::to_string_pretty(&accounts)?);
    Ok(())
}

fn auto_swap_success_text(snap: &ProviderSnapshot, to: &AccountId) -> String {
    let target = snap
        .accounts
        .iter()
        .find(|a| a.account.id == *to)
        .and_then(|a| {
            let label = a.account.label.trim();
            if label.is_empty() || label == a.account.id.0.as_str() {
                None
            } else {
                Some(label.to_string())
            }
        });

    match target {
        Some(label) => format!("auto: swapped to {label}"),
        None => "auto: swapped".into(),
    }
}

/// 扫本地 ~/.claude / ~/.codex；如果有当前激活账号则 import 到 registry（已存在时 upsert）。
/// 任一 provider 失败（用户没登录过）静默跳过。
fn sync_local_active(ctx: &AppContext) {
    if default_entry_avoids_keychain_sync() {
        sync_local_active_metadata(ctx);
        return;
    }
    match ctx.claude.import_active(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("claude", &account.id) {
                tracing::debug!(err=%e, "skip claude active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip claude auto-import"),
    }
    match ctx.codex.sync_active_metadata(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("codex", &account.id) {
                tracing::debug!(err=%e, "skip codex active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip codex auto-import"),
    }
}

fn sync_local_active_metadata(ctx: &AppContext) {
    match ctx.claude.sync_active_metadata(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("claude", &account.id) {
                tracing::debug!(err=%e, "skip claude active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip claude active metadata sync"),
    }
    match ctx.codex.sync_active_metadata(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("codex", &account.id) {
                tracing::debug!(err=%e, "skip codex active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip codex active metadata sync"),
    }
}

#[cfg(target_os = "macos")]
fn default_entry_avoids_keychain_sync() -> bool {
    std::env::var_os("SUBSWAP_SYNC_KEYCHAIN_ON_START").is_none()
}

#[cfg(not(target_os = "macos"))]
fn default_entry_avoids_keychain_sync() -> bool {
    false
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
                    fetch_state: QuotaFetchState::Loading,
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

/// 渐进式自动切换的运行内状态:额度是边查边回的,不能查到第一份就把决策锁死。
#[derive(Default)]
struct AutoSwapProgress {
    /// 本次运行内每个 provider 已切到的目标,避免重复 activate(重写凭证)。
    activated_targets: HashMap<String, AccountId>,
    /// 本次运行内主动离开过的账号,「只升级、不回头」防止 A→B→A 抖动。
    abandoned: HashMap<String, HashSet<AccountId>>,
}

async fn fill_quotas_progressively(
    registry: &ProviderRegistry,
    audit: &AuditLog,
    snapshots: &mut [ProviderSnapshot],
    cfg: &PolicyConfig,
    auto_lines: &mut Vec<AutoLine>,
    mut renderer: Option<&mut InlineRenderer>,
    cache_path: &std::path::Path,
) -> Result<()> {
    let total: usize = snapshots.iter().map(|snap| snap.accounts.len()).sum();
    if total == 0 {
        return Ok(());
    }
    let mut cache = QuotaCache::load(cache_path);
    let min_refresh = Duration::from_millis(settings::current().quota.min_refresh_interval_ms);

    let mut jobs = Vec::new();
    for snap in snapshots.iter_mut() {
        let provider = snap.provider.clone();
        for awq in &mut snap.accounts {
            // 缓存节流：缓存够新(< min_refresh)就直接复用、不打 usage 端点，避免高频触发 429。
            // daemon 与 CLI 共用 quota_cache.json，谁先查到谁刷新 cached_at，另一方据此跳过。
            if let Some(entry) = cache.fresh(&provider, &awq.account.id.0, min_refresh) {
                awq.quotas = entry.quotas;
                awq.fetch_state = QuotaFetchState::Ready;
                continue;
            }
            // 凭证已走明文 FileStore，查任何账号都不再弹钥匙串，激活/非激活一律查额度。
            jobs.push((provider.clone(), awq.account.id.clone()));
        }
    }
    if let Some(renderer) = renderer.as_deref_mut() {
        renderer.render(snapshots, auto_lines)?;
    }
    if jobs.is_empty() {
        return Ok(());
    }

    let mut progress = AutoSwapProgress::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(jobs.len());
    for (provider, account_id) in jobs {
        let p = registry.get(&provider)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = query_quota_with_retry(p.as_ref(), &account_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx
                .send(QuotaUpdate {
                    provider,
                    account_id,
                    result,
                })
                .await;
        });
    }
    drop(tx);

    while let Some(update) = rx.recv().await {
        let provider = update.provider.clone();
        apply_quota_update(snapshots, update, &mut cache);
        try_auto_swap_ready_provider(
            registry,
            audit,
            snapshots,
            &provider,
            cfg,
            auto_lines,
            &mut progress,
        )
        .await?;
        if let Some(renderer) = renderer.as_deref_mut() {
            renderer.render(snapshots, auto_lines)?;
        }
    }
    cache.save(cache_path);
    Ok(())
}

/// 每收到一份额度就对该 provider 重判一次,而不是查到第一份就锁死决策。
///
/// 这样做是为了修掉一个时序竞态:渐进式拉额度时,更优的候选可能还没查回来,
/// 此时只能先切到一个「逃生候选」(查询失败/loading 时的兜底);等更优候选的额度
/// 落地后,本函数会再判一次并升级过去——一次 `subswap` 内自我纠正,无需用户再跑一遍。
///
/// 防抖动:`auto_decide` 只在当前 active 确实不行(耗尽/超阈值/loading/失败)时才返回
/// Swap,所以切到一个真正可用的号后会自然 NoOp;再叠加 `abandoned`「不切回已离开的号」,
/// 保证决策随额度补全单调收敛,不会 A→B→A 来回顶。
async fn try_auto_swap_ready_provider(
    registry: &ProviderRegistry,
    audit: &AuditLog,
    snapshots: &mut [ProviderSnapshot],
    provider: &str,
    cfg: &PolicyConfig,
    auto_lines: &mut Vec<AutoLine>,
    progress: &mut AutoSwapProgress,
) -> Result<()> {
    let Some(index) = snapshots.iter().position(|snap| snap.provider == provider) else {
        return Ok(());
    };
    let snap = &snapshots[index];
    if snap.accounts.is_empty() {
        return Ok(());
    }

    let (from, to) = match auto_decide(snap, cfg) {
        PolicyDecision::Swap { from, to, .. } => (from, to),
        PolicyDecision::Degraded { reason } => {
            tracing::debug!(
                provider=%provider,
                reason=%compact_policy_reason(&reason),
                "auto swap degraded"
            );
            return Ok(());
        }
        // 沉默是金。额度可能还在补,下一份回来时会重判,不在此处锁死。
        PolicyDecision::NoOp { .. } => return Ok(()),
    };

    // 已经切到过这个目标:无需重复 activate(重写凭证)。
    if progress.activated_targets.get(provider) == Some(&to) {
        return Ok(());
    }
    // 只升级、不回头:本次运行内主动离开过的账号不再切回,避免抖动。
    if progress
        .abandoned
        .get(provider)
        .is_some_and(|left| left.contains(&to))
    {
        return Ok(());
    }

    let p = registry.get(provider)?;
    let success_text = auto_swap_success_text(snap, &to);
    match p.activate(&to).await {
        Ok(()) => {
            set_auto_line(auto_lines, provider, success_text, AutoLineKind::Info);
            audit.append(AuditEvent::ok("auto_swap", provider, Some(to.0.as_str())));
            mark_active(snapshots, provider, &to);
            if let Some(from) = from {
                progress
                    .abandoned
                    .entry(provider.to_string())
                    .or_default()
                    .insert(from);
            }
            progress.activated_targets.insert(provider.to_string(), to);
        }
        Err(e) => {
            set_auto_line(
                auto_lines,
                provider,
                format!("auto: failed ({})", compact_error(&e.to_string())),
                AutoLineKind::Error,
            );
            audit.append(AuditEvent::err(
                "auto_swap",
                provider,
                Some(to.0.as_str()),
                &e.to_string(),
            ));
        }
    }

    Ok(())
}
/// 同一 provider 的自动切换提示原地替换,保证最终只展示一行最新结果
/// (例如先切逃生号、再升级到更优号时,只显示升级后的那条)。
fn set_auto_line(auto_lines: &mut Vec<AutoLine>, provider: &str, text: String, kind: AutoLineKind) {
    if let Some(line) = auto_lines.iter_mut().find(|l| l.provider == provider) {
        line.text = text;
        line.kind = kind;
    } else {
        auto_lines.push(AutoLine {
            provider: provider.to_string(),
            text,
            kind,
        });
    }
}

fn apply_quota_update(
    snapshots: &mut [ProviderSnapshot],
    update: QuotaUpdate,
    cache: &mut QuotaCache,
) {
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
            cache.set(&update.provider, &update.account_id.0, quotas.clone());
            awq.quotas = quotas;
            awq.fetch_state = QuotaFetchState::Ready;
        }
        Err(err) => {
            if let Some(entry) = cache.get(&update.provider, &update.account_id.0) {
                awq.quotas = entry.quotas.clone();
                awq.fetch_state = QuotaFetchState::Stale {
                    cached_at: entry.cached_at,
                    error: err,
                };
            } else {
                awq.quotas.clear();
                awq.fetch_state = QuotaFetchState::Failed(err);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use subswap_core::{
        Account, ClientTarget, Provider, QuotaFetchState, QuotaStatus, QuotaWindow,
    };
    use tokio::sync::{mpsc, Notify};

    fn snap_with_account(id: &str, label: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: "codex".into(),
            accounts: vec![AccountWithQuotas {
                account: Account {
                    provider: "codex".into(),
                    id: AccountId(id.into()),
                    label: label.into(),
                    active: false,
                    created_at: Utc::now(),
                    last_used_at: None,
                    priority: 100,
                    extra: serde_json::Map::new(),
                },
                quotas: Vec::new(),
                fetch_state: QuotaFetchState::Ready,
            }],
        }
    }

    #[test]
    fn auto_swap_success_text_uses_friendly_label() {
        let snap = snap_with_account(
            "c1311d9b-47d1-4b8b-95e9-3401f967abd6",
            "stromandanika707621@gmail.com",
        );

        assert_eq!(
            auto_swap_success_text(
                &snap,
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into())
            ),
            "auto: swapped to stromandanika707621@gmail.com"
        );
    }

    #[test]
    fn auto_swap_success_text_hides_raw_id_without_label() {
        let snap = snap_with_account(
            "c1311d9b-47d1-4b8b-95e9-3401f967abd6",
            "c1311d9b-47d1-4b8b-95e9-3401f967abd6",
        );

        assert_eq!(
            auto_swap_success_text(
                &snap,
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into())
            ),
            "auto: swapped"
        );
    }

    struct MockProvider {
        id: &'static str,
        accounts: Vec<Account>,
        quotas: HashMap<String, Vec<Quota>>,
        wait_for_quota: Option<Arc<Notify>>,
        wait_by_account: HashMap<String, Arc<Notify>>,
        // 这些账号的 quota 查询返回 Err,模拟拉取失败 → fetch_state=Failed。
        fail_accounts: HashSet<String>,
        activated: mpsc::UnboundedSender<(String, String)>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn display_name(&self) -> &'static str {
            self.id
        }

        fn client_targets(&self) -> Vec<ClientTarget> {
            Vec::new()
        }

        async fn list_accounts(&self) -> subswap_core::Result<Vec<Account>> {
            Ok(self.accounts.clone())
        }

        async fn activate(&self, id: &AccountId) -> subswap_core::Result<()> {
            let _ = self.activated.send((self.id.to_string(), id.0.clone()));
            Ok(())
        }

        async fn query_quota(&self, id: &AccountId) -> subswap_core::Result<Vec<Quota>> {
            if let Some(wait_for_quota) = self.wait_by_account.get(&id.0) {
                wait_for_quota.notified().await;
            } else if let Some(wait_for_quota) = &self.wait_for_quota {
                wait_for_quota.notified().await;
            }
            if self.fail_accounts.contains(&id.0) {
                // 用非重试错误(429),让失败快速落地为 Failed,不被 query_quota_with_retry
                // 的指数退避拖慢——否则测试里 escape 的失败状态会迟迟不到。
                return Err(subswap_core::Error::QuotaFetch(
                    "usage returned 429 too many requests".into(),
                ));
            }
            Ok(self.quotas.get(&id.0).cloned().unwrap_or_default())
        }
    }

    fn account(provider: &str, id: &str, active: bool) -> Account {
        Account {
            provider: provider.into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    fn quota(provider: &str, id: &str, used: u64, status: QuotaStatus) -> Quota {
        Quota {
            provider: provider.into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used,
            limit: 100,
            reset_at: None,
            status,
            note: None,
        }
    }

    #[tokio::test]
    async fn ready_provider_auto_swaps_and_never_reports_isolated_session_skip() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        let slow_claude = Arc::new(Notify::new());

        let mut codex_quotas = HashMap::new();
        codex_quotas.insert(
            "codex-active".into(),
            vec![quota("codex", "codex-active", 99, QuotaStatus::Warn)],
        );
        codex_quotas.insert(
            "codex-candidate".into(),
            vec![quota("codex", "codex-candidate", 1, QuotaStatus::Ok)],
        );

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "claude",
            accounts: vec![account("claude", "claude-active", true)],
            quotas: HashMap::new(),
            wait_for_quota: Some(slow_claude.clone()),
            wait_by_account: HashMap::new(),
            fail_accounts: HashSet::new(),
            activated: activated_tx.clone(),
        }));
        registry.register(Arc::new(MockProvider {
            id: "codex",
            accounts: vec![
                account("codex", "codex-active", true),
                account("codex", "codex-candidate", false),
            ],
            quotas: codex_quotas,
            wait_for_quota: None,
            wait_by_account: HashMap::new(),
            fail_accounts: HashSet::new(),
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            settle_grace_ms: 60_000,
        };
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            fill_quotas_progressively(
                &registry,
                &audit,
                &mut snapshots,
                &cfg,
                &mut auto_lines,
                None,
                &cache_path,
            )
            .await
            .unwrap();
            (snapshots, auto_lines)
        });

        let activated = tokio::time::timeout(Duration::from_millis(300), activated_rx.recv())
            .await
            .expect("codex should activate before claude quota finishes")
            .expect("activation channel should stay open");
        assert_eq!(
            activated,
            ("codex".to_string(), "codex-candidate".to_string())
        );

        slow_claude.notify_waiters();
        let (snapshots, auto_lines) = handle.await.unwrap();
        let codex = snapshots
            .iter()
            .find(|snap| snap.provider == "codex")
            .unwrap();
        assert!(codex
            .accounts
            .iter()
            .any(|account| account.account.id.0 == "codex-candidate" && account.account.active));
        assert_eq!(auto_lines.len(), 1);
        assert_eq!(auto_lines[0].provider, "codex");
        assert!(
            !auto_lines[0].text.contains("isolated session active"),
            "auto swap must activate instead of skipping for an isolated session: {}",
            auto_lines[0].text
        );
    }

    #[tokio::test]
    async fn active_loading_auto_swaps_to_known_candidate() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        let slow_active = Arc::new(Notify::new());

        let mut quotas = HashMap::new();
        quotas.insert(
            "active".into(),
            vec![quota("claude", "active", 10, QuotaStatus::Ok)],
        );
        quotas.insert(
            "candidate".into(),
            vec![quota("claude", "candidate", 0, QuotaStatus::Ok)],
        );

        let mut wait_by_account = HashMap::new();
        wait_by_account.insert("active".into(), slow_active.clone());

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "claude",
            accounts: vec![
                account("claude", "active", true),
                account("claude", "candidate", false),
            ],
            quotas,
            wait_for_quota: None,
            wait_by_account,
            fail_accounts: HashSet::new(),
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            settle_grace_ms: 60_000,
        };
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            fill_quotas_progressively(
                &registry,
                &audit,
                &mut snapshots,
                &cfg,
                &mut auto_lines,
                None,
                &cache_path,
            )
            .await
            .unwrap();
            (snapshots, auto_lines)
        });

        let activated = tokio::time::timeout(Duration::from_millis(300), activated_rx.recv())
            .await
            .expect("candidate should activate while active quota is still loading")
            .expect("activation channel should stay open");
        assert_eq!(activated, ("claude".to_string(), "candidate".to_string()));

        slow_active.notify_waiters();
        let (snapshots, auto_lines) = handle.await.unwrap();
        let claude = snapshots
            .iter()
            .find(|snap| snap.provider == "claude")
            .unwrap();
        assert!(claude
            .accounts
            .iter()
            .any(|account| account.account.id.0 == "candidate" && account.account.active));
        assert_eq!(auto_lines.len(), 1);
        assert_eq!(auto_lines[0].provider, "claude");
    }

    /// 复现并验证修复:active 已耗尽时,先到的「逃生候选」(escape,额度查询失败)
    /// 被抢先切上;待真正可用的更优候选额度落地后,应在同一次运行内自动升级过去,
    /// 且最终只保留一条提示行。修复前会锁死在 escape,需用户再跑一次 `subswap` 才纠正。
    #[tokio::test]
    async fn upgrades_from_escape_candidate_when_better_quota_arrives() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        // better 候选额度拉取放慢,确保 active 先耗尽、escape 先被选中。
        let slow_better = Arc::new(Notify::new());

        let mut quotas = HashMap::new();
        // active 已耗尽 → 必须切走。
        quotas.insert(
            "active".into(),
            vec![quota("claude", "active", 100, QuotaStatus::Exhausted)],
        );
        // better 真正可用,但额度回得慢。
        quotas.insert(
            "better".into(),
            vec![quota("claude", "better", 0, QuotaStatus::Ok)],
        );
        // escape 的 quota 查询失败 → fetch_state=Failed。allow_unknown=false 下它不是常规
        // viable 候选,只能走 auto_decide 的「失败候选逃生」兜底被先切上,随后被 better 顶替。
        let mut wait_by_account = HashMap::new();
        wait_by_account.insert("better".into(), slow_better.clone());
        let mut fail_accounts = HashSet::new();
        fail_accounts.insert("escape".to_string());

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "claude",
            accounts: vec![
                account("claude", "active", true),
                account("claude", "escape", false),
                account("claude", "better", false),
            ],
            quotas,
            wait_for_quota: None,
            wait_by_account,
            fail_accounts,
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            // 关闭沉淀宽限:否则切到 escape(Failed)后会被 settle-grace 拦住升级。
            settle_grace_ms: 0,
        };
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            fill_quotas_progressively(
                &registry,
                &audit,
                &mut snapshots,
                &cfg,
                &mut auto_lines,
                None,
                &cache_path,
            )
            .await
            .unwrap();
            (snapshots, auto_lines)
        });

        // 第一跳:better 还在 loading,先切到 escape。
        let first = tokio::time::timeout(Duration::from_millis(300), activated_rx.recv())
            .await
            .expect("escape candidate should activate first")
            .expect("activation channel open");
        assert_eq!(first, ("claude".to_string(), "escape".to_string()));

        // better 额度落地 → 应升级到 better。
        slow_better.notify_waiters();
        let second = tokio::time::timeout(Duration::from_millis(300), activated_rx.recv())
            .await
            .expect("should upgrade to better candidate once its quota arrives")
            .expect("activation channel open");
        assert_eq!(second, ("claude".to_string(), "better".to_string()));

        let (snapshots, auto_lines) = handle.await.unwrap();
        let claude = snapshots
            .iter()
            .find(|snap| snap.provider == "claude")
            .unwrap();
        assert!(claude
            .accounts
            .iter()
            .any(|a| a.account.id.0 == "better" && a.account.active));
        // 升级后不再切回已离开的 escape / active。
        assert!(claude
            .accounts
            .iter()
            .all(|a| a.account.id.0 == "better" || !a.account.active));
        // 多次切换只保留一条最新提示行。
        assert_eq!(auto_lines.len(), 1);
        assert_eq!(auto_lines[0].provider, "claude");
    }
}
