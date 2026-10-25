//! 有界、无凭证的请求可观测性计数器。
//!
//! 只记录**实际发出**的直接 HTTP 请求与原生 RPC 调用，以及共享采集器上报的
//! collection/cache/coalescing/deferred 事件。所有上游 URL、令牌、邮箱、原始账号
//! 标识与响应正文都不会写入磁盘；账号只保存 SHA-256 派生的短键。
//!
//! 设计边界：
//! - 写入走 `flock` + 读改写 + 临时文件原子替换，CLI/daemon 多进程可安全合并。
//! - 计数器、状态码、账号/Porvider 条目和最近一小时分钟桶全部有上限。
//! - 每个 scope 保留自己的分钟桶；`snapshot()` 额外计算 `last_hour`，避免把生命周期
//!   累计值误当成过去一小时。
//! - `snapshot()` 是纯只读：不建目录、不建锁文件、不修复损坏文件。
//! - 写入是可丢弃的遥测：任何 IO/解析失败都不会影响额度查询主路径。

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 落盘 schema 版本；未知版本按损坏处理，避免用旧结构误读新字段。
const SCHEMA_VERSION: u32 = 1;
/// 每个维度最多保留的账号键数量（按最近使用淘汰）。
const MAX_ACCOUNTS: usize = 128;
/// Provider 键数量上限；Provider id 来自代码常量，仍做防御性限制。
const MAX_PROVIDERS: usize = 32;
/// 最近分钟桶数量（约一小时）。
const MAX_MINUTES: usize = 60;
/// 每个 scope 内不同 HTTP 状态码数量上限；超出归入 `other`。
const MAX_STATUSES: usize = 32;
/// 没有账号归属的事件（如无法从 blob 解析账号的 OAuth 刷新）。
const UNATTRIBUTED: &str = "unattributed";
/// 测试与采集器可显式指定文件路径的隐藏环境变量；必须是绝对路径。
const PATH_ENV: &str = "SUBSWAP_REQUEST_METRICS_PATH";

tokio::task_local! {
    /// 单元测试/显式调用方的一次性落盘目标，避免碰到真实用户目录。
    static SINK: PathBuf;
}

/// 请求类事件计数器。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RequestCounters {
    http_start: u64,
    http_complete: u64,
    native_rpc_start: u64,
    native_rpc_complete: u64,
}

/// 采集器事件计数器。与请求计数分开，避免把缓存命中/合并误读成上游请求。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct CollectionCounters {
    collection_start: u64,
    collection_success: u64,
    collection_failure: u64,
    cache_hit: u64,
    coalesced: u64,
    deferred: u64,
}

/// 一个窗口内的请求/采集汇总（用于快照的 `last_hour`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct ScopeWindow {
    requests: RequestCounters,
    collections: CollectionCounters,
    statuses: BTreeMap<String, u64>,
}

/// 一个统计维度（总量 / Provider / 账号）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Scope {
    requests: RequestCounters,
    collections: CollectionCounters,
    /// 仅记录 HTTP 完成状态码；原生 RPC 不应伪造 HTTP 状态。
    statuses: BTreeMap<String, u64>,
    /// epoch 秒，仅用于账号/Provider 上限淘汰。
    last_seen: i64,
    /// 本维度的最近分钟桶（最多 `MAX_MINUTES` 个）。旧文件缺失时为空。
    minutes: Vec<MinuteBucket>,
    /// 只在只读快照中填充；落盘状态不保存，避免把某个时刻的窗口冻结成旧值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_hour: Option<ScopeWindow>,
}

/// 最近一分钟的全局计数桶。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct MinuteBucket {
    minute: i64,
    requests: RequestCounters,
    collections: CollectionCounters,
    statuses: BTreeMap<String, u64>,
}

