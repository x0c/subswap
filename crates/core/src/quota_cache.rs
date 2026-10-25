//! quota 查询结果的磁盘缓存。
//!
//! 存储在 `cache_dir/quota_cache.json`，按单个 quota 窗口过滤：
//! - 有 `reset_at` 的窗口：reset_at 已过则该窗口数据失效，不返回。
//! - 无 `reset_at` 的窗口：按窗口类型兜底 TTL（5h/7d/30d），超出则失效。
//! - 所有窗口都失效 → 整条 entry 不返回（等同缓存未命中）。
//!
//! 缓存是可丢弃数据：读失败回空缓存；写失败只 `tracing::warn`，不阻塞主流程。
//!
//! 并发写入：`save` 先锁 `<cache>.lock`，重读磁盘最新状态，再按「观测/事件时间」只合并
//! 本进程真正改过的账号，最后用临时文件 + rename 原子替换。旧进程的读写不能覆盖其他进程
//! 更新的测量结果、失败退避或 `rm` 墓碑。只读加载的缓存 `save` 不会重写文件。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::model::{Quota, QuotaWindow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub quotas: Vec<Quota>,
    pub cached_at: DateTime<Utc>,
}

/// `get()` 返回的有效缓存快照；quotas 已过滤掉过期窗口。
pub struct ValidEntry {
    pub quotas: Vec<Quota>,
    pub cached_at: DateTime<Utc>,
}

/// 连续查询失败的记录，用于失败退避。只存错误摘要，不含任何凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    /// 最近一次失败时间。
    pub failed_at: DateTime<Utc>,
    /// 连续失败次数（成功一次即清零）。
    pub consecutive: u32,
    /// 最近一次失败的错误文本，退避期间原样复用给展示层。
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuotaCache {
    entries: HashMap<String, CachedEntry>,
    /// 失败退避表。老版本缓存文件没有该字段，`default` 保证可平滑升级。
    #[serde(default)]
    failures: HashMap<String, FailureEntry>,
    /// 账号移除墓碑：`key -> 移除事件时间`。
    ///
    /// 跨进程合并时必须保留：一个在 `rm` 之前加载缓存的旧进程，不能用更早的观测把
    /// 已删除账号的成功缓存或失败记录复活。
    #[serde(default)]
    removals: HashMap<String, DateTime<Utc>>,
    /// 成功查询清除失败记录的事件时间。用于防止旧进程把更早的失败退避重新写回。
    #[serde(default)]
    failure_clears: HashMap<String, DateTime<Utc>>,
    /// 本次进程内被 `set` / `record_failure` 改过的成功条目；不落盘，`save` 只合并这些 key。
    #[serde(skip)]
    dirty_entries: HashSet<String>,
    /// 本次进程内被改过的失败记录。
    #[serde(skip)]
    dirty_failures: HashSet<String>,
    /// 本次进程内新增/更新的移除墓碑。
    #[serde(skip)]
    dirty_removals: HashSet<String>,
    /// 本次进程内新增/更新的失败清除事件。
    #[serde(skip)]
    dirty_failure_clears: HashSet<String>,
}

/// 原子写临时文件序号，避免同进程多个缓存实例并发 `save` 时临时文件重名。
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

impl QuotaCache {
    /// 从文件加载缓存；文件不存在或解析失败返回空缓存。
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 将缓存写入文件；失败只 warn，不抛错（缓存是可丢弃数据，对齐 audit 写失败策略）。
    ///
    /// 只写本次进程真正改过的账号：先获取 `<path>.lock`，重读磁盘最新值，按观测/事件时间
    /// 合并后原子替换。只读加载（没有任何 `set` / `record_failure` / `remove`）时直接返回，
    /// 不触碰文件，避免无意义重写和与其它进程的写竞争。
    ///
    /// 这是阻塞方法（`flock` + 磁盘 IO）；async 调用方必须放进 `spawn_blocking`。
    pub fn save(&mut self, path: &Path) {
        if !self.is_dirty() {
            return;
        }
        if let Err(e) = self.save_locked(path) {
            tracing::warn!(err=%e, path=%path.display(), "quota cache write failed");
        }
    }

