//! `quota_collection::collect_quota_at` 的隔离集成测试。
//!
//! 全部使用显式临时 cache 路径与显式 `Settings`，不读真实账号、不写全局目录、不发网络
//! 请求。覆盖成功写回、原始观测时间保留、缓存命中、并发合并（coalescing）、取消后的
//! 请求下限、超时/鉴权失败退避。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use subswap_core::quota_collection::read_status;
use subswap_core::settings::Settings;
use subswap_core::{
    collect_quota_at, Account, AccountId, ClientTarget, CollectionState, Error, Provider, Quota,
    QuotaCache, QuotaReading, QuotaStatus, QuotaWindow, Result,
};

/// 可控的假 Provider：按固定延迟返回成功或指定错误。
struct FakeProvider {
    calls: AtomicUsize,
    delay: Duration,
    outcome: Outcome,
}

enum Outcome {
    Success,
    Unauthorized,
}

impl FakeProvider {
    fn success(delay_ms: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(delay_ms),
            outcome: Outcome::Success,
        }
    }

    fn unauthorized(delay_ms: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(delay_ms),
            outcome: Outcome::Unauthorized,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn display_name(&self) -> &'static str {
        "Fake"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        Vec::new()
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        Ok(Vec::new())
    }

    async fn activate(&self, _id: &AccountId) -> Result<()> {
        Ok(())
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        match self.outcome {
            Outcome::Success => Ok(vec![
                quota(id, QuotaWindow::FiveHour, 12),
                quota(id, QuotaWindow::SevenDay, 34),
            ]),
            Outcome::Unauthorized => {
                Err(Error::QuotaFetch("usage returned 401 Unauthorized".into()))
            }
        }
    }
}

fn quota(id: &AccountId, window: QuotaWindow, used: u64) -> Quota {
    Quota {
        provider: "fake".into(),
        account_id: id.clone(),
        window,
        used,
        limit: 100,
        reset_at: Some(chrono::Utc::now() + chrono::Duration::days(1)),
        status: QuotaStatus::Ok,
        note: None,
    }
}

fn settings(min_refresh_ms: u64, timeout_ms: u64, retries: u32) -> Settings {
    let mut settings = Settings::default();
    settings.quota.min_refresh_interval_ms = min_refresh_ms;
    settings.quota.fetch_timeout_ms = timeout_ms;
    settings.quota.fetch_retries = retries;
    settings.quota.fetch_retry_delay_ms = 0;
    settings.quota.failure_backoff_max_ms = 60_000;
    settings.quota.max_concurrent_fetches = 4;
    settings.quota.max_concurrent_per_provider = 1;
    settings
}

fn cache_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("quota_cache.json")
}

fn assert_same_reading(left: &QuotaReading, right: &QuotaReading) {
    assert_eq!(left.observed_at, right.observed_at);
    assert_eq!(left.quotas.len(), right.quotas.len());
    for (left, right) in left.quotas.iter().zip(&right.quotas) {
        assert_eq!(left.window, right.window);
        assert_eq!(left.used, right.used);
        assert_eq!(left.limit, right.limit);
    }
}

#[tokio::test]
async fn success_persists_quota_and_original_observation_then_serves_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_path(&dir);
    let id = AccountId("alice".into());
    let provider = FakeProvider::success(0);
    let config = settings(90_000, 2_000, 0);

    let first = collect_quota_at(&provider, &id, &cache, &config)
        .await
        .unwrap();
    assert_eq!(provider.calls(), 1);
    assert_eq!(first.quotas.len(), 2);
    let now = chrono::Utc::now();
    assert!(first.observed_at <= now);
    assert!(first.observed_at > now - chrono::Duration::seconds(10));

    let disk = QuotaCache::load(&cache);
    let entry = disk.get("fake", "alice").expect("cache entry");
    assert_eq!(entry.cached_at, first.observed_at);
    assert_eq!(entry.quotas.len(), 2);
    assert_eq!(entry.quotas[0].window, QuotaWindow::FiveHour);
    assert_eq!(entry.quotas[1].window, QuotaWindow::SevenDay);

    let status = read_status(&cache, "fake", "alice").unwrap().unwrap();
    assert_eq!(status.state, CollectionState::Success);
    assert_eq!(status.observed_at, Some(first.observed_at));

    let second = collect_quota_at(&provider, &id, &cache, &config)
        .await
        .unwrap();
    assert_eq!(provider.calls(), 1, "fresh cache must skip the provider");
    assert_same_reading(&first, &second);
}