/// 持久化根结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct State {
    schema_version: u32,
    updated_at: i64,
    /// 每次从损坏文件恢复后 +1，便于发现遥测曾被重置。
    corrupt_resets: u64,
    totals: Scope,
    providers: BTreeMap<String, Scope>,
    accounts: BTreeMap<String, Scope>,
    minutes: Vec<MinuteBucket>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            updated_at: 0,
            corrupt_resets: 0,
            totals: Scope::default(),
            providers: BTreeMap::new(),
            accounts: BTreeMap::new(),
            minutes: Vec::new(),
        }
    }
}

/// 固定事件集合；未知事件直接丢弃，避免把任意字符串变成磁盘键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    HttpStart,
    HttpComplete,
    NativeRpcStart,
    NativeRpcComplete,
    CollectionStart,
    CollectionSuccess,
    CollectionFailure,
    CacheHit,
    Coalesced,
    Deferred,
}

impl Event {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "http_start" => Some(Self::HttpStart),
            "http_complete" => Some(Self::HttpComplete),
            "native_rpc_start" => Some(Self::NativeRpcStart),
            "native_rpc_complete" => Some(Self::NativeRpcComplete),
            "collection_start" => Some(Self::CollectionStart),
            "collection_success" => Some(Self::CollectionSuccess),
            "collection_failure" => Some(Self::CollectionFailure),
            "cache_hit" => Some(Self::CacheHit),
            "coalesced" => Some(Self::Coalesced),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }
}

/// 记录一次请求/采集事件。路径解析失败或写入失败时静默跳过，不阻塞调用方。
pub async fn record(provider: &str, account_id: &str, event: &str, status: Option<u16>) {
    let Some(path) = sink_path() else {
        return;
    };
    record_at(&path, provider, account_id, event, status).await;
}

/// 以显式文件路径记录事件；供采集器集成测试与调用方隔离运行使用。
///
/// 生产调用通常使用 [`record`]；两者写入格式完全一致。
pub async fn record_at(
    path: &Path,
    provider: &str,
    account_id: &str,
    event: &str,
    status: Option<u16>,
) {
    let Some(event) = Event::parse(event) else {
        return;
    };
    let path = path.to_path_buf();
    let provider = provider.to_string();
    let account = account_id.to_string();
    let now = Utc::now();
    let joined = tokio::task::spawn_blocking(move || {
        record_sync(&path, &provider, &account, event, status, now)
    })
    .await;
    match joined {
        Ok(Ok(())) => {}
        _ => tracing::debug!("request metrics update skipped"),
    }
}

/// 只读快照；不创建目录、锁文件，也不会修复损坏状态。
pub fn snapshot() -> Value {
    match sink_path() {
        Some(path) => snapshot_at(&path),
        None => render(&State::default(), Utc::now(), false, false, false),
    }
}

/// 从显式路径读取只读快照。文件缺失/损坏时返回稳定结构，绝不写盘。
pub fn snapshot_at(path: &Path) -> Value {
    let now = Utc::now();
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<State>(&bytes) {
            Ok(state) if state.schema_version == SCHEMA_VERSION => {
                render(&state, now, true, false, false)
            }
            Ok(_) => render(&State::default(), now, false, true, false),
            Err(_) => render(&State::default(), now, false, true, false),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            render(&State::default(), now, false, false, false)
        }
        Err(_) => render(&State::default(), now, false, false, true),
    }
}

/// 在指定路径的 task-local 作用域内运行；仅用于测试与 hermetic 调用。
///
/// 作用域只影响当前异步任务链，不会泄漏到其他并发任务。
pub async fn with_sink<F>(path: PathBuf, future: F) -> F::Output
where
    F: Future,
{
    SINK.scope(path, future).await
}

// ---------------------------------------------------------------------------
// 落盘与合并
// ---------------------------------------------------------------------------

fn record_sync(
    path: &Path,
    provider: &str,
    account: &str,
    event: Event,
    status: Option<u16>,
    now: DateTime<Utc>,
) -> std::io::Result<()> {
    let provider = provider_key(provider);
    let account = account_key(&provider, account);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = lock_path(path);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let mut state = load_locked(path)?;
    apply_event(&mut state, &provider, &account, event, status, now);
    write_atomic(path, &state)?;
    let _ = fs2::FileExt::unlock(&lock);
    Ok(())
}

