//! 跨进程 quota 采集协调器。
//!
//! 这个模块把「是否该发请求、谁去发、结果何时算观测」从各个调用方收口到一处：
//!
//! - 请求前先在 `<cache_dir>/quota_collection/` 落盘预约（`next_attempt_at`），
//!   取消、超时或进程退出都不会立即再打一次上游。
//! - 同一 `(provider, account)` 的并发采集者共享同一把 fs2 文件锁；后来者等待有界时间，
//!   直接复用先到者的结果，并保留原始 `observed_at`。
//! - 全局与 Provider 维度的文件槽位限制跨进程并发请求数。
//! - 协调记录损坏/不完整时 fail closed：不发请求，也不覆盖记录。
//! - 真实请求仍走 [`crate::quota_query`] 的保守重试与 401/403/429 不重试规则。
//!
//! 缓存写入由 [`QuotaCache`] 自己加锁并原子合并；本模块只负责在拿到真实结果后调用一次，
//! 缓存写失败不会抹掉已经落盘的请求预约。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{AccountId, Quota};
use crate::paths::AppPaths;
use crate::provider::Provider;
use crate::quota_cache::{is_authentication_failure, QuotaCache};
use crate::quota_query::raw_query_quota_with_retry;
use crate::request_metrics;
use crate::settings::{self, Settings};

/// 一次 quota 测量的返回：额度值与**原始观测时间**。
///
/// 调用方必须同时保留两者；不能把复用缓存/合并等待得到的结果重新盖上当前时间戳，
/// 否则旧数据会被误当成实时额度。
#[derive(Debug, Clone)]
pub struct QuotaReading {
    pub quotas: Vec<Quota>,
    pub observed_at: DateTime<Utc>,
}

/// 跨进程协调记录的公开只读状态（不含账号/Provider 原始标识）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CollectionState {
    /// 已预约，正在排队或请求中（正常进程内）；进程退出后会保留到退避下限。
    #[default]
    Pending,
    /// 已有一次成功观测。
    Success,
    /// 请求失败，等待 `next_attempt_at`。
    Failure,
    /// 因并发槽位不足主动推迟，没有发请求。
    Deferred,
    /// 账号已被 `rm` 显式移除：旧成功结果作废，但请求下限仍保留。
    Removed,
}

/// 调度器/状态面可读取的持久采集状态。
#[derive(Debug, Clone)]
pub struct CollectionStatus {
    pub state: CollectionState,
    pub observed_at: Option<DateTime<Utc>>,
    /// 下一次允许真实请求的时间；失败、429 或进程崩溃后也会保留。
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub failures: u32,
    pub error: Option<String>,
    pub quotas: Vec<Quota>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Record {
    #[serde(default)]
    state: CollectionState,
    #[serde(default)]
    observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    next_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    quotas: Vec<Quota>,
    #[serde(default)]
    error: Option<String>,
}

/// 账号锁/记录文件序号，避免同一进程内多个异步任务临时文件重名。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
/// 槽位轮转起点，避免所有进程总从 0 号槽开始制造无谓争抢。
static SLOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// 协调目录名（位于 quota cache 同级的 cache 目录内）。
const COLLECTION_DIR: &str = "quota_collection";

/// 按当前配置查询 quota，并持久化采集结果与退避。
///
/// 这是 CLI/daemon 共用的生产入口；缓存命中、并发合并、退避与失败记录都由本函数负责。
pub async fn collect_quota(provider: &dyn Provider, id: &AccountId) -> Result<QuotaReading> {
    let cfg = settings::current();
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    collect_impl(provider, id, &cache_path, cfg.as_ref(), true).await
}

/// 显式缓存路径与配置的等价入口，供隔离测试/嵌入调用使用。
///
/// 不写全局请求指标；生产路径请用 [`collect_quota`]。
pub async fn collect_quota_at(
    provider: &dyn Provider,
    id: &AccountId,
    cache_path: &Path,
    cfg: &Settings,
) -> Result<QuotaReading> {
    collect_impl(provider, id, cache_path, cfg, false).await
}

/// 读取某账号的持久采集状态；文件缺失返回 `None`，损坏/不完整返回 `Err`。
///
/// 只读，不创建目录也不发请求；调度器可用它读取 `next_attempt_at` 等截止时间。
pub fn read_status(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
) -> Result<Option<CollectionStatus>> {
    let dir = coordination_dir(cache_path);
    let stem = account_stem(provider, account_id);
    let path = record_path(&dir, &stem);
    match read_record(&path)? {
        Some(record) => Ok(Some(CollectionStatus {
            state: record.state,
            observed_at: record.observed_at,
            next_attempt_at: record.next_attempt_at,
            failures: record.failures,
            error: record.error,
            quotas: record.quotas,
        })),
        None => Ok(None),
    }
}

/// 只读取某账号下一次允许采集的时间；供调度器排序/去重。
///
/// 这是同步磁盘读取；async 调用方应使用 [`next_attempt_at_async`]，或自行放进
/// `spawn_blocking`。
pub fn next_attempt_at(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
) -> Result<Option<DateTime<Utc>>> {
    Ok(read_status(cache_path, provider, account_id)?.and_then(|status| status.next_attempt_at))
}

/// [`read_status`] 的 async 包装：磁盘读取在 `spawn_blocking` 内完成。
pub async fn read_status_async(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
) -> Result<Option<CollectionStatus>> {
    let path = cache_path.to_path_buf();
    let provider = provider.to_string();
    let account_id = account_id.to_string();
    tokio::task::spawn_blocking(move || read_status(&path, &provider, &account_id))
        .await
        .map_err(|error| {
            Error::QuotaFetch(format!("quota collection state read task failed: {error}"))
        })?
}

/// [`next_attempt_at`] 的 async 包装：磁盘读取在 `spawn_blocking` 内完成。
pub async fn next_attempt_at_async(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
) -> Result<Option<DateTime<Utc>>> {
    Ok(read_status_async(cache_path, provider, account_id)
        .await?
        .and_then(|status| status.next_attempt_at))
}

/// 账号被显式 `rm` 后的协调状态失效。
///
/// 会等待该账号正在进行的采集结束（账号锁），随后：
/// - 保留未来的 `next_attempt_at` 请求下限（含服务端 Retry-After），不盲目删除记录/锁；
/// - 把记录改为 [`CollectionState::Removed`] 并清空旧成功结果；
/// - 删除 `QuotaCache` 中该账号的成功/失败记录，写入移除墓碑。
///
/// 全程不发送网络请求。`rm` 应 best-effort 调用：失败只警告，不能因此拒绝删除账号。
/// 这是阻塞文件操作，实际锁与 IO 在 `spawn_blocking` 内完成。
pub async fn invalidate_collection_state(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
) -> Result<()> {
    let path = cache_path.to_path_buf();
    let provider = provider.to_string();
    let account_id = account_id.to_string();
    tokio::task::spawn_blocking(move || {
        invalidate_collection_state_sync(&path, &provider, &account_id)
    })
    .await
    .map_err(|error| {
        Error::QuotaFetch(format!(
            "quota collection invalidation task failed: {error}"
        ))
    })?
}

fn invalidate_collection_state_sync(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
) -> Result<()> {
    let dir = coordination_dir(cache_path);
    fs::create_dir_all(&dir)?;
    let stem = account_stem(provider, account_id);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(format!("{stem}.lock")))?;
    // 阻塞等待在途采集结束：rm 是显式用户动作，可以等待；这样旧请求不会在失效之后
    // 再写回成功结果或缓存。绝不因此拒绝删除账号。
    FileExt::lock_exclusive(&lock_file)?;