    fn is_dirty(&self) -> bool {
        !(self.dirty_entries.is_empty()
            && self.dirty_failures.is_empty()
            && self.dirty_removals.is_empty()
            && self.dirty_failure_clears.is_empty())
    }

    /// 在缓存锁内完成「重读 → 合并 → 原子替换」。
    fn save_locked(&mut self, path: &Path) -> std::io::Result<()> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        // 保持旧行为：父目录不存在时不创建、不写文件；保留 dirty 标记供下次重试。
        if let Some(parent) = parent {
            if !parent.exists() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("quota cache parent missing: {}", parent.display()),
                ));
            }
        }

        let lock_path = lock_path(path);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        FileExt::lock_exclusive(&lock)?;

        // 锁内重读，拿到其它进程最新写入；解析失败仍按「缓存可丢弃」回空缓存。
        let mut latest = QuotaCache::load(path);
        merge_into(&mut latest, self);
        write_atomic(path, &latest)?;

        // 用合并后的磁盘状态替换本进程视图，避免后续 `get` 继续看到已被删除/已被更新的旧值。
        self.entries = latest.entries;
        self.failures = latest.failures;
        self.removals = latest.removals;
        self.failure_clears = latest.failure_clears;
        self.dirty_entries.clear();
        self.dirty_failures.clear();
        self.dirty_removals.clear();
        self.dirty_failure_clears.clear();

        let _ = FileExt::unlock(&lock);
        Ok(())
    }

    /// 获取仍有效的缓存快照，已过期的窗口被过滤。
    /// 若所有窗口都过期或无缓存，返回 None。
    pub fn get(&self, provider: &str, account_id: &str) -> Option<ValidEntry> {
        let entry = self.entries.get(&cache_key(provider, account_id))?;
        let now = Utc::now();
        let valid_quotas: Vec<Quota> = entry
            .quotas
            .iter()
            .filter(|q| !quota_expired(q, entry.cached_at, now))
            .cloned()
            .collect();
        if valid_quotas.is_empty() {
            return None;
        }
        Some(ValidEntry {
            quotas: valid_quotas,
            cached_at: entry.cached_at,
        })
    }

    /// 若缓存足够新(`cached_at` 距今 < `max_age`)且仍有有效窗口，返回快照；否则 None。
    /// 用于「缓存节流」：够新就直接复用、跳过真实 quota 查询，避免高频打 usage 端点。
    ///
    /// 最近一次失败是鉴权/缺凭据时不复用成功缓存：那些数字往往是串号或已作废令牌留下的，
    /// 继续当 `Ready` 会让「needs re-login」的行看起来像额度耗尽。
    pub fn fresh(
        &self,
        provider: &str,
        account_id: &str,
        max_age: std::time::Duration,
    ) -> Option<ValidEntry> {
        let key = cache_key(provider, account_id);
        if self
            .failures
            .get(&key)
            .is_some_and(|entry| is_authentication_failure(&entry.error))
        {
            return None;
        }
        let entry = self.entries.get(&key)?;
        let age = Utc::now() - entry.cached_at;
        // age 为负(时钟回拨/未来时间戳)时视为「不新鲜」,保守地重新查询。
        if age < Duration::zero() || age >= Duration::from_std(max_age).ok()? {
            return None;
        }
        self.get(provider, account_id)
    }

    /// 更新或插入缓存条目；同时清掉该账号更早的失败退避记录。
    pub fn set(&mut self, provider: &str, account_id: &str, quotas: Vec<Quota>) {
        self.set_at(provider, account_id, quotas, Utc::now());
    }

    /// 用指定的观测时间写入缓存条目。
    ///
    /// 协调器会把「真实抓取完成时间」或「复用的原始测量时间」传进来；调用方不得在
    /// 复用缓存/合并等待结果时把时间刷新成现在，否则会把旧数据伪装成实时额度。
    /// 比已有移除墓碑更早的观测会被忽略，不能复活已删除账号。
    pub fn set_at(
        &mut self,
        provider: &str,
        account_id: &str,
        quotas: Vec<Quota>,
        observed_at: DateTime<Utc>,
    ) {
        let key = cache_key(provider, account_id);
        if let Some(removed_at) = self.removals.get(&key).copied() {
            if removed_at > observed_at {
                return;
            }
            self.removals.remove(&key);
            self.dirty_removals.insert(key.clone());
        }
        self.entries.insert(
            key.clone(),
            CachedEntry {
                quotas,
                cached_at: observed_at,
            },
        );
        self.dirty_entries.insert(key.clone());

        // 成功观测只清除不晚于它的失败；比它更新的失败仍代表最近一次真实事件。
        if self
            .failures
            .get(&key)
            .map_or(true, |failure| failure.failed_at <= observed_at)
        {
            self.failures.remove(&key);
            self.dirty_failures.insert(key.clone());
            self.failure_clears.insert(key.clone(), observed_at);
            self.dirty_failure_clears.insert(key);
        }
    }

    /// 记一次查询失败，累加连续失败次数。
    ///
    /// 失败结果不进 `entries`（不能拿失败当数据用），单独记在这里只为算退避窗口。
    /// 鉴权失败还会丢掉旧的成功缓存，避免下一屏把过期 0% 当成真实余量。
    pub fn record_failure(&mut self, provider: &str, account_id: &str, error: &str) {
        let key = cache_key(provider, account_id);
        let now = Utc::now();
        if is_authentication_failure(error) {
            self.entries.remove(&key);
            self.dirty_entries.insert(key.clone());
            // 鉴权失败同时是「成功缓存失效」事件；用墓碑防止旧进程把它写回来。
            self.removals.insert(key.clone(), now);
            self.dirty_removals.insert(key.clone());
        }
        let consecutive = self
            .failures
            .get(&key)
            .map(|f| f.consecutive.saturating_add(1))
            .unwrap_or(1);
        self.failures.insert(
            key.clone(),
            FailureEntry {
                failed_at: now,
                consecutive,
                error: error.to_string(),
            },
        );
        self.dirty_failures.insert(key.clone());
        // 新失败事件晚于旧的成功清除事件；旧清除不再需要保留。
        if self
            .failure_clears
            .get(&key)
            .is_some_and(|cleared_at| *cleared_at <= now)
        {
            self.failure_clears.remove(&key);
            self.dirty_failure_clears.insert(key);
        }
    }

    /// 只读查看最近一次失败记录；供只读状态导出展示 `needs re-login` 等错误，
    /// 不改动缓存，也不触发任何网络请求。
    pub fn failure(&self, provider: &str, account_id: &str) -> Option<&FailureEntry> {
        self.failures.get(&cache_key(provider, account_id))
    }

    /// 删除某账号的成功缓存与失败退避（`rm` 时调用，避免尸号数字粘在下一个同 id 导入上）。
    ///
    /// 同时写入移除墓碑：在 `rm` 之前加载缓存的旧进程不能把旧成功值或旧失败退避复活。
    pub fn remove(&mut self, provider: &str, account_id: &str) {
        let key = cache_key(provider, account_id);
        let now = Utc::now();
        self.entries.remove(&key);
        self.failures.remove(&key);
        self.dirty_entries.insert(key.clone());
        self.dirty_failures.insert(key.clone());
        self.removals.insert(key.clone(), now);
        self.dirty_removals.insert(key.clone());
        self.failure_clears.insert(key.clone(), now);
        self.dirty_failure_clears.insert(key);
    }

    /// 该账号是否仍处于失败退避窗口内；是则返回最近一次失败记录，调用方应跳过真实查询。
    ///
    /// 退避时长 = `base * 2^(consecutive - 1)`，封顶 `cap`。
    pub fn in_failure_backoff(
        &self,
        provider: &str,
        account_id: &str,
        base: std::time::Duration,
        cap: std::time::Duration,
    ) -> Option<&FailureEntry> {
        let entry = self.failures.get(&cache_key(provider, account_id))?;
        // 401/403 往往会在原生客户端刚刷新凭据后立刻恢复。仍保留一个基础窗口避免请求风暴，
        // 但不把旧鉴权失败指数退避到 15 分钟，否则用户明明已经恢复登录仍会长期看到旧错误。
        let effective_cap = if is_authentication_failure(&entry.error) {
            base
        } else {
            cap
        };
        let backoff = failure_backoff(base, effective_cap, entry.consecutive);
        let age = Utc::now() - entry.failed_at;
        // age 为负(时钟回拨)时视为已过窗口，保守地允许重查。
        if age < Duration::zero() || age >= Duration::from_std(backoff).ok()? {
            return None;
        }
        Some(entry)
    }
}