/// 持锁读取；解析失败或旧版本时隔离旧文件并从一个干净状态继续。
///
/// 未来版本（`schema_version > 当前`）直接拒绝写入，避免旧二进制降级时覆盖新数据。
fn load_locked(path: &Path) -> std::io::Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<State>(&bytes) {
            Ok(state) if state.schema_version == SCHEMA_VERSION => Ok(state),
            Ok(state) if state.schema_version > SCHEMA_VERSION => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request metrics schema is newer than this build",
            )),
            _ => Ok(recover_corrupt(path)),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error),
    }
}

fn recover_corrupt(path: &Path) -> State {
    let backup = corrupt_path(path);
    // 只保留一份隔离副本，避免反复损坏生成无限文件。
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
    State {
        corrupt_resets: 1,
        ..State::default()
    }
}

fn apply_event(
    state: &mut State,
    provider: &str,
    account: &str,
    event: Event,
    status: Option<u16>,
    now: DateTime<Utc>,
) {
    if state.schema_version != SCHEMA_VERSION {
        *state = State::default();
    }
    let now_ms = now.timestamp_millis();
    let now_secs = now.timestamp();
    let minute = now.timestamp().div_euclid(60);
    state.updated_at = now_ms;

    state.totals.last_seen = now_secs;
    state.totals.last_hour = None;
    bump(&mut state.totals, event, status);
    bump_minutes(&mut state.totals.minutes, minute, event, status);

    let provider_scope = scope_mut(&mut state.providers, provider.to_string(), MAX_PROVIDERS);
    provider_scope.last_seen = now_secs;
    provider_scope.last_hour = None;
    bump(provider_scope, event, status);
    bump_minutes(&mut provider_scope.minutes, minute, event, status);

    let account_scope = scope_mut(&mut state.accounts, account.to_string(), MAX_ACCOUNTS);
    account_scope.last_seen = now_secs;
    account_scope.last_hour = None;
    bump(account_scope, event, status);
    bump_minutes(&mut account_scope.minutes, minute, event, status);

    bump_minutes(&mut state.minutes, minute, event, status);
}

/// 更新一个维度的分钟桶；排序、去重并裁剪到最近 `MAX_MINUTES` 个。
fn bump_minutes(minutes: &mut Vec<MinuteBucket>, minute: i64, event: Event, status: Option<u16>) {
    if let Some(bucket) = minutes.iter_mut().find(|bucket| bucket.minute == minute) {
        bump_bucket(bucket, event, status);
    } else {
        let mut bucket = MinuteBucket {
            minute,
            ..MinuteBucket::default()
        };
        bump_bucket(&mut bucket, event, status);
        minutes.push(bucket);
    }
    minutes.sort_by_key(|bucket| bucket.minute);
    minutes.dedup_by_key(|bucket| bucket.minute);
    if minutes.len() > MAX_MINUTES {
        let excess = minutes.len() - MAX_MINUTES;
        minutes.drain(..excess);
    }
}

fn bump(scope: &mut Scope, event: Event, status: Option<u16>) {
    bump_parts(
        &mut scope.requests,
        &mut scope.collections,
        &mut scope.statuses,
        event,
        status,
    );
}

fn bump_bucket(bucket: &mut MinuteBucket, event: Event, status: Option<u16>) {
    bump_parts(
        &mut bucket.requests,
        &mut bucket.collections,
        &mut bucket.statuses,
        event,
        status,
    );
}