    let now = Utc::now();
    let record_path = record_path(&dir, &stem);
    let record = match read_record(&record_path) {
        Ok(Some(record)) => record,
        Ok(None) => Record::default(),
        Err(error) => {
            // 显式删除是修复损坏协调状态的机会；否则同 ID 重新导入会永久 fail closed。
            tracing::warn!(
                provider,
                account = account_id,
                err = %error,
                "quota collection state unreadable during removal; resetting"
            );
            Record::default()
        }
    };
    let floor = record
        .next_attempt_at
        .filter(|deadline| *deadline > now)
        .unwrap_or(now);
    let removed = Record {
        state: CollectionState::Removed,
        observed_at: None,
        next_attempt_at: Some(floor),
        failures: 0,
        quotas: Vec::new(),
        error: Some("account removed; waiting for a fresh import".into()),
    };
    let record_result = write_record(&record_path, &removed);

    // 仍在账号锁内：本账号没有在途采集能在此之后写回旧结果。
    let mut cache = QuotaCache::load(cache_path);
    cache.remove(provider, account_id);
    cache.save(cache_path);

    let _ = FileExt::unlock(&lock_file);
    record_result
}

async fn collect_impl(
    provider: &dyn Provider,
    id: &AccountId,
    cache_path: &Path,
    cfg: &Settings,
    emit_metrics: bool,
) -> Result<QuotaReading> {
    let provider_id = provider.id();
    let account_id = id.0.as_str();
    let min_refresh = StdDuration::from_millis(cfg.quota.min_refresh_interval_ms);
    let backoff_cap = StdDuration::from_millis(cfg.quota.failure_backoff_max_ms);

    let cache = load_cache(cache_path).await;
    if let Some(entry) = cache.fresh(provider_id, account_id, min_refresh) {
        emit(emit_metrics, provider_id, account_id, "cache_hit", None).await;
        return Ok(QuotaReading {
            quotas: entry.quotas,
            observed_at: entry.cached_at,
        });
    }
    if let Some(failure) =
        cache.in_failure_backoff(provider_id, account_id, min_refresh, backoff_cap)
    {
        // 失败退避只读缓存；协调记录里的截止时间用于并发等待者。
        emit(emit_metrics, provider_id, account_id, "deferred", None).await;
        return Err(Error::QuotaFetch(failure.error.clone()));
    }

    let dir = coordination_dir(cache_path);
    let stem = account_stem(provider_id, account_id);
    let wait_budget = wait_budget(cfg);

    // 两次机会：先尝试直接拿账号锁；被占用则等待先到者，若等待期间对方退出（锁释放）
    // 再重试一次接管。
    for attempt in 0..2 {
        if let Some(lock) = try_account_lock(dir.clone(), stem.clone()).await? {
            return fetch_with_lock(lock, provider, id, cache_path, cfg, emit_metrics).await;
        }
        if attempt == 1 {
            break;
        }
        match wait_for_coalesced(&CoalesceWait {
            dir: &dir,
            stem: &stem,
            cache_path,
            provider_id,
            account_id,
            emit_metrics,
            wait_budget,
            min_refresh,
        })
        .await
        {
            Coalesced::Reading(reading) => return Ok(reading),
            Coalesced::Failed(error) => return Err(error),
            Coalesced::Retry => continue,
        }
    }
    Err(Error::QuotaFetch(
        "quota collection already in progress; retry later".into(),
    ))
}

enum Coalesced {
    Reading(QuotaReading),
    Failed(Error),
    Retry,
}

/// 等待另一个正在持有账号锁的采集者所需上下文。
struct CoalesceWait<'a> {
    dir: &'a Path,
    stem: &'a str,
    cache_path: &'a Path,
    provider_id: &'a str,
    account_id: &'a str,
    emit_metrics: bool,
    wait_budget: StdDuration,
    min_refresh: StdDuration,
}

