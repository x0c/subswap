//! `subswap`（无参）默认入口。
//!
//! 流程：sync_local_active → 先渲染骨架 → 并发拉 quota 渐进刷新 → per-provider AutoSwapPolicy → 最终渲染。
//! 详见 docs/design/ARCHITECTURE.md §3.1。

use std::collections::HashSet;
use std::io::{self, IsTerminal};

use anyhow::Result;
use futures::future::join_all;
use subswap_core::{
    auto_decide, paths::AppPaths, query_quota_with_retry, AccountId, AccountWithQuotas, AuditEvent,
    AuditLog, PolicyConfig, PolicyDecision, ProviderRegistry, ProviderSnapshot, Quota, QuotaCache,
    QuotaFetchState,
};

use crate::app::AppContext;
use crate::daemon_spawn::ensure_daemon_running;
use crate::render::{compact_error, compact_policy_reason, AutoLine, AutoLineKind, InlineRenderer};

pub async fn run(ctx: &AppContext) -> Result<()> {
    // 1. 自动 import 本地激活账号（如果没记录过）。
    sync_local_active(ctx);

    // 2. 先输出账号骨架，再随 quota 请求完成原地刷新。
    let interactive = io::stdout().is_terminal();
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

    // 3. 最终渲染。交互场景刷新原输出块；非交互场景只输出最终版。
    renderer.render(&snapshots, &auto_lines)?;

    // 4. 后台保活:用户无感地拉起 daemon(已经在跑则什么都不做)。
    //    失败仅 debug 日志,不影响默认命令的退出码。
    if let Err(e) = ensure_daemon_running() {
        tracing::debug!(err = %e, "ensure_daemon_running failed; continuing");
    }
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

    let mut jobs = Vec::new();
    for snap in snapshots.iter_mut() {
        let provider = snap.provider.clone();
        for awq in &mut snap.accounts {
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

    let mut decided_providers = HashSet::new();
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
            &mut decided_providers,
        )
        .await?;
        if let Some(renderer) = renderer.as_deref_mut() {
            renderer.render(snapshots, auto_lines)?;
        }
    }
    cache.save(cache_path);
    Ok(())
}

async fn try_auto_swap_ready_provider(
    registry: &ProviderRegistry,
    audit: &AuditLog,
    snapshots: &mut [ProviderSnapshot],
    provider: &str,
    cfg: &PolicyConfig,
    auto_lines: &mut Vec<AutoLine>,
    decided_providers: &mut HashSet<String>,
) -> Result<()> {
    if decided_providers.contains(provider) {
        return Ok(());
    }
    let Some(index) = snapshots.iter().position(|snap| snap.provider == provider) else {
        return Ok(());
    };
    let snap = &snapshots[index];
    if snap.accounts.is_empty() {
        return Ok(());
    }
    let has_loading = snap.accounts.iter().any(|a| a.fetch_state.is_loading());
    let decision = auto_decide(snap, cfg);
    if has_loading && !matches!(decision, PolicyDecision::Swap { .. }) {
        return Ok(());
    }
    decided_providers.insert(provider.to_string());

    match decision {
        PolicyDecision::Swap { to, .. } => {
            let p = registry.get(provider)?;
            let success_text = auto_swap_success_text(snap, &to);
            match p.activate(&to).await {
                Ok(()) => {
                    auto_lines.push(AutoLine {
                        provider: provider.to_string(),
                        text: success_text,
                        kind: AutoLineKind::Info,
                    });
                    audit.append(AuditEvent::ok("auto_swap", provider, Some(to.0.as_str())));
                    mark_active(snapshots, provider, &to);
                }
                Err(e) => {
                    auto_lines.push(AutoLine {
                        provider: provider.to_string(),
                        text: format!("auto: failed ({})", compact_error(&e.to_string())),
                        kind: AutoLineKind::Error,
                    });
                    audit.append(AuditEvent::err(
                        "auto_swap",
                        provider,
                        Some(to.0.as_str()),
                        &e.to_string(),
                    ));
                }
            }
        }
        PolicyDecision::Degraded { reason } => {
            tracing::debug!(
                provider=%provider,
                reason=%compact_policy_reason(&reason),
                "auto swap degraded"
            );
        }
        PolicyDecision::NoOp { .. } => {} // 沉默是金
    }

    Ok(())
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
            window: QuotaWindow::Month,
            used,
            limit: 100,
            reset_at: None,
            status,
            note: None,
        }
    }

    #[tokio::test]
    async fn ready_provider_auto_swaps_before_other_provider_finishes() {
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
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            threshold: 0.98,
            allow_unknown: false,
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
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            threshold: 0.98,
            allow_unknown: false,
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
}