fn bump_parts(
    requests: &mut RequestCounters,
    collections: &mut CollectionCounters,
    statuses: &mut BTreeMap<String, u64>,
    event: Event,
    status: Option<u16>,
) {
    match event {
        Event::HttpStart => requests.http_start = requests.http_start.saturating_add(1),
        Event::HttpComplete => {
            requests.http_complete = requests.http_complete.saturating_add(1);
            if let Some(code) = status.filter(|code| (100..=599).contains(code)) {
                add_status(statuses, code);
            }
        }
        Event::NativeRpcStart => {
            requests.native_rpc_start = requests.native_rpc_start.saturating_add(1)
        }
        Event::NativeRpcComplete => {
            requests.native_rpc_complete = requests.native_rpc_complete.saturating_add(1)
        }
        Event::CollectionStart => {
            collections.collection_start = collections.collection_start.saturating_add(1)
        }
        Event::CollectionSuccess => {
            collections.collection_success = collections.collection_success.saturating_add(1)
        }
        Event::CollectionFailure => {
            collections.collection_failure = collections.collection_failure.saturating_add(1)
        }
        Event::CacheHit => collections.cache_hit = collections.cache_hit.saturating_add(1),
        Event::Coalesced => collections.coalesced = collections.coalesced.saturating_add(1),
        Event::Deferred => collections.deferred = collections.deferred.saturating_add(1),
    }
}

fn add_status(statuses: &mut BTreeMap<String, u64>, code: u16) {
    let key = code.to_string();
    if let Some(count) = statuses.get_mut(&key) {
        *count = count.saturating_add(1);
        return;
    }
    if statuses.len() < MAX_STATUSES - 1 {
        statuses.insert(key, 1);
    } else {
        let other = statuses.entry("other".into()).or_insert(0);
        *other = other.saturating_add(1);
    }
}

fn scope_mut(scopes: &mut BTreeMap<String, Scope>, key: String, cap: usize) -> &mut Scope {
    if !scopes.contains_key(&key) && scopes.len() >= cap {
        if let Some(oldest) = scopes
            .iter()
            .min_by_key(|(name, scope)| (scope.last_seen, name.as_str()))
            .map(|(name, _)| name.clone())
        {
            scopes.remove(&oldest);
        }
    }
    scopes.entry(key).or_default()
}

fn write_atomic(path: &Path, state: &State) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "request_metrics.json".into());
    let temp = path.with_file_name(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        next_temp_id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        // 遥测可丢弃：不做 fsync，避免每个请求事件都等待磁盘；
        // 原子性仍由同目录 rename 保证，崩溃时最多丢最近事件。
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn next_temp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "request_metrics.json".into());
    path.with_file_name(format!("{name}.lock"))
}

fn corrupt_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "request_metrics.json".into());
    path.with_file_name(format!("{name}.corrupt"))
}

// ---------------------------------------------------------------------------
// 路径与隐私
// ---------------------------------------------------------------------------

fn sink_path() -> Option<PathBuf> {
    if let Ok(path) = SINK.try_with(Clone::clone) {
        return Some(path);
    }
    if let Some(path) = env_path() {
        return Some(path);
    }
    // 单元测试默认关闭隐式落盘，避免 provider mock 测试碰真实用户目录。
    if cfg!(test) {
        return None;
    }
    default_path()
}

fn env_path() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(PATH_ENV)?);
    path.is_absolute().then_some(path)
}

/// 与生产 [`crate::paths::AppPaths`] 相同的默认根；使用只读解析，绝不创建目录。
///
/// 写入路径由 `record_sync` 在真正落盘前补建父目录；只读快照不得触碰文件系统。
fn default_path() -> Option<PathBuf> {
    crate::paths::AppPaths::resolve_readonly()
        .ok()
        .map(|paths| paths.data_dir.join("request_metrics.json"))
}

/// Provider 只允许代码常量风格的 ASCII；异常值归入 `other`，防止把 URL/邮箱当维度。
fn provider_key(raw: &str) -> String {
    let value = raw.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return "other".into();
    }
    value.to_ascii_lowercase()
}