/// 等待另一个正在持有账号锁的采集者；有界等待后返回其成功值或失败，不自己发请求。
async fn wait_for_coalesced(wait: &CoalesceWait<'_>) -> Coalesced {
    let CoalesceWait {
        dir,
        stem,
        cache_path,
        provider_id,
        account_id,
        emit_metrics,
        wait_budget,
        min_refresh,
    } = *wait;
    let deadline = deadline_after(Utc::now(), wait_budget);
    loop {
        match read_record_async(&record_path(dir, stem)).await {
            Ok(Some(record)) => match record.state {
                CollectionState::Success => {
                    let Some(observed_at) = record.observed_at else {
                        return Coalesced::Failed(Error::QuotaFetch(
                            "quota collection state invalid".into(),
                        ));
                    };
                    // 只合并先到者这次的新结果。若记录是更早的旧测量（下限已过），
                    // 继续等它刷新，避免把旧数字当成并发结果冒充实时。
                    let fresh = min_refresh.is_zero()
                        || record
                            .next_attempt_at
                            .is_some_and(|deadline| deadline > Utc::now());
                    if fresh {
                        let quotas = crate::quota_cache::current_quotas(
                            &record.quotas,
                            observed_at,
                            Utc::now(),
                        );
                        if quotas.is_empty() {
                            emit(emit_metrics, provider_id, account_id, "deferred", None).await;
                            return Coalesced::Failed(Error::QuotaFetch(
                                "quota reading expired after reset; waiting for the next refresh"
                                    .into(),
                            ));
                        }
                        emit(emit_metrics, provider_id, account_id, "coalesced", None).await;
                        return Coalesced::Reading(QuotaReading {
                            quotas,
                            observed_at,
                        });
                    }
                }
                CollectionState::Failure | CollectionState::Deferred => {
                    // 退避未过：这是先到者的最终结果，直接复用错误。
                    // 退避已过：锁的持有者很可能正在发起新一轮，继续等它的新结果。
                    if record
                        .next_attempt_at
                        .is_some_and(|deadline| deadline > Utc::now())
                    {
                        emit(emit_metrics, provider_id, account_id, "coalesced", None).await;
                        return Coalesced::Failed(Error::QuotaFetch(
                            record
                                .error
                                .clone()
                                .unwrap_or_else(|| "quota collection failed".into()),
                        ));
                    }
                }
                CollectionState::Removed => {
                    // 下限内：明确告知账号已移除且仍在冷却；下限已过则等待重新导入后的
                    // 新采集结果，不能把“已移除”当成永久失败。
                    if record
                        .next_attempt_at
                        .is_some_and(|deadline| deadline > Utc::now())
                    {
                        emit(emit_metrics, provider_id, account_id, "deferred", None).await;
                        return Coalesced::Failed(Error::QuotaFetch(
                            "account was removed; waiting for a fresh import".into(),
                        ));
                    }
                }
                CollectionState::Pending => {}
            },
            Ok(None) => {}
            // 协调记录不可读时 fail closed，不能猜。
            Err(error) => return Coalesced::Failed(error),
        }

        // 先到者可能已退出并释放锁；尝试接管。失败（仍被占用）继续等。
        match try_account_lock(dir.to_path_buf(), stem.to_string()).await {
            Ok(Some(_guard)) => return Coalesced::Retry,
            Ok(None) => {}
            Err(error) => return Coalesced::Failed(error),
        }

        let now = Utc::now();
        if now >= deadline {
            // 先到者可能已经写完缓存但还没来得及写协调记录。
            if let Some(entry) =
                load_cache(cache_path)
                    .await
                    .fresh(provider_id, account_id, min_refresh)
            {
                emit(emit_metrics, provider_id, account_id, "coalesced", None).await;
                return Coalesced::Reading(QuotaReading {
                    quotas: entry.quotas,
                    observed_at: entry.cached_at,
                });
            }
            emit(emit_metrics, provider_id, account_id, "deferred", None).await;
            return Coalesced::Failed(Error::QuotaFetch(
                "quota collection already in progress; retry later".into(),
            ));
        }

        let remaining = (deadline - now)
            .to_std()
            .unwrap_or(StdDuration::from_millis(1))
            .min(StdDuration::from_millis(50));
        tokio::time::sleep(remaining).await;
    }
}

/// 已持有账号锁：检查持久状态、预约、排队槽位、发真实请求、写回结果。
async fn fetch_with_lock(
    lock: AccountLock,
    provider: &dyn Provider,
    id: &AccountId,
    cache_path: &Path,
    cfg: &Settings,
    emit_metrics: bool,
) -> Result<QuotaReading> {
    let provider_id = provider.id();
    let account_id = id.0.as_str();
    let record_path = lock.record_path.clone();
    let min_refresh_ms = cfg.quota.min_refresh_interval_ms;
    let now = Utc::now();

    let mut record = read_record_async(&record_path).await?.unwrap_or_default();

    if let Some(deadline) = record.next_attempt_at {
        if deadline > now {
            match record.state {
                CollectionState::Success => {
                    let Some(observed_at) = record.observed_at else {
                        return Err(Error::QuotaFetch("quota collection state invalid".into()));
                    };
                    let quotas =
                        crate::quota_cache::current_quotas(&record.quotas, observed_at, Utc::now());
                    if quotas.is_empty() {
                        emit(emit_metrics, provider_id, account_id, "deferred", None).await;
                        return Err(Error::QuotaFetch(
                            "quota reading expired after reset; waiting for the next refresh"
                                .into(),
                        ));
                    }
                    emit(emit_metrics, provider_id, account_id, "cache_hit", None).await;
                    return Ok(QuotaReading {
                        quotas,
                        observed_at,
                    });
                }
                CollectionState::Failure | CollectionState::Deferred => {
                    emit(emit_metrics, provider_id, account_id, "deferred", None).await;
                    return Err(Error::QuotaFetch(
                        record
                            .error
                            .clone()
                            .unwrap_or_else(|| "quota collection cooling down".into()),
                    ));
                }
                CollectionState::Pending => {
                    // 锁已在本进程手里，说明原持有者已退出/取消；仍须尊重它落盘的请求下限。
                    emit(emit_metrics, provider_id, account_id, "deferred", None).await;
                    return Err(Error::QuotaFetch(
                        "quota collection is cooling down after an interrupted attempt".into(),
                    ));
                }
                CollectionState::Removed => {
                    // rm 保留了下限；下限内不请求，也不返回已作废的旧成功结果。
                    emit(emit_metrics, provider_id, account_id, "deferred", None).await;
                    return Err(Error::QuotaFetch(
                        "account was removed; waiting for a fresh import".into(),
                    ));
                }
            }
        }
    }

    // 先落盘预约，再排队/发请求：取消、超时、进程退出都不能立即再打一次。
    record.state = CollectionState::Pending;
    record.next_attempt_at = Some(deadline_after(now, millis(min_refresh_ms)));
    record.observed_at = None;
    record.quotas.clear();
    record.error = Some("quota collection in progress".into());
    write_record_async(&record_path, &record).await?;

    let slot_deadline = deadline_after(Utc::now(), slot_wait_budget(cfg));
    let slot = acquire_slots(
        coordination_dir(cache_path),
        cfg,
        provider_id,
        slot_deadline,
    )
    .await?;
    let Some(slot) = slot else {
        // 没有发请求；保留预约下限，避免槽位争抢变成请求风暴。
        record.state = CollectionState::Deferred;
        record.error = Some("quota collection deferred: concurrency limit reached".into());
        let _ = write_record_async(&record_path, &record).await;
        emit(emit_metrics, provider_id, account_id, "deferred", None).await;
        return Err(Error::QuotaFetch(
            "quota collection deferred: concurrency limit reached".into(),
        ));
    };

    emit(
        emit_metrics,
        provider_id,
        account_id,
        "collection_start",
        None,
    )
    .await;
    let result = raw_query_quota_with_retry(provider, id, &cfg.quota).await;
    drop(slot);

    match result {
        Ok(quotas) => {
            let observed_at = Utc::now();
            record.state = CollectionState::Success;
            record.observed_at = Some(observed_at);
            record.next_attempt_at = Some(deadline_after(observed_at, millis(min_refresh_ms)));
            record.failures = 0;
            record.quotas = quotas.clone();
            record.error = None;
            if let Err(error) = write_record_async(&record_path, &record).await {
                // 预约/结果记录写失败不回滚已拿到的真实数据；缓存仍会承担节流。
                tracing::warn!(
                    provider = provider_id,
                    account = account_id,
                    err = %error,
                    "quota collection success state write failed"
                );
            }
            persist_success(
                cache_path,
                provider_id,
                account_id,
                quotas.clone(),
                observed_at,
            )
            .await;
            emit(
                emit_metrics,
                provider_id,
                account_id,
                "collection_success",
                None,
            )
            .await;
            Ok(QuotaReading {
                quotas,
                observed_at,
            })
        }
        Err(error) => {
            let now = Utc::now();
            let failures = record.failures.saturating_add(1);
            let text = error.to_string();
            let auth_failure = is_authentication_failure(&text);
            let mut status = None;
            let mut retry_deadline = None;
            if let Error::QuotaRateLimited { retry_at, .. } = &error {
                status = Some(429);
                retry_deadline = *retry_at;
            }
            let delay_ms = failure_delay_ms(cfg, failures, auth_failure);
            let mut next_attempt_at = deadline_after(now, millis(delay_ms));
            if let Some(server_deadline) = retry_deadline {
                // 服务端 Retry-After 更晚时压过通用上限，且不会被下一次失败缩短。
                next_attempt_at = next_attempt_at.max(server_deadline);
            }

            record.state = CollectionState::Failure;
            record.observed_at = None;
            record.next_attempt_at = Some(next_attempt_at);
            record.failures = failures;
            record.quotas.clear();
            record.error = Some(text.clone());
            if let Err(write_error) = write_record_async(&record_path, &record).await {
                tracing::warn!(
                    provider = provider_id,
                    account = account_id,
                    err = %write_error,
                    "quota collection failure state write failed"
                );
            }
            persist_failure(cache_path, provider_id, account_id, &text).await;
            emit(
                emit_metrics,
                provider_id,
                account_id,
                "collection_failure",
                status,
            )
            .await;
            Err(error)
        }
    }
}