/// 把本进程的 dirty 变更合并进锁内重读的 `latest`。
fn merge_into(latest: &mut QuotaCache, local: &QuotaCache) {
    // 1. 墓碑只增不减：取两边更晚的事件时间。
    for key in &local.dirty_removals {
        if let Some(removed_at) = local.removals.get(key) {
            let slot = latest.removals.entry(key.clone()).or_insert(*removed_at);
            if *removed_at > *slot {
                *slot = *removed_at;
            }
        }
    }
    for key in &local.dirty_failure_clears {
        if let Some(cleared_at) = local.failure_clears.get(key) {
            let slot = latest
                .failure_clears
                .entry(key.clone())
                .or_insert(*cleared_at);
            if *cleared_at > *slot {
                *slot = *cleared_at;
            }
        }
    }

    // 2. 成功条目：只处理本次真正改过的 key（以及伴随删除墓碑的 key）。
    let mut entry_keys: HashSet<&String> = local.dirty_entries.iter().collect();
    entry_keys.extend(local.dirty_removals.iter());
    for key in entry_keys {
        let local_entry = local
            .dirty_entries
            .contains(key)
            .then(|| local.entries.get(key))
            .flatten();
        let local_removal = local
            .dirty_removals
            .contains(key)
            .then(|| local.removals.get(key))
            .flatten();
        merge_entry(latest, key, local_entry, local_removal);
    }

    // 3. 失败记录：成功条目本身也是一次「清除更早失败」的观测。
    let mut failure_keys: HashSet<&String> = local.dirty_failures.iter().collect();
    failure_keys.extend(local.dirty_failure_clears.iter());
    for key in failure_keys {
        let local_failure = local
            .dirty_failures
            .contains(key)
            .then(|| local.failures.get(key))
            .flatten();
        let local_clear = local
            .dirty_failure_clears
            .contains(key)
            .then(|| local.failure_clears.get(key))
            .flatten();
        let winning_entry = latest.entries.get(key).map(|entry| entry.cached_at);
        merge_failure(latest, key, local_failure, local_clear, winning_entry);
    }
}