/// 账号键 = SHA-256(provider + NUL + 原始账号) 的前 128 bit；不保存原值。
fn account_key(provider: &str, raw: &str) -> String {
    if raw.trim().is_empty() {
        return UNATTRIBUTED.into();
    }
    let mut hasher = Sha256::new();
    hasher.update(b"subswap-request-metrics\0");
    hasher.update(provider.as_bytes());
    hasher.update([0]);
    hasher.update(raw.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// 快照渲染
// ---------------------------------------------------------------------------

/// 把某个维度的分钟桶裁剪到最近一小时，并计算窗口汇总。
fn finalize_scope(scope: &mut Scope, cutoff: i64) {
    scope.minutes.retain(|bucket| bucket.minute >= cutoff);
    if scope.minutes.len() > MAX_MINUTES {
        let excess = scope.minutes.len() - MAX_MINUTES;
        scope.minutes.drain(..excess);
    }
    let mut window = ScopeWindow::default();
    for bucket in &scope.minutes {
        merge_requests(&mut window.requests, &bucket.requests);
        merge_collections(&mut window.collections, &bucket.collections);
        merge_statuses(&mut window.statuses, &bucket.statuses);
    }
    scope.last_hour = Some(window);
}

fn merge_requests(target: &mut RequestCounters, source: &RequestCounters) {
    target.http_start = target.http_start.saturating_add(source.http_start);
    target.http_complete = target.http_complete.saturating_add(source.http_complete);
    target.native_rpc_start = target
        .native_rpc_start
        .saturating_add(source.native_rpc_start);
    target.native_rpc_complete = target
        .native_rpc_complete
        .saturating_add(source.native_rpc_complete);
}

fn merge_collections(target: &mut CollectionCounters, source: &CollectionCounters) {
    target.collection_start = target
        .collection_start
        .saturating_add(source.collection_start);
    target.collection_success = target
        .collection_success
        .saturating_add(source.collection_success);
    target.collection_failure = target
        .collection_failure
        .saturating_add(source.collection_failure);
    target.cache_hit = target.cache_hit.saturating_add(source.cache_hit);
    target.coalesced = target.coalesced.saturating_add(source.coalesced);
    target.deferred = target.deferred.saturating_add(source.deferred);
}

fn merge_statuses(target: &mut BTreeMap<String, u64>, source: &BTreeMap<String, u64>) {
    for (key, count) in source {
        if let Some(existing) = target.get_mut(key) {
            *existing = existing.saturating_add(*count);
            continue;
        }
        if target.len() < MAX_STATUSES - 1 {
            target.insert(key.clone(), *count);
        } else {
            let other = target.entry("other".into()).or_insert(0);
            *other = other.saturating_add(*count);
        }
    }
}

fn render(
    state: &State,
    now: DateTime<Utc>,
    available: bool,
    corrupt: bool,
    unreadable: bool,
) -> Value {
    let mut state = state.clone();
    let now_minute = now.timestamp().div_euclid(60);
    let cutoff = now_minute - (MAX_MINUTES as i64 - 1);
    state.minutes.retain(|bucket| bucket.minute >= cutoff);
    if state.minutes.len() > MAX_MINUTES {
        let excess = state.minutes.len() - MAX_MINUTES;
        state.minutes.drain(..excess);
    }
    // totals 的窗口以顶层分钟桶为准，兼容缺少 per-scope minutes 的旧文件。
    state.totals.minutes = state.minutes.clone();
    finalize_scope(&mut state.totals, cutoff);
    for scope in state.providers.values_mut() {
        finalize_scope(scope, cutoff);
    }
    for scope in state.accounts.values_mut() {
        finalize_scope(scope, cutoff);
    }
    let mut value = serde_json::to_value(&state).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.insert("available".into(), json!(available));
        object.insert("corrupt".into(), json!(corrupt));
        if unreadable {
            object.insert("unreadable".into(), json!(true));
        }
        object.insert("generated_at".into(), json!(now.to_rfc3339()));
        object.insert("window_minutes".into(), json!(MAX_MINUTES));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("metrics").join("request_metrics.json")
    }

    fn event(name: &str) -> Event {
        Event::parse(name).expect("known event")
    }

    #[tokio::test]
    async fn counts_request_collection_and_status_separately() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let raw_account = "user@example.com";
        with_sink(path.clone(), async {
            record("claude", raw_account, "http_start", None).await;
            record("claude", raw_account, "http_complete", Some(200)).await;
            record("claude", raw_account, "http_complete", Some(429)).await;
            record("claude", raw_account, "native_rpc_start", None).await;
            record("claude", raw_account, "native_rpc_complete", None).await;
            record("claude", raw_account, "collection_start", None).await;
            record("claude", raw_account, "collection_success", None).await;
            record("claude", raw_account, "cache_hit", None).await;
            record("claude", raw_account, "coalesced", None).await;
            record("claude", raw_account, "deferred", None).await;
        })
        .await;

        let snap = snapshot_at(&path);
        assert_eq!(snap["available"], json!(true));
        assert_eq!(snap["totals"]["requests"]["http_start"], json!(1));
        assert_eq!(snap["totals"]["requests"]["http_complete"], json!(2));
        assert_eq!(snap["totals"]["requests"]["native_rpc_start"], json!(1));
        assert_eq!(snap["totals"]["requests"]["native_rpc_complete"], json!(1));
        assert_eq!(snap["totals"]["statuses"]["200"], json!(1));
        assert_eq!(snap["totals"]["statuses"]["429"], json!(1));
        assert_eq!(snap["totals"]["collections"]["collection_start"], json!(1));
        assert_eq!(
            snap["totals"]["collections"]["collection_success"],
            json!(1)
        );
        assert_eq!(snap["totals"]["collections"]["cache_hit"], json!(1));
        assert_eq!(snap["totals"]["collections"]["coalesced"], json!(1));
        assert_eq!(snap["totals"]["collections"]["deferred"], json!(1));

        assert_eq!(
            snap["providers"]["claude"]["requests"]["http_start"],
            json!(1)
        );
        assert_eq!(
            snap["totals"]["last_hour"]["requests"]["http_start"],
            json!(1)
        );
        assert_eq!(
            snap["providers"]["claude"]["last_hour"]["requests"]["http_complete"],
            json!(2)
        );
        assert_eq!(snap["accounts"].as_object().unwrap().len(), 1);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(
            !stored.contains(raw_account),
            "raw account leaked: {stored}"
        );
        assert!(!stored.contains("example.com"));
        assert!(!stored.contains("429 rate limited"));
        let account = snap["accounts"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(
            account["last_hour"]["requests"]["native_rpc_start"],
            json!(1)
        );
        assert_eq!(
            account["last_hour"]["collections"]["collection_success"],
            json!(1)
        );
    }

    #[test]
    fn unknown_events_providers_and_statuses_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let now = Utc::now();
        record_sync(
            &path,
            "https://api.example.com/user@example.com",
            "acct",
            event("http_complete"),
            Some(200),
            now,
        )
        .unwrap();
        assert!(record_sync(&path, "claude", "acct", Event::HttpStart, None, now).is_ok());
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("\"other\""));
        assert!(!stored.contains("api.example.com"));
        assert!(!stored.contains("user@example.com"));

        // 未知事件名不会创建新键。
        assert!(Event::parse("made_up_event").is_none());

        let mut state = State::default();
        for code in 100..=599 {
            apply_event(
                &mut state,
                "claude",
                "acct",
                Event::HttpComplete,
                Some(code),
                now,
            );
        }
        assert!(state.totals.statuses.len() <= MAX_STATUSES);
        assert!(state.totals.statuses.contains_key("other"));
    }

    #[test]
    fn accounts_are_capped_but_totals_remain_complete() {
        let mut state = State::default();
        let now = Utc::now();
        for index in 0..(MAX_ACCOUNTS + 50) {
            apply_event(
                &mut state,
                "claude",
                &format!("account-{index}"),
                Event::HttpStart,
                None,
                now,
            );
        }
        assert_eq!(state.accounts.len(), MAX_ACCOUNTS);
        assert_eq!(state.totals.requests.http_start as usize, MAX_ACCOUNTS + 50);
        // 被淘汰的账号再次出现时重新进入有界表。
        apply_event(
            &mut state,
            "claude",
            "account-0",
            Event::HttpStart,
            None,
            now,
        );
        assert_eq!(state.accounts.len(), MAX_ACCOUNTS);
        assert_eq!(state.totals.requests.http_start as usize, MAX_ACCOUNTS + 51);
    }

    #[test]
    fn last_hour_excludes_old_events_for_totals_provider_and_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let now = Utc::now();
        let mut state = State::default();
        apply_event(
            &mut state,
            "claude",
            "acct",
            Event::HttpStart,
            None,
            now - chrono::Duration::hours(2),
        );
        apply_event(
            &mut state,
            "claude",
            "acct",
            Event::HttpStart,
            None,
            now - chrono::Duration::minutes(30),
        );
        apply_event(
            &mut state,
            "claude",
            "acct",
            Event::HttpComplete,
            Some(200),
            now - chrono::Duration::minutes(30),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_atomic(&path, &state).unwrap();

        let snap = snapshot_at(&path);
        assert_eq!(snap["totals"]["requests"]["http_start"], json!(2));
        assert_eq!(
            snap["totals"]["last_hour"]["requests"]["http_start"],
            json!(1),
            "last_hour must not reuse lifetime totals"
        );
        assert_eq!(snap["totals"]["last_hour"]["statuses"]["200"], json!(1));
        assert_eq!(
            snap["providers"]["claude"]["requests"]["http_start"],
            json!(2)
        );
        assert_eq!(
            snap["providers"]["claude"]["last_hour"]["requests"]["http_start"],
            json!(1)
        );
        let account = snap["accounts"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(account["requests"]["http_start"], json!(2));
        assert_eq!(account["last_hour"]["requests"]["http_start"], json!(1));
        assert_eq!(account["last_hour"]["requests"]["http_complete"], json!(1));
        // 快照只展示窗口内的桶，旧事件不影响 last_hour。
        assert_eq!(
            snap["providers"]["claude"]["minutes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(account["minutes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn older_scope_files_without_minutes_report_zero_last_hour() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = json!({
            "schema_version": 1,
            "updated_at": 0,
            "corrupt_resets": 0,
            "totals": {"requests": {"http_start": 9}, "statuses": {"200": 9}},
            "providers": {"claude": {"requests": {"http_start": 4}}},
            "accounts": {},
            "minutes": []
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let snap = snapshot_at(&path);
        assert_eq!(snap["available"], json!(true));
        assert_eq!(snap["totals"]["requests"]["http_start"], json!(9));
        assert_eq!(
            snap["totals"]["last_hour"]["requests"]["http_start"],
            json!(0)
        );
        assert_eq!(
            snap["providers"]["claude"]["requests"]["http_start"],
            json!(4)
        );
        assert_eq!(
            snap["providers"]["claude"]["last_hour"]["requests"]["http_start"],
            json!(0)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn minute_buckets_keep_only_the_latest_hour() {
        let mut state = State::default();
        let start = Utc::now() - chrono::Duration::minutes(90);
        for minute in 0..90 {
            apply_event(
                &mut state,
                "claude",
                "acct",
                Event::HttpStart,
                None,
                start + chrono::Duration::minutes(minute),
            );
        }
        assert_eq!(state.minutes.len(), MAX_MINUTES);
        assert_eq!(state.totals.minutes.len(), MAX_MINUTES);
        let provider = state.providers.get("claude").unwrap();
        assert_eq!(provider.minutes.len(), MAX_MINUTES);
        let account = state.accounts.values().next().unwrap();
        assert_eq!(account.minutes.len(), MAX_MINUTES);
        assert_eq!(
            state.minutes.first().unwrap().minute,
            (start + chrono::Duration::minutes(30))
                .timestamp()
                .div_euclid(60)
        );
        assert_eq!(
            provider.minutes.first().unwrap().minute,
            state.minutes.first().unwrap().minute
        );
        assert_eq!(
            account.minutes.first().unwrap().minute,
            state.minutes.first().unwrap().minute
        );
    }

    #[test]
    fn snapshot_at_missing_root_creates_no_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("absent-subswap-home");
        let path = root.join("data").join("request_metrics.json");
        let snap = snapshot_at(&path);
        assert_eq!(snap["available"], json!(false));
        assert!(!root.exists(), "snapshot created the application root");
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn snapshot_is_read_only_for_missing_corrupt_and_valid_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("request_metrics.json");

        let missing = snapshot_at(&path);
        assert_eq!(missing["available"], json!(false));
        assert_eq!(missing["corrupt"], json!(false));
        assert!(!path.parent().unwrap().exists(), "snapshot created dirs");

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not-json").unwrap();
        let before = std::fs::read(&path).unwrap();
        let corrupt = snapshot_at(&path);
        assert_eq!(corrupt["available"], json!(false));
        assert_eq!(corrupt["corrupt"], json!(true));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!lock_path(&path).exists(), "snapshot created a lock file");

        std::fs::write(&path, serde_json::to_vec(&State::default()).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let valid = snapshot_at(&path);
        assert_eq!(valid["available"], json!(true));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!lock_path(&path).exists());
    }

    #[test]
    fn corrupt_file_is_quarantined_on_write_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"broken").unwrap();
        record_sync(&path, "claude", "acct", Event::HttpStart, None, Utc::now()).unwrap();
        assert!(corrupt_path(&path).exists());
        let snap = snapshot_at(&path);
        assert_eq!(snap["available"], json!(true));
        assert_eq!(snap["corrupt_resets"], json!(1));
        assert_eq!(snap["totals"]["requests"]["http_start"], json!(1));
    }

    #[test]
    fn newer_schema_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let state =
            serde_json::json!({"schema_version": 99, "totals": {"requests": {"http_start": 7}}});
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(record_sync(&path, "claude", "acct", Event::HttpStart, None, Utc::now()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(snapshot_at(&path)["available"], json!(false));
    }

    #[test]
    fn concurrent_threads_merge_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let now = Utc::now();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let path = path.clone();
                scope.spawn(move || {
                    for _ in 0..25 {
                        record_sync(
                            &path,
                            "claude",
                            "shared-account",
                            Event::HttpComplete,
                            Some(200),
                            now,
                        )
                        .unwrap();
                    }
                });
            }
        });
        let snap = snapshot_at(&path);
        assert_eq!(snap["totals"]["requests"]["http_complete"], json!(400));
        assert_eq!(
            snap["totals"]["last_hour"]["requests"]["http_complete"],
            json!(400)
        );
        assert_eq!(snap["totals"]["statuses"]["200"], json!(400));
        assert_eq!(
            snap["providers"]["claude"]["last_hour"]["statuses"]["200"],
            json!(400)
        );
        assert_eq!(snap["accounts"].as_object().unwrap().len(), 1);
        let account = snap["accounts"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(account["requests"]["http_complete"], json!(400));
        assert_eq!(
            account["last_hour"]["requests"]["http_complete"],
            json!(400)
        );
    }

    #[test]
    fn metrics_child_writer() {
        let Ok(path) = std::env::var("SUBSWAP_METRICS_CHILD_PATH") else {
            return;
        };
        let path = PathBuf::from(path);
        for _ in 0..40 {
            record_sync(
                &path,
                "claude",
                "process-shared",
                Event::NativeRpcComplete,
                None,
                Utc::now(),
            )
            .unwrap();
        }
    }

    #[test]
    fn separate_processes_merge_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..3 {
            children.push(
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "request_metrics::tests::metrics_child_writer",
                        "--nocapture",
                    ])
                    .env("SUBSWAP_METRICS_CHILD_PATH", &path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let snap = snapshot_at(&path);
        assert_eq!(
            snap["totals"]["requests"]["native_rpc_complete"],
            json!(120)
        );
        assert_eq!(
            snap["totals"]["last_hour"]["requests"]["native_rpc_complete"],
            json!(120)
        );
        assert_eq!(snap["accounts"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn hour_buckets_are_filtered_in_snapshot_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let now = Utc::now();
        let mut state = State::default();
        for minute in 0..70 {
            state.minutes.push(MinuteBucket {
                minute: now.timestamp().div_euclid(60) - minute,
                ..MinuteBucket::default()
            });
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let snap = snapshot_at(&path);
        assert_eq!(snap["minutes"].as_array().unwrap().len(), MAX_MINUTES);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