/// 真实请求成功后的缓存写入；失败只 warn，缓存本身可丢弃。
async fn persist_success(
    cache_path: &Path,
    provider: &str,
    account_id: &str,
    quotas: Vec<Quota>,
    observed_at: DateTime<Utc>,
) {
    let path = cache_path.to_path_buf();
    let provider = provider.to_string();
    let account_id = account_id.to_string();
    let joined = tokio::task::spawn_blocking(move || {
        let mut cache = QuotaCache::load(&path);
        cache.set_at(&provider, &account_id, quotas, observed_at);
        cache.save(&path);
    })
    .await;
    if let Err(error) = joined {
        tracing::warn!(err = %error, "quota cache success persist task failed");
    }
}

/// 真实请求失败后的缓存失败退避。
async fn persist_failure(cache_path: &Path, provider: &str, account_id: &str, error: &str) {
    let path = cache_path.to_path_buf();
    let provider = provider.to_string();
    let account_id = account_id.to_string();
    let error = error.to_string();
    let joined = tokio::task::spawn_blocking(move || {
        let mut cache = QuotaCache::load(&path);
        cache.record_failure(&provider, &account_id, &error);
        cache.save(&path);
    })
    .await;
    if let Err(error) = joined {
        tracing::warn!(err = %error, "quota cache failure persist task failed");
    }
}

/// 账号级文件锁。`File` 一旦 drop 就释放 flock。
struct AccountLock {
    _file: File,
    record_path: PathBuf,
}

struct SlotGuard {
    _files: Vec<File>,
}

/// fs2 官方用 `lock_contended_error()` 表示 try-lock 的“已被占用”错误。
///
/// Windows 原生错误是 `ERROR_LOCK_VIOLATION`，其 `ErrorKind` 不一定映射成
/// `WouldBlock`；因此同时比较 raw OS error 与 kind，不靠猜测。
fn is_lock_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.raw_os_error() == contended.raw_os_error() || error.kind() == contended.kind()
}

async fn try_account_lock(dir: PathBuf, stem: String) -> Result<Option<AccountLock>> {
    tokio::task::spawn_blocking(move || try_account_lock_sync(&dir, &stem))
        .await
        .map_err(|error| Error::QuotaFetch(format!("quota collection lock task failed: {error}")))?
}

fn try_account_lock_sync(dir: &Path, stem: &str) -> Result<Option<AccountLock>> {
    fs::create_dir_all(dir)?;
    let lock_path = dir.join(format!("{stem}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(AccountLock {
            _file: file,
            record_path: record_path(dir, stem),
        })),
        Err(error) if is_lock_contended(&error) => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

/// 取全局槽位后再取 Provider 槽位；取不到 Provider 槽时立即归还全局槽，避免饿死其它 Provider。
fn try_acquire_slots(
    dir: &Path,
    global_slots: usize,
    provider_slots: usize,
    provider: &str,
) -> std::io::Result<Option<SlotGuard>> {
    fs::create_dir_all(dir)?;
    let Some(global) = try_lock_any(dir, "global", global_slots)? else {
        return Ok(None);
    };
    let provider_prefix = format!("provider-{}", sanitize_component(provider, 32));
    match try_lock_any(dir, &provider_prefix, provider_slots)? {
        Some(provider_slot) => Ok(Some(SlotGuard {
            _files: vec![global, provider_slot],
        })),
        None => {
            // 归还全局槽（drop 即解锁），让其它 Provider 继续。
            drop(global);
            Ok(None)
        }
    }
}

