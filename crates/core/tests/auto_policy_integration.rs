use chrono::Utc;
use subswap_core::{
    auto_decide, Account, AccountId, AccountWithQuotas, PolicyConfig, PolicyDecision,
    ProviderSnapshot, Quota, QuotaFetchState, QuotaStatus, QuotaWindow,
};

fn account(id: &str, active: bool) -> Account {
    Account {
        provider: "mock".into(),
        id: AccountId(id.into()),
        label: id.into(),
        active,
        created_at: Utc::now(),
        last_used_at: None,
        priority: 100,
        extra: serde_json::Map::new(),
    }
}

fn quota(id: &str, used: u64, status: QuotaStatus) -> Quota {
    Quota {
        provider: "mock".into(),
        account_id: AccountId(id.into()),
        window: QuotaWindow::FiveHour,
        used,
        limit: 100,
        reset_at: None,
        status,
        note: None,
    }
}

fn awq(id: &str, active: bool, used: u64, status: QuotaStatus) -> AccountWithQuotas {
    AccountWithQuotas {
        account: account(id, active),
        quotas: vec![quota(id, used, status)],
        fetch_state: QuotaFetchState::Ready,
    }
}

fn snapshot(accounts: Vec<AccountWithQuotas>) -> ProviderSnapshot {
    ProviderSnapshot {
        provider: "mock".into(),
        accounts,
    }
}

#[test]
fn warn_below_auto_threshold_does_not_swap() {
    let snap = snapshot(vec![
        awq("active", true, 90, QuotaStatus::Warn),
        awq("candidate", false, 1, QuotaStatus::Ok),
    ]);

    assert!(matches!(
        auto_decide(&snap, &PolicyConfig::default()),
        PolicyDecision::NoOp { .. }
    ));
}

#[test]
fn default_threshold_swaps_at_99_percent() {
    let snap = snapshot(vec![
        awq("active", true, 99, QuotaStatus::Warn),
        awq("candidate", false, 1, QuotaStatus::Ok),
    ]);

    match auto_decide(&snap, &PolicyConfig::default()) {
        PolicyDecision::Swap { from, to, .. } => {
            assert_eq!(from.unwrap().0, "active");
            assert_eq!(to.0, "candidate");
        }
        other => panic!("expected swap at default threshold, got {other:?}"),
    }
}

#[test]
fn seven_day_threshold_does_not_swap_at_99_percent() {
    let mut active = awq("active", true, 99, QuotaStatus::Warn);
    active.quotas[0].window = QuotaWindow::SevenDay;
    let snap = snapshot(vec![active, awq("candidate", false, 1, QuotaStatus::Ok)]);

    assert!(matches!(
        auto_decide(&snap, &PolicyConfig::default()),
        PolicyDecision::NoOp { .. }
    ));
}

#[test]
fn active_quota_fetch_error_keeps_current_account() {
    let mut active = awq("active", true, 0, QuotaStatus::Unknown);
    active.fetch_state = QuotaFetchState::Failed("429 too many requests".into());
    let snap = snapshot(vec![active, awq("candidate", false, 1, QuotaStatus::Ok)]);

    assert!(matches!(
        auto_decide(&snap, &PolicyConfig::default()),
        PolicyDecision::Degraded { .. }
    ));
}

#[test]
fn exhausted_active_keeps_current_account_without_confirmed_target() {
    let mut candidate = awq("candidate", false, 0, QuotaStatus::Unknown);
    candidate.quotas.clear();
    candidate.fetch_state = QuotaFetchState::Failed("429 too many requests".into());
    let snap = snapshot(vec![
        awq("active", true, 100, QuotaStatus::Exhausted),
        candidate,
    ]);

    assert!(matches!(
        auto_decide(&snap, &PolicyConfig::default()),
        PolicyDecision::Degraded { .. }
    ));
}