/// 合并单个账号的成功缓存：按观测时间取新者，墓碑阻止更早的观测复活。
fn merge_entry(
    latest: &mut QuotaCache,
    key: &str,
    local_entry: Option<&CachedEntry>,
    local_removal: Option<&DateTime<Utc>>,
) {
    let removal = match (latest.removals.get(key).copied(), local_removal.copied()) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let disk_entry = latest
        .entries
        .get(key)
        .filter(|entry| removal.map_or(true, |removed_at| entry.cached_at > removed_at))
        .cloned();
    let local_entry = local_entry
        .filter(|entry| removal.map_or(true, |removed_at| entry.cached_at > removed_at))
        .cloned();

    let winner = match (local_entry, disk_entry) {
        (Some(local), Some(disk)) => {
            if local.cached_at >= disk.cached_at {
                Some(local)
            } else {
                Some(disk)
            }
        }
        (Some(local), None) => Some(local),
        (None, Some(disk)) => Some(disk),
        (None, None) => None,
    };

    match winner {
        Some(entry) => {
            if removal.is_some_and(|removed_at| removed_at > entry.cached_at) {
                latest.entries.remove(key);
                if let Some(removed_at) = removal {
                    latest.removals.insert(key.to_string(), removed_at);
                }
            } else {
                latest.entries.insert(key.to_string(), entry);
                // 已有更新的成功观测时，旧墓碑不再需要：任何更早的旧快照都会被这条新观测挡住。
                latest.removals.remove(key);
            }
        }
        None => {
            latest.entries.remove(key);
            if let Some(removed_at) = removal {
                latest.removals.insert(key.to_string(), removed_at);
            }
        }
    }
}