fn try_lock_any(dir: &Path, prefix: &str, count: usize) -> std::io::Result<Option<File>> {
    let start = SLOT_SEQ.fetch_add(1, Ordering::Relaxed) as usize;
    for offset in 0..count {
        let index = (start + offset) % count;
        let path = dir.join(format!("{prefix}-{index}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(Some(file)),
            Err(error) if is_lock_contended(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

async fn acquire_slots(
    dir: PathBuf,
    cfg: &Settings,
    provider: &str,
    deadline: DateTime<Utc>,
) -> Result<Option<SlotGuard>> {
    let global_slots = cfg.quota.max_concurrent_fetches.clamp(1, 64);
    let provider_slots = cfg.quota.max_concurrent_per_provider.clamp(1, 32);
    let provider = provider.to_string();
    loop {
        let dir_for_task = dir.clone();
        let provider_for_task = provider.clone();
        let attempt = tokio::task::spawn_blocking(move || {
            try_acquire_slots(
                &dir_for_task,
                global_slots,
                provider_slots,
                &provider_for_task,
            )
        })
        .await
        .map_err(|error| {
            Error::QuotaFetch(format!("quota collection slot task failed: {error}"))
        })?;
        if let Some(slot) = attempt? {
            return Ok(Some(slot));
        }
        let now = Utc::now();
        if now >= deadline {
            return Ok(None);
        }
        let remaining = (deadline - now)
            .to_std()
            .unwrap_or(StdDuration::from_millis(1))
            .min(StdDuration::from_millis(50));
        tokio::time::sleep(remaining).await;
    }
}

fn coordination_dir(cache_path: &Path) -> PathBuf {
    cache_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(COLLECTION_DIR)
}

fn record_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.json"))
}

fn account_stem(provider: &str, account_id: &str) -> String {
    let prefix = sanitize_component(provider, 24);
    let hash = fnv1a(&[provider.as_bytes(), b"\0", account_id.as_bytes()]);
    format!("{prefix}-{hash:016x}")
}

fn sanitize_component(value: &str, limit: usize) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(limit)
        .collect();
    if sanitized.is_empty() {
        "p".into()
    } else {
        sanitized
    }
}

/// 稳定、无依赖的 FNV-1a 64 位哈希；只用于把账号标识变成不可逆文件名。
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn validate_record(record: &Record) -> Result<()> {
    let valid = match record.state {
        CollectionState::Pending => record.next_attempt_at.is_some(),
        CollectionState::Success => {
            record.observed_at.is_some() && record.next_attempt_at.is_some()
        }
        CollectionState::Failure | CollectionState::Deferred => {
            record.next_attempt_at.is_some() && record.error.is_some()
        }
        CollectionState::Removed => record.next_attempt_at.is_some(),
    };
    if valid {
        Ok(())
    } else {
        Err(Error::QuotaFetch("quota collection state invalid".into()))
    }
}

fn read_record(path: &Path) -> Result<Option<Record>> {
    match fs::read(path) {
        Ok(bytes) => {
            let record: Record = serde_json::from_slice(&bytes).map_err(|error| {
                tracing::debug!(err = %error, path = %path.display(), "quota collection state unreadable");
                Error::QuotaFetch("quota collection state unreadable".into())
            })?;
            validate_record(&record)?;
            Ok(Some(record))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

async fn read_record_async(path: &Path) -> Result<Option<Record>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_record(&path))
        .await
        .map_err(|error| {
            Error::QuotaFetch(format!("quota collection state read task failed: {error}"))
        })?
}

fn write_record(path: &Path, record: &Record) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(record)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "record.json".into());
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), seq));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(())
}

async fn write_record_async(path: &Path, record: &Record) -> Result<()> {
    let path = path.to_path_buf();
    let record = record.clone();
    tokio::task::spawn_blocking(move || write_record(&path, &record))
        .await
        .map_err(|error| {
            Error::QuotaFetch(format!("quota collection state write task failed: {error}"))
        })?
}

async fn load_cache(path: &Path) -> QuotaCache {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || QuotaCache::load(&path))
        .await
        .unwrap_or_default()
}

async fn emit(enabled: bool, provider: &str, account_id: &str, event: &str, status: Option<u16>) {
    if enabled {
        request_metrics::record(provider, account_id, event, status).await;
    }
}

/// 合并等待先到者的上限：一次普通请求超时的时间。
fn wait_budget(cfg: &Settings) -> StdDuration {
    StdDuration::from_millis(cfg.quota.fetch_timeout_ms.max(250))
}

/// 等并发槽位的上限。必须短于真实请求超时，否则就算排到槽位，请求本身也已超时。
fn slot_wait_budget(cfg: &Settings) -> StdDuration {
    StdDuration::from_millis(
        cfg.quota
            .fetch_timeout_ms
            .saturating_mul(3)
            .checked_div(4)
            .unwrap_or(0)
            .max(250),
    )
}

fn millis(value: u64) -> StdDuration {
    StdDuration::from_millis(value)
}