// 所有 Provider 共用同一决策；先返回的候选不能触发健康账号的切换。
#[test]
fn all_providers_preserve_uncertain_active_and_require_a_usable_target() {
    for provider in [
        "claude",
        "codex",
        "kimi",
        "cursor",
        "opencode",
        "opencode-api-key",
        "commandcode",
    ] {
        let make_snapshot = |mut accounts: Vec<AccountWithQuotas>| {
            for a in &mut accounts {
                a.account.provider = provider.into();
                for q in &mut a.quotas {
                    q.provider = provider.into();
                }
            }
            ProviderSnapshot {
                provider: provider.into(),
                accounts,
            }
        };
        let policy = PolicyConfig {
            manual_hold_ms: 0,
            settle_grace_ms: 0,
            ..PolicyConfig::default()
        };
        for state in [
            QuotaFetchState::Loading,
            QuotaFetchState::Failed("timeout".into()),
            QuotaFetchState::Failed("429 too many requests".into()),
            QuotaFetchState::Failed("401 needs re-login".into()),
            QuotaFetchState::Stale {
                cached_at: Utc::now() - chrono::Duration::hours(1),
                error: "timeout".into(),
            },
        ] {
            // 即便快照里残留耗尽额度，未完成/失败的查询也不是触发证据。
            let mut active = awq("active", true, 100, QuotaStatus::Exhausted);
            active.fetch_state = state.clone();
            let decision = auto_decide(
                &make_snapshot(vec![active, awq("candidate", false, 0, QuotaStatus::Ok)]),
                &policy,
            );
            assert!(
                !matches!(decision, PolicyDecision::Swap { .. }),
                "{provider} {state:?}: {decision:?}"
            );

            let mut candidate = awq("candidate", false, 0, QuotaStatus::Ok);
            candidate.fetch_state = state;
            let decision = auto_decide(
                &make_snapshot(vec![
                    awq("active", true, 100, QuotaStatus::Exhausted),
                    candidate,
                ]),
                &policy,
            );
            assert!(
                !matches!(decision, PolicyDecision::Swap { .. }),
                "{provider}: {decision:?}"
            );
        }
        for used in [0, 2, 98] {
            let active = awq("active", true, used, QuotaStatus::Ok);
            let decision = auto_decide(
                &make_snapshot(vec![active, awq("candidate", false, 0, QuotaStatus::Ok)]),
                &policy,
            );
            assert!(
                matches!(decision, PolicyDecision::NoOp { .. }),
                "{provider}: {decision:?}"
            );
        }
        for (used, status, limit) in [
            (100, QuotaStatus::Unknown, 100),
            (100, QuotaStatus::Exhausted, 0),
        ] {
            let mut active = awq("active", true, used, status);
            active.quotas[0].limit = limit;
            let decision = auto_decide(
                &make_snapshot(vec![active, awq("candidate", false, 0, QuotaStatus::Ok)]),
                &policy,
            );
            assert!(
                matches!(decision, PolicyDecision::NoOp { .. }),
                "{provider}: {decision:?}"
            );
        }
        // 保留真正的耗尽/阈值切换。
        let decision = auto_decide(
            &make_snapshot(vec![
                awq("active", true, 100, QuotaStatus::Exhausted),
                awq("candidate", false, 0, QuotaStatus::Ok),
            ]),
            &policy,
        );
        assert!(
            matches!(decision, PolicyDecision::Swap { to, .. } if to.0 == "candidate"),
            "{provider}"
        );
    }
}

#[test]
fn cursor_unknown_parallel_pools_do_not_establish_exhaustion() {
    let mut active = awq("active", true, 100, QuotaStatus::Exhausted);
    active.account.provider = "cursor".into();
    active.quotas[0].window = QuotaWindow::FirstPartyModels;
    let mut unknown = quota("active", 100, QuotaStatus::Unknown);
    unknown.window = QuotaWindow::Api;
    active.quotas.push(unknown);
    let mut candidate = awq("candidate", false, 0, QuotaStatus::Ok);
    candidate.account.provider = "cursor".into();
    candidate.quotas[0].window = QuotaWindow::FirstPartyModels;
    let snap = ProviderSnapshot {
        provider: "cursor".into(),
        accounts: vec![active, candidate],
    };
    assert!(matches!(
        auto_decide(&snap, &PolicyConfig::default()),
        PolicyDecision::NoOp { .. }
    ));
}
