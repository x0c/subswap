use chrono::Utc;
use subswap_core::{
    auto_decide, Account, AccountId, AccountWithQuotas, PolicyConfig, PolicyDecision,
    ProviderSnapshot, Quota, QuotaStatus, QuotaWindow,
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
        window: QuotaWindow::Month,
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
        fetch_error: None,
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
fn active_quota_fetch_error_degrades_instead_of_guessing() {
    let mut active = awq("active", true, 0, QuotaStatus::Unknown);
    active.fetch_error = Some("429 too many requests".into());
    let snap = snapshot(vec![active, awq("candidate", false, 1, QuotaStatus::Ok)]);

    assert!(matches!(
        auto_decide(&snap, &PolicyConfig::default()),
        PolicyDecision::Degraded { .. }
    ));
}