#[tokio::test]
async fn fresh_cache_preserves_the_original_observation_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_path(&dir);
    let id = AccountId("parked".into());
    let provider = FakeProvider::success(0);
    let observed_at = chrono::Utc::now() - chrono::Duration::minutes(5);

    let mut seeded = QuotaCache::default();
    seeded.set_at(
        "fake",
        "parked",
        vec![quota(&id, QuotaWindow::SevenDay, 40)],
        observed_at,
    );
    seeded.save(&cache);

    // 一小时内的缓存足够新：协调器必须复用原值，不得重新计时。
    let reading = collect_quota_at(&provider, &id, &cache, &settings(3_600_000, 2_000, 0))
        .await
        .unwrap();
    assert_eq!(provider.calls(), 0);
    assert_eq!(reading.observed_at, observed_at);
    assert_eq!(reading.quotas.len(), 1);
    assert_eq!(reading.quotas[0].used, 40);
}

#[tokio::test]
async fn concurrent_collectors_coalesce_into_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_path(&dir);
    let id = AccountId("shared".into());
    let provider = FakeProvider::success(100);
    let config = settings(90_000, 2_000, 0);

    let (first, second) = tokio::join!(
        collect_quota_at(&provider, &id, &cache, &config),
        collect_quota_at(&provider, &id, &cache, &config),
    );
    let first = first.unwrap();
    let second = second.unwrap();

    assert_eq!(
        provider.calls(),
        1,
        "concurrent collectors must coalesce into one upstream request"
    );
    assert_same_reading(&first, &second);
    assert_eq!(first.quotas.len(), 2);
}

#[tokio::test]
async fn cancellation_keeps_the_minimum_interval_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_path(&dir);
    let id = AccountId("cancelled".into());
    // 查询本身足够慢，确保超时发生在真实请求开始之后。
    let provider = FakeProvider::success(5_000);
    let config = settings(60_000, 10_000, 0);

    let cancelled = tokio::time::timeout(
        Duration::from_millis(200),
        collect_quota_at(&provider, &id, &cache, &config),
    )
    .await;
    assert!(cancelled.is_err(), "query should still be running");
    assert_eq!(provider.calls(), 1);

    // 取消/进程退出不能抹掉已经落盘的请求下限：下一轮不得立刻再打上游。
    let error = collect_quota_at(&provider, &id, &cache, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cooling down") || error.contains("retry later"),
        "unexpected error: {error}"
    );
    assert_eq!(provider.calls(), 1, "cancelled attempt must not be retried");

    let status = read_status(&cache, "fake", "cancelled").unwrap().unwrap();
    assert_eq!(status.state, CollectionState::Pending);
    assert!(status.next_attempt_at.is_some());
    assert!(QuotaCache::load(&cache).get("fake", "cancelled").is_none());
}

#[tokio::test]
async fn deadline_failure_is_persisted_with_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_path(&dir);
    let id = AccountId("slow".into());
    let provider = FakeProvider::success(500);
    let config = settings(60_000, 20, 0);

    let error = collect_quota_at(&provider, &id, &cache, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("timeout"), "unexpected error: {error}");
    assert_eq!(provider.calls(), 1);

    let status = read_status(&cache, "fake", "slow").unwrap().unwrap();
    assert_eq!(status.state, CollectionState::Failure);
    assert_eq!(status.failures, 1);
    assert_eq!(status.observed_at, None);
    assert!(status.next_attempt_at.unwrap() > chrono::Utc::now());
    assert!(status
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("timeout"));

    let disk = QuotaCache::load(&cache);
    let failure = disk.failure("fake", "slow").expect("failure backoff");
    assert!(failure.error.contains("timeout"));

    // 退避期内再次调用只读失败记录，不再打上游。
    let retry = collect_quota_at(&provider, &id, &cache, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(retry.contains("timeout"), "unexpected retry error: {retry}");
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn authentication_failure_is_recorded_without_retry() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_path(&dir);
    let id = AccountId("expired".into());
    let provider = FakeProvider::unauthorized(0);
    let config = settings(60_000, 2_000, 2);

    let error = collect_quota_at(&provider, &id, &cache, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("401"), "unexpected error: {error}");
    assert_eq!(provider.calls(), 1, "401 must not be retried");

    let status = read_status(&cache, "fake", "expired").unwrap().unwrap();
    assert_eq!(status.state, CollectionState::Failure);
    assert!(status.error.as_deref().unwrap_or_default().contains("401"));

    let disk = QuotaCache::load(&cache);
    assert!(disk.failure("fake", "expired").is_some());
    assert!(disk.get("fake", "expired").is_none());
}