/// 合并单个账号的失败记录；成功观测和显式清除都能清掉更早的失败。
fn merge_failure(
    latest: &mut QuotaCache,
    key: &str,
    local_failure: Option<&FailureEntry>,
    local_clear: Option<&DateTime<Utc>>,
    winning_entry_at: Option<DateTime<Utc>>,
) {
    let mut clear = match (
        latest.failure_clears.get(key).copied(),
        local_clear.copied(),
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if let Some(observed_at) = winning_entry_at {
        clear = match (clear, Some(observed_at)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
    let disk_failure = latest
        .failures
        .get(key)
        .filter(|failure| clear.map_or(true, |cleared_at| failure.failed_at > cleared_at))
        .cloned();
    let local_failure = local_failure
        .filter(|failure| clear.map_or(true, |cleared_at| failure.failed_at > cleared_at))
        .cloned();

    let winner = match (local_failure, disk_failure) {
        (Some(local), Some(disk)) => {
            if local.failed_at >= disk.failed_at {
                Some(local)
            } else {
                Some(disk)
            }
        }
        (Some(local), None) => Some(local),
        (None, Some(disk)) => Some(disk),
        (None, None) => None,
    };

    match winner {
        Some(failure) => {
            latest.failures.insert(key.to_string(), failure);
            // 失败比清除事件更新，旧清除事件可以丢弃。
            latest.failure_clears.remove(key);
        }
        None => {
            latest.failures.remove(key);
            if let Some(cleared_at) = clear {
                latest.failure_clears.insert(key.to_string(), cleared_at);
            }
        }
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// 临时文件 + rename 原子替换；写盘后 fsync，避免崩溃留下半截 JSON。
fn write_atomic(path: &Path, cache: &QuotaCache) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(cache).map_err(std::io::Error::other)?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "quota_cache.json".to_string());
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".{file_name}.{}.{}.tmp", std::process::id(), seq);
    let temp = parent.map_or_else(|| PathBuf::from(&temp_name), |p| p.join(&temp_name));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result?;

    // 目录项也 fsync 一次，保证 rename 在崩溃后仍然可见（失败不阻塞缓存写入）。
    #[cfg(unix)]
    if let Some(parent) = parent {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// 连续失败 n 次后的退避时长：`base * 2^(n-1)`，封顶 `cap`。
fn failure_backoff(
    base: std::time::Duration,
    cap: std::time::Duration,
    consecutive: u32,
) -> std::time::Duration {
    let shift = consecutive.saturating_sub(1).min(16);
    base.saturating_mul(1_u32 << shift).min(cap.max(base))
}

fn cache_key(provider: &str, account_id: &str) -> String {
    format!("{provider}::{account_id}")
}

/// 额度查询失败是否属于确定性鉴权/缺凭据，而不是网络或 429。
///
/// 这类错误不得把旧 quota 缓存当可用余量展示，也不得成为自动切换候选。
pub fn is_authentication_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("re-login")
        || lower.contains("invalid_grant")
        || lower.contains("missing credential")
        || lower.contains("no credentials")
        || lower.contains("no keyring entry")
        || lower.contains("access token missing")
        || lower.contains("belong to another")
}

/// 过滤掉在 `now` 已经 reset/过期的窗口。
///
/// 协调器复用持久记录里的成功结果时必须走这里：记录仍在最短请求间隔内，不代表
/// 其中的 5h/7d 窗口没有 reset；过期窗口不能继续作为当前余量展示或参与决策。
pub(crate) fn current_quotas(
    quotas: &[Quota],
    observed_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<Quota> {
    quotas
        .iter()
        .filter(|quota| !quota_expired(quota, observed_at, now))
        .cloned()
        .collect()
}

/// 判断单个 quota 窗口是否已失效。
fn quota_expired(q: &Quota, cached_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    if let Some(reset_at) = q.reset_at {
        // 窗口已重置 → 数据失效
        return reset_at <= now;
    }
    // 没有 reset_at：按窗口类型兜底 TTL
    cached_at + window_ttl(q.window) <= now
}

fn window_ttl(window: QuotaWindow) -> Duration {
    match window {
        QuotaWindow::FiveHour | QuotaWindow::Custom => Duration::hours(5),
        QuotaWindow::SevenDay => Duration::days(7),
        QuotaWindow::Month
        | QuotaWindow::FirstPartyModels
        | QuotaWindow::Api
        | QuotaWindow::Credits
        | QuotaWindow::ResetCredits => Duration::days(30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountId, QuotaStatus};

    fn sample_quota() -> Quota {
        Quota {
            provider: "claude".into(),
            account_id: AccountId("a@x.com".into()),
            window: QuotaWindow::SevenDay,
            used: 10,
            limit: 100,
            reset_at: Some(Utc::now() + Duration::days(3)),
            status: QuotaStatus::Ok,
            note: None,
        }
    }

    fn quota_with_used(used: u64) -> Quota {
        Quota {
            used,
            ..sample_quota()
        }
    }

    #[test]
    fn fresh_returns_only_within_window() {
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        // 刚写入 → 90s 窗口内算新鲜。
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(90))
            .is_some());
        // 0 窗口 → 任何缓存都视为不新鲜,强制重新查询。
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(0))
            .is_none());
        // 未知账号 → None。
        assert!(cache
            .fresh("claude", "b@x.com", std::time::Duration::from_secs(90))
            .is_none());
    }

    #[test]
    fn failure_backoff_grows_and_clears_on_success() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let mut cache = QuotaCache::default();

        // 首次失败 → 退避 base，窗口内跳过查询。
        cache.record_failure("claude", "a@x.com", "429 rate limited");
        let hit = cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .unwrap();
        assert_eq!(hit.consecutive, 1);
        assert_eq!(hit.error, "429 rate limited");

        // 第 3 次失败 → 退避 4*base；把 failed_at 拨到 3*base 之前仍在窗口内。
        cache.record_failure("claude", "a@x.com", "429 rate limited");
        cache.record_failure("claude", "a@x.com", "429 rate limited");
        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(270);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_some());

        // 超过封顶时长 → 放行重查。
        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(1000);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_none());

        // 查成功一次即清零，不再退避。
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_none());
    }

    #[test]
    fn authentication_failure_backoff_is_capped_at_base_interval() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        let mut cache = QuotaCache::default();

        for _ in 0..5 {
            cache.record_failure(
                "claude",
                "a@x.com",
                "quota fetch: usage returned 401 Unauthorized",
            );
        }
        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(89);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_some());

        cache.failures.get_mut("claude::a@x.com").unwrap().failed_at =
            Utc::now() - Duration::seconds(91);
        assert!(cache
            .in_failure_backoff("claude", "a@x.com", base, cap)
            .is_none());
    }

    #[test]
    fn auth_failure_drops_success_cache_and_skips_fresh() {
        let mut cache = QuotaCache::default();
        cache.set("cursor", "auth0|dead", vec![sample_quota()]);
        cache.record_failure(
            "cursor",
            "auth0|dead",
            "re-login required for cursor:auth0|dead; stored credentials belong to another Cursor account",
        );
        assert!(cache.get("cursor", "auth0|dead").is_none());
        assert!(cache
            .fresh("cursor", "auth0|dead", std::time::Duration::from_secs(90))
            .is_none());
    }

    #[test]
    fn remove_clears_success_and_failure_entries() {
        let mut cache = QuotaCache::default();
        cache.set("cursor", "auth0|gone", vec![sample_quota()]);
        cache.record_failure("cursor", "auth0|gone", "429 rate limited");
        cache.remove("cursor", "auth0|gone");
        assert!(cache.get("cursor", "auth0|gone").is_none());
        assert!(cache.failure("cursor", "auth0|gone").is_none());
        assert!(cache
            .in_failure_backoff(
                "cursor",
                "auth0|gone",
                std::time::Duration::from_secs(90),
                std::time::Duration::from_secs(900)
            )
            .is_none());
    }

    #[test]
    fn failure_backoff_is_capped() {
        let base = std::time::Duration::from_secs(90);
        let cap = std::time::Duration::from_secs(900);
        assert_eq!(failure_backoff(base, cap, 1), base);
        assert_eq!(failure_backoff(base, cap, 3), base * 4);
        // 2^(20-1) * 90s 远超封顶,必须被 cap 住。
        assert_eq!(failure_backoff(base, cap, 20), cap);
    }

    #[test]
    fn fresh_rejects_stale_entry() {
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        // 手动把 cached_at 拨到 5 分钟前 → 超过 90s 窗口,不新鲜。
        if let Some(entry) = cache.entries.get_mut("claude::a@x.com") {
            entry.cached_at = Utc::now() - Duration::minutes(5);
        }
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(90))
            .is_none());
    }

    #[test]
    fn save_roundtrips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        cache.record_failure("claude", "b@x.com", "429 rate limited");
        cache.save(&path);

        let loaded = QuotaCache::load(&path);
        assert!(loaded.get("claude", "a@x.com").is_some());
        assert_eq!(
            loaded
                .in_failure_backoff(
                    "claude",
                    "b@x.com",
                    std::time::Duration::from_secs(90),
                    std::time::Duration::from_secs(900)
                )
                .map(|f| f.error.as_str()),
            Some("429 rate limited")
        );
    }

    #[test]
    fn save_to_missing_parent_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing").join("quota_cache.json");
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        cache.save(&path);
        assert!(!path.exists());
    }

    #[test]
    fn set_at_preserves_observation_timestamp() {
        let mut cache = QuotaCache::default();
        let observed_at = Utc::now() - Duration::seconds(120);
        cache.set_at("claude", "a@x.com", vec![sample_quota()], observed_at);
        assert_eq!(
            cache.get("claude", "a@x.com").unwrap().cached_at,
            observed_at
        );
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(300))
            .is_some());
        assert!(cache
            .fresh("claude", "a@x.com", std::time::Duration::from_secs(60))
            .is_none());
    }

    #[test]
    fn save_merges_unrelated_entries_from_two_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");

        let mut first = QuotaCache::load(&path);
        let mut second = QuotaCache::load(&path);
        first.set("claude", "a@x.com", vec![sample_quota()]);
        first.save(&path);
        second.set("claude", "b@x.com", vec![sample_quota()]);
        second.save(&path);

        let loaded = QuotaCache::load(&path);
        assert!(loaded.get("claude", "a@x.com").is_some());
        assert!(loaded.get("claude", "b@x.com").is_some());
    }

    #[test]
    fn save_keeps_newer_observation_from_other_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");
        let t_old = Utc::now() - Duration::minutes(10);
        let t_new = Utc::now();

        let mut stale = QuotaCache::load(&path);
        let mut fresher = QuotaCache::load(&path);
        fresher.set_at("claude", "a@x.com", vec![quota_with_used(80)], t_new);
        fresher.save(&path);

        stale.set_at("claude", "a@x.com", vec![quota_with_used(10)], t_old);
        stale.save(&path);

        let loaded = QuotaCache::load(&path);
        let entry = loaded.get("claude", "a@x.com").unwrap();
        assert_eq!(entry.cached_at, t_new);
        assert_eq!(entry.quotas[0].used, 80);
    }

    #[test]
    fn removal_tombstone_blocks_stale_resurrection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");
        let stale_observed = Utc::now() - Duration::minutes(5);

        let mut seed = QuotaCache::load(&path);
        seed.set_at("cursor", "auth0|gone", vec![sample_quota()], stale_observed);
        seed.save(&path);

        // 两个进程都在 rm 之前加载了旧缓存。
        let mut stale = QuotaCache::load(&path);
        let mut remover = QuotaCache::load(&path);
        remover.remove("cursor", "auth0|gone");
        remover.save(&path);

        // 旧进程用删除之前的观测再写一次，不得复活。
        stale.set_at(
            "cursor",
            "auth0|gone",
            vec![quota_with_used(99)],
            stale_observed,
        );
        stale.save(&path);

        let loaded = QuotaCache::load(&path);
        assert!(loaded.get("cursor", "auth0|gone").is_none());
        assert!(loaded.failure("cursor", "auth0|gone").is_none());
        assert!(loaded.removals.contains_key("cursor::auth0|gone"));
    }

    #[test]
    fn newer_observation_after_removal_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");

        let mut remover = QuotaCache::load(&path);
        remover.remove("cursor", "auth0|gone");
        remover.save(&path);

        // 重新导入/重新登录后的新观测晚于墓碑，是合法新数据。
        let mut later = QuotaCache::load(&path);
        later.set_at(
            "cursor",
            "auth0|gone",
            vec![sample_quota()],
            Utc::now() + Duration::seconds(1),
        );
        later.save(&path);

        let loaded = QuotaCache::load(&path);
        assert!(loaded.get("cursor", "auth0|gone").is_some());
    }

    #[test]
    fn save_preserves_other_handle_failure_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");

        let mut failing = QuotaCache::load(&path);
        failing.record_failure("claude", "a@x.com", "429 rate limited");
        failing.save(&path);

        let mut other = QuotaCache::load(&path);
        other.set("claude", "b@x.com", vec![sample_quota()]);
        other.save(&path);

        let loaded = QuotaCache::load(&path);
        assert_eq!(
            loaded
                .failure("claude", "a@x.com")
                .map(|f| f.error.as_str()),
            Some("429 rate limited")
        );
    }

    #[test]
    fn older_success_does_not_clear_newer_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");

        let mut failing = QuotaCache::load(&path);
        failing.record_failure("claude", "a@x.com", "429 rate limited");
        failing.save(&path);

        let mut stale = QuotaCache::load(&path);
        stale.set_at(
            "claude",
            "a@x.com",
            vec![sample_quota()],
            Utc::now() - Duration::minutes(5),
        );
        stale.save(&path);

        let loaded = QuotaCache::load(&path);
        assert!(loaded.failure("claude", "a@x.com").is_some());
    }

    #[test]
    fn newer_success_clears_older_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");

        let mut failing = QuotaCache::load(&path);
        failing.record_failure("claude", "a@x.com", "429 rate limited");
        failing.save(&path);

        let mut healed = QuotaCache::load(&path);
        healed.set_at(
            "claude",
            "a@x.com",
            vec![sample_quota()],
            Utc::now() + Duration::seconds(1),
        );
        healed.save(&path);

        let loaded = QuotaCache::load(&path);
        assert!(loaded.failure("claude", "a@x.com").is_none());
        assert!(loaded.get("claude", "a@x.com").is_some());
    }

    #[test]
    fn read_only_save_does_not_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("quota_cache.json");
        let mut seed = QuotaCache::load(&path);
        seed.set("claude", "a@x.com", vec![sample_quota()]);
        seed.save(&path);

        // 加载后删除文件；只读实例的 save 不应重新创建它。
        let mut loaded = QuotaCache::load(&path);
        fs::remove_file(&path).unwrap();
        loaded.save(&path);
        assert!(!path.exists());
    }

    #[test]
    fn current_quotas_drops_reset_windows() {
        let observed_at = Utc::now() - Duration::minutes(1);
        let now = Utc::now();
        let future = Quota {
            reset_at: Some(now + Duration::hours(1)),
            ..sample_quota()
        };
        let expired = Quota {
            window: QuotaWindow::FiveHour,
            reset_at: Some(now - Duration::seconds(1)),
            ..sample_quota()
        };
        let kept = current_quotas(&[future, expired], observed_at, now);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].window, QuotaWindow::SevenDay);
    }

    #[test]
    fn dirty_markers_are_not_serialized() {
        let mut cache = QuotaCache::default();
        cache.set("claude", "a@x.com", vec![sample_quota()]);
        let json = serde_json::to_string(&cache).unwrap();
        assert!(!json.contains("dirty_"));
        let loaded: QuotaCache = serde_json::from_str(&json).unwrap();
        assert!(!loaded.is_dirty());
    }
}