fn deadline_after(now: DateTime<Utc>, duration: StdDuration) -> DateTime<Utc> {
    let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
    now.checked_add_signed(Duration::milliseconds(millis))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

fn failure_delay_ms(cfg: &Settings, failures: u32, auth_failure: bool) -> u64 {
    let base = cfg.quota.min_refresh_interval_ms.max(1);
    if auth_failure {
        // 401/403 通常在重新登录后立刻恢复；保留基础窗口避免风暴，但不要指数退避。
        return base;
    }
    let cap = cfg.quota.failure_backoff_max_ms.max(base);
    let shift = failures.saturating_sub(1).min(16);
    base.saturating_mul(1_u64 << shift).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use crate::model::{Account, ClientTarget, QuotaStatus, QuotaWindow};

    fn test_settings() -> Settings {
        let mut cfg = Settings::default();
        cfg.quota.min_refresh_interval_ms = 90_000;
        cfg.quota.fetch_timeout_ms = 2_000;
        cfg.quota.fetch_retries = 0;
        cfg.quota.fetch_retry_delay_ms = 0;
        cfg.quota.failure_backoff_max_ms = 900_000;
        cfg.quota.max_concurrent_fetches = 4;
        cfg.quota.max_concurrent_per_provider = 2;
        cfg
    }

    fn quota(used: u64) -> Quota {
        Quota {
            provider: "mock".into(),
            account_id: AccountId("a".into()),
            window: QuotaWindow::SevenDay,
            used,
            limit: 100,
            reset_at: Some(Utc::now() + Duration::days(3)),
            status: QuotaStatus::Ok,
            note: None,
        }
    }

    #[derive(Clone)]
    enum Behavior {
        Success(Vec<Quota>),
        Fail(&'static str),
        RateLimited(DateTime<Utc>),
        Block {
            started: Arc<Notify>,
            release: Arc<Notify>,
            quotas: Vec<Quota>,
        },
    }

    struct MockProvider {
        id: &'static str,
        calls: Arc<AtomicUsize>,
        behavior: Arc<Mutex<Behavior>>,
    }

    impl MockProvider {
        fn new(id: &'static str, behavior: Behavior) -> Self {
            Self {
                id,
                calls: Arc::new(AtomicUsize::new(0)),
                behavior: Arc::new(Mutex::new(behavior)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn set_behavior(&self, behavior: Behavior) {
            *self.behavior.lock().unwrap() = behavior;
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn display_name(&self) -> &'static str {
            "Mock"
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

        async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let behavior = self.behavior.lock().unwrap().clone();
            match behavior {
                Behavior::Success(quotas) => Ok(quotas),
                Behavior::Fail(message) => Err(Error::QuotaFetch(message.into())),
                Behavior::RateLimited(retry_at) => Err(Error::QuotaRateLimited {
                    message: "usage returned 429 Too Many Requests".into(),
                    retry_at: Some(retry_at),
                }),
                Behavior::Block {
                    started,
                    release,
                    quotas,
                } => {
                    started.notify_one();
                    release.notified().await;
                    Ok(quotas)
                }
            }
        }
    }

    #[tokio::test]
    async fn cache_hit_preserves_original_observed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let observed_at = Utc::now() - Duration::seconds(30);
        let mut cache = QuotaCache::load(&cache_path);
        cache.set_at("mock", "a", vec![quota(10)], observed_at);
        cache.save(&cache_path);

        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(99)]));
        let reading = collect_quota_at(
            &provider,
            &AccountId("a".into()),
            &cache_path,
            &test_settings(),
        )
        .await
        .unwrap();

        assert_eq!(provider.calls(), 0);
        assert_eq!(reading.observed_at, observed_at);
        assert_eq!(reading.quotas[0].used, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_collectors_share_one_request_and_observed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider = Arc::new(MockProvider::new(
            "mock",
            Behavior::Block {
                started: started.clone(),
                release: release.clone(),
                quotas: vec![quota(42)],
            },
        ));
        let cfg = test_settings();

        let first_provider = provider.clone();
        let first_path = cache_path.clone();
        let first_cfg = cfg.clone();
        let first = tokio::spawn(async move {
            collect_quota_at(
                &*first_provider,
                &AccountId("a".into()),
                &first_path,
                &first_cfg,
            )
            .await
        });
        started.notified().await;

        let second_provider = provider.clone();
        let second_path = cache_path.clone();
        let second_cfg = cfg.clone();
        let second = tokio::spawn(async move {
            collect_quota_at(
                &*second_provider,
                &AccountId("a".into()),
                &second_path,
                &second_cfg,
            )
            .await
        });
        // 让第二个采集者进入「等锁 → 等先到者结果」的路径。
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        release.notify_one();

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(provider.calls(), 1);
        assert_eq!(first.observed_at, second.observed_at);
        assert_eq!(second.quotas[0].used, 42);
    }

    #[test]
    fn fs2_contended_lock_error_is_classified_portably() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("contended.lock");
        let first = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock_exclusive(&first).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let error = FileExt::try_lock_exclusive(&second).unwrap_err();
        assert!(is_lock_contended(&error), "{error:?}");
        assert!(!is_lock_contended(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not contention"
        )));
    }

    /// 两个独立文件句柄模拟两个进程：fs2 flock 在进程内不同 open file description 之间
    /// 同样互斥，因此这个测试覆盖跨进程锁的核心语义。
    #[test]
    fn parallel_handles_acquire_exactly_one_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("quota_collection");
        let stem = "mock-lock".to_string();
        let barrier = Arc::new(Barrier::new(2));
        let results = Arc::new(Mutex::new(Vec::new()));

        std::thread::scope(|scope| {
            for _ in 0..2 {
                let dir = dir.clone();
                let stem = stem.clone();
                let barrier = barrier.clone();
                let results = results.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let guard = try_account_lock_sync(&dir, &stem).unwrap();
                    results.lock().unwrap().push(guard.is_some());
                    // 持有锁直到两边都尝试过，确保竞争真实存在。
                    barrier.wait();
                    drop(guard);
                });
            }
        });

        let mut observed = results.lock().unwrap().clone();
        observed.sort();
        assert_eq!(observed, vec![false, true]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_collection_keeps_floor_and_allows_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider = Arc::new(MockProvider::new(
            "mock",
            Behavior::Block {
                started: started.clone(),
                release,
                quotas: vec![quota(1)],
            },
        ));
        let mut cfg = test_settings();
        cfg.quota.min_refresh_interval_ms = 600_000;
        cfg.quota.fetch_timeout_ms = 5_000;

        let task_provider = provider.clone();
        let task_path = cache_path.clone();
        let task_cfg = cfg.clone();
        let task = tokio::spawn(async move {
            collect_quota_at(
                &*task_provider,
                &AccountId("a".into()),
                &task_path,
                &task_cfg,
            )
            .await
        });
        started.notified().await;
        task.abort();
        let _ = task.await;
        assert_eq!(provider.calls(), 1);

        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Pending);
        assert!(status.next_attempt_at.unwrap() > Utc::now());

        // 取消后的下限仍然生效：不重新发请求。
        provider.set_behavior(Behavior::Success(vec![quota(2)]));
        let error = collect_quota_at(&*provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("cooling down"), "{error}");
        assert_eq!(provider.calls(), 1);

        // 等价于进程崩溃后下限已过：下一次采集可以接管。
        let record_file = record_path(&coordination_dir(&cache_path), &account_stem("mock", "a"));
        let mut record: Record = serde_json::from_slice(&fs::read(&record_file).unwrap()).unwrap();
        record.next_attempt_at = Some(Utc::now() - Duration::seconds(1));
        write_record(&record_file, &record).unwrap();

        let reading = collect_quota_at(&*provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 2);
        assert_eq!(reading.quotas[0].used, 2);
    }

    #[tokio::test]
    async fn invalid_coordination_state_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let record_file = record_path(&coordination_dir(&cache_path), &account_stem("mock", "a"));
        fs::create_dir_all(record_file.parent().unwrap()).unwrap();
        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(1)]));
        let cfg = test_settings();

        fs::write(&record_file, b"{not json").unwrap();
        let error = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unreadable"), "{error}");
        assert_eq!(provider.calls(), 0);
        assert!(read_status(&cache_path, "mock", "a").is_err());

        fs::write(&record_file, br#"{"state":"success"}"#).unwrap();
        let error = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid"), "{error}");
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn server_retry_deadline_is_persisted_and_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let retry_at = Utc::now() + Duration::hours(2);
        let provider = MockProvider::new("mock", Behavior::RateLimited(retry_at));
        let cfg = test_settings();

        let error = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err();
        match error {
            Error::QuotaRateLimited { retry_at: seen, .. } => {
                assert_eq!(seen, Some(retry_at));
            }
            other => panic!("expected typed rate limit error, got {other}"),
        }
        assert_eq!(provider.calls(), 1);

        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Failure);
        assert_eq!(status.failures, 1);
        assert!(status.next_attempt_at.unwrap() >= retry_at);
        assert!(status.error.unwrap().contains("429"));
        assert!(next_attempt_at(&cache_path, "mock", "a").unwrap().unwrap() >= retry_at);

        // 第二次调用被持久截止时间挡住，不再打 Provider。
        let error = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("429"), "{error}");
        assert_eq!(provider.calls(), 1);
        assert!(QuotaCache::load(&cache_path).failure("mock", "a").is_some());
    }

    #[tokio::test]
    async fn success_persists_observation_and_second_call_uses_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(33)]));
        let cfg = test_settings();

        let first = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 1);

        let cached = QuotaCache::load(&cache_path).get("mock", "a").unwrap();
        assert_eq!(cached.cached_at, first.observed_at);
        assert_eq!(cached.quotas[0].used, 33);

        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Success);
        assert_eq!(status.observed_at, Some(first.observed_at));
        assert_eq!(
            status.next_attempt_at,
            Some(first.observed_at + Duration::milliseconds(90_000))
        );

        let second = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 1);
        assert_eq!(second.observed_at, first.observed_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn global_and_provider_slots_bound_concurrency() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let blocking = Arc::new(MockProvider::new(
            "p1",
            Behavior::Block {
                started: started.clone(),
                release: release.clone(),
                quotas: vec![quota(1)],
            },
        ));
        let mut cfg = test_settings();
        cfg.quota.fetch_timeout_ms = 2_000;
        cfg.quota.max_concurrent_fetches = 2;
        cfg.quota.max_concurrent_per_provider = 1;

        let task_provider = blocking.clone();
        let task_path = cache_path.clone();
        let task_cfg = cfg.clone();
        let first = tokio::spawn(async move {
            collect_quota_at(
                &*task_provider,
                &AccountId("a".into()),
                &task_path,
                &task_cfg,
            )
            .await
        });
        started.notified().await;

        // 同 Provider 第二个账号：Provider 槽位已满，必须在有界等待后推迟。
        let same_provider = MockProvider::new("p1", Behavior::Success(vec![quota(2)]));
        let error = collect_quota_at(&same_provider, &AccountId("b".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("concurrency"), "{error}");
        assert_eq!(same_provider.calls(), 0);

        // 另一个 Provider 仍有全局槽位，可以正常采集，不被 p1 的排队拖住。
        let other_provider = MockProvider::new("p2", Behavior::Success(vec![quota(3)]));
        let other = collect_quota_at(&other_provider, &AccountId("c".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(other_provider.calls(), 1);
        assert_eq!(other.quotas[0].used, 3);

        release.notify_one();
        assert_eq!(first.await.unwrap().unwrap().quotas[0].used, 1);
    }

    #[tokio::test]
    async fn success_record_within_floor_filters_expired_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(1)]));
        let cfg = test_settings();
        let record_file = record_path(&coordination_dir(&cache_path), &account_stem("mock", "a"));
        let observed_at = Utc::now() - Duration::seconds(10);
        let floor = Utc::now() + Duration::seconds(60);
        let mut expired = quota(99);
        expired.window = QuotaWindow::FiveHour;
        expired.reset_at = Some(Utc::now() - Duration::seconds(1));

        write_record(
            &record_file,
            &Record {
                state: CollectionState::Success,
                observed_at: Some(observed_at),
                next_attempt_at: Some(floor),
                quotas: vec![quota(7), expired.clone()],
                ..Record::default()
            },
        )
        .unwrap();

        let reading = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 0);
        assert_eq!(reading.observed_at, observed_at);
        assert_eq!(reading.quotas.len(), 1);
        assert_eq!(reading.quotas[0].used, 7);

        // 全部窗口都已 reset：不能把旧记录伪装成当前成功结果。
        let mut fully_expired = quota(88);
        fully_expired.reset_at = Some(Utc::now() - Duration::seconds(1));
        write_record(
            &record_file,
            &Record {
                state: CollectionState::Success,
                observed_at: Some(observed_at),
                next_attempt_at: Some(floor),
                quotas: vec![fully_expired],
                ..Record::default()
            },
        )
        .unwrap();
        let error = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("expired"), "{error}");
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn removal_invalidates_success_and_preserves_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(5)]));
        let mut cfg = test_settings();
        cfg.quota.min_refresh_interval_ms = 600_000;

        let old_observed = Utc::now() - Duration::seconds(1);
        let old_floor = Utc::now() + Duration::seconds(600);
        let mut cache = QuotaCache::load(&cache_path);
        cache.set_at("mock", "a", vec![quota(42)], old_observed);
        cache.save(&cache_path);
        let record_file = record_path(&coordination_dir(&cache_path), &account_stem("mock", "a"));
        write_record(
            &record_file,
            &Record {
                state: CollectionState::Success,
                observed_at: Some(old_observed),
                next_attempt_at: Some(old_floor),
                quotas: vec![quota(42)],
                ..Record::default()
            },
        )
        .unwrap();

        invalidate_collection_state(&cache_path, "mock", "a")
            .await
            .unwrap();

        assert!(QuotaCache::load(&cache_path).get("mock", "a").is_none());
        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Removed);
        assert_eq!(status.observed_at, None);
        assert!(status.quotas.is_empty());
        assert!(status.next_attempt_at.unwrap() >= old_floor);

        // 冷却下限内不请求、不返回旧成功结果。
        let error = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("removed"), "{error}");
        assert_eq!(provider.calls(), 0);

        // 下限过期后，同 ID 重新导入可正常采集（新观测晚于移除墓碑）。
        let mut record: Record = serde_json::from_slice(&fs::read(&record_file).unwrap()).unwrap();
        record.next_attempt_at = Some(Utc::now() - Duration::seconds(1));
        write_record(&record_file, &record).unwrap();
        let reading = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 1);
        assert_eq!(reading.quotas[0].used, 5);
        assert!(reading.observed_at > old_observed);
        assert!(QuotaCache::load(&cache_path)
            .get("mock", "a")
            .is_some_and(|entry| entry.cached_at == reading.observed_at));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn removal_waits_for_inflight_and_never_resurrects_old_success() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider = Arc::new(MockProvider::new(
            "mock",
            Behavior::Block {
                started: started.clone(),
                release: release.clone(),
                quotas: vec![quota(42)],
            },
        ));
        let mut cfg = test_settings();
        cfg.quota.min_refresh_interval_ms = 600_000;

        let task_provider = provider.clone();
        let task_path = cache_path.clone();
        let task_cfg = cfg.clone();
        let first = tokio::spawn(async move {
            collect_quota_at(
                &*task_provider,
                &AccountId("a".into()),
                &task_path,
                &task_cfg,
            )
            .await
        });
        started.notified().await;

        let invalidate_path = cache_path.clone();
        let invalidate = tokio::spawn(async move {
            invalidate_collection_state(&invalidate_path, "mock", "a").await
        });
        // 确保 rm 侧正在等账号锁。
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        release.notify_one();

        let old = first.await.unwrap().unwrap();
        invalidate.await.unwrap().unwrap();
        assert_eq!(provider.calls(), 1);
        assert!(QuotaCache::load(&cache_path).get("mock", "a").is_none());
        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Removed);
        assert!(status.next_attempt_at.unwrap() >= old.observed_at);

        // 重新导入后拿到的是新观测，不是旧成功结果。
        provider.set_behavior(Behavior::Success(vec![quota(77)]));
        let record_file = record_path(&coordination_dir(&cache_path), &account_stem("mock", "a"));
        let mut record: Record = serde_json::from_slice(&fs::read(&record_file).unwrap()).unwrap();
        record.next_attempt_at = Some(Utc::now() - Duration::seconds(1));
        write_record(&record_file, &record).unwrap();
        let fresh = collect_quota_at(&*provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 2);
        assert_eq!(fresh.quotas[0].used, 77);
        assert!(fresh.observed_at > old.observed_at);
    }

    #[tokio::test]
    async fn cache_write_failure_does_not_erase_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        // 用同名目录让缓存原子替换失败；协调记录仍写在同级 quota_collection/ 里。
        fs::create_dir(&cache_path).unwrap();
        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(21)]));
        let cfg = test_settings();

        let first = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 1);

        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Success);
        assert_eq!(status.observed_at, Some(first.observed_at));

        // 缓存写失败不影响预约/结果记录：下一次直接复用协调记录，不再发请求。
        let second = collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap();
        assert_eq!(provider.calls(), 1);
        assert_eq!(second.observed_at, first.observed_at);
        assert_eq!(second.quotas[0].used, 21);
    }

    #[tokio::test]
    async fn auth_failure_drops_cache_and_keeps_base_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let mut cfg = test_settings();
        cfg.quota.min_refresh_interval_ms = 60_000;

        // 旧的“成功”缓存不能成为 401 之后的可用余量。
        let mut cache = QuotaCache::load(&cache_path);
        cache.set_at(
            "mock",
            "a",
            vec![quota(99)],
            Utc::now() - Duration::minutes(10),
        );
        cache.save(&cache_path);

        // 已有 5 次失败；鉴权失败仍按基础窗口等待，不做指数退避。
        let record_file = record_path(&coordination_dir(&cache_path), &account_stem("mock", "a"));
        let record = Record {
            state: CollectionState::Failure,
            next_attempt_at: Some(Utc::now() - Duration::seconds(1)),
            failures: 5,
            error: Some("usage returned 401 Unauthorized".into()),
            ..Record::default()
        };
        write_record(&record_file, &record).unwrap();

        let provider = MockProvider::new("mock", Behavior::Fail("usage returned 401 Unauthorized"));
        let started = Utc::now();
        collect_quota_at(&provider, &AccountId("a".into()), &cache_path, &cfg)
            .await
            .unwrap_err();
        assert_eq!(provider.calls(), 1);
        assert!(QuotaCache::load(&cache_path).get("mock", "a").is_none());

        let status = read_status(&cache_path, "mock", "a").unwrap().unwrap();
        assert_eq!(status.state, CollectionState::Failure);
        assert_eq!(status.failures, 6);
        let deadline = status.next_attempt_at.unwrap();
        assert!(deadline <= Utc::now() + Duration::seconds(62));
        assert!(deadline >= started + Duration::seconds(59));
    }

    #[tokio::test]
    async fn collection_events_are_emitted() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("quota_cache.json");
        let metrics_path = tmp.path().join("request_metrics.json");
        let provider = MockProvider::new("mock", Behavior::Success(vec![quota(7)]));
        let cfg = test_settings();

        request_metrics::with_sink(metrics_path.clone(), async {
            collect_impl(&provider, &AccountId("a".into()), &cache_path, &cfg, true)
                .await
                .unwrap();
            collect_impl(&provider, &AccountId("a".into()), &cache_path, &cfg, true)
                .await
                .unwrap();
        })
        .await;

        let snapshot = request_metrics::snapshot_at(&metrics_path);
        assert_eq!(
            snapshot["totals"]["collections"]["collection_start"],
            serde_json::json!(1)
        );
        assert_eq!(
            snapshot["totals"]["collections"]["collection_success"],
            serde_json::json!(1)
        );
        assert_eq!(
            snapshot["totals"]["collections"]["cache_hit"],
            serde_json::json!(1)
        );
    }
}
