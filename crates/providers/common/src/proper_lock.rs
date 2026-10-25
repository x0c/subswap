//! 与 Node `proper-lockfile` 互斥兼容的目录锁。
//!
//! 原生客户端（Kimi TS、Claude Code）用 `mkdir <lockfile>` 抢锁，持有期间周期性刷新目录 mtime，
//! 超过 `stale` 未刷新视为死锁可被回收。subswap 复用同一约定才能与官方刷新互斥，
//! 不自创一套互不相认的锁。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use subswap_core::error::Result;

const RETRY_INTERVAL: Duration = Duration::from_millis(50);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const STALE_RECHECK_INTERVAL: Duration = Duration::from_millis(100);

/// 锁目录的身份：被回收后同名重建的目录不是同一把锁。
type Identity = (u64, u64);

/// 持有中的目录锁。Drop 时停止续租，并只删除仍属于自己的目录。
///
/// 持有者若被判 stale 回收、别人同名重建，旧持有者既不续租也不删除新目录，只标记失锁。
pub struct ProperLock {
    path: PathBuf,
    identity: Option<Identity>,
    lost: Arc<AtomicBool>,
    stop_heartbeat: Option<Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
}

impl ProperLock {
    /// 锁目录路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 锁目录是否仍是本次创建的那一个。
    pub fn still_held(&self) -> bool {
        !self.lost.load(Ordering::SeqCst) && owns(&self.path, self.identity)
    }
}

impl Drop for ProperLock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop_heartbeat.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if owns(&self.path, self.identity) {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

fn identity_of(path: &Path) -> Option<Identity> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let created = metadata
            .created()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?;
        Some((created.as_secs(), u64::from(created.subsec_nanos())))
    }
}

/// 身份未知（平台不提供）时无法区分重建目录，退回按路径持有。
fn owns(path: &Path, identity: Option<Identity>) -> bool {
    match identity {
        Some(expected) => identity_of(path) == Some(expected),
        None => path.exists(),
    }
}

/// 在 `wait_timeout` 内抢占 `lock_dir`；超时返回 `Ok(None)`。阻塞调用，async 上下文须包进
/// `spawn_blocking`。
///
/// 超过 `stale_after` 未续租、且复核期间 mtime 未变的锁才原子改名回收；mtime 在未来的锁
/// 永不视为过期（时钟回拨场景下宁可超时，也不抢走可能仍在刷新的持有者）。
pub fn acquire(
    lock_dir: &Path,
    stale_after: Duration,
    wait_timeout: Duration,
) -> Result<Option<ProperLock>> {
    if let Some(parent) = lock_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let deadline = Instant::now() + wait_timeout;
    let mut stale_candidate = None;
    loop {
        match std::fs::create_dir(lock_dir) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if try_reap_stale(lock_dir, stale_after, &mut stale_candidate)? {
                    continue;
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(RETRY_INTERVAL);
            }
            Err(error) => return Err(error.into()),
        }
    }
    let identity = identity_of(lock_dir);
    let lost = Arc::new(AtomicBool::new(false));
    let (stop, heartbeat) = start_heartbeat(lock_dir.to_path_buf(), identity, lost.clone());
    Ok(Some(ProperLock {
        path: lock_dir.to_path_buf(),
        identity,
        lost,
        stop_heartbeat: Some(stop),
        heartbeat: Some(heartbeat),
    }))
}

fn try_reap_stale(
    lock_dir: &Path,
    stale_after: Duration,
    candidate: &mut Option<(SystemTime, Instant)>,
) -> Result<bool> {
    let metadata = match std::fs::metadata(lock_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let modified = metadata.modified()?;
    let stale = SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > stale_after);
    if !stale {
        *candidate = None;
        return Ok(false);
    }
    match candidate {
        Some((observed, since)) if *observed == modified => {
            if since.elapsed() < STALE_RECHECK_INTERVAL {
                return Ok(false);
            }
        }
        _ => {
            *candidate = Some((modified, Instant::now()));
            return Ok(false);
        }
    }
    if std::fs::metadata(lock_dir)?.modified()? != modified {
        *candidate = None;
        return Ok(false);
    }
    let unique = format!(
        "{}.subswap-reap-{}-{}",
        lock_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("lock"),
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let reap_path = lock_dir.with_file_name(unique);
    match std::fs::rename(lock_dir, &reap_path) {
        Ok(()) => {
            std::fs::remove_dir_all(reap_path)?;
            *candidate = None;
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
            ) =>
        {
            *candidate = None;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn start_heartbeat(
    path: PathBuf,
    identity: Option<Identity>,
    lost: Arc<AtomicBool>,
) -> (Sender<()>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || loop {
        match receiver.recv_timeout(HEARTBEAT_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !owns(&path, identity) {
                    lost.store(true, Ordering::SeqCst);
                    break;
                }
                let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
            }
        }
    });
    (sender, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_times_out_until_first_is_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".lock");
        let first = acquire(&dir, Duration::from_secs(60), Duration::ZERO)
            .unwrap()
            .unwrap();
        assert!(
            acquire(&dir, Duration::from_secs(60), Duration::from_millis(120))
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(!dir.exists());
        assert!(acquire(&dir, Duration::from_secs(60), Duration::ZERO)
            .unwrap()
            .is_some());
    }

    #[test]
    fn stale_lock_is_reaped_but_future_dated_lock_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".lock");
        std::fs::create_dir(&dir).unwrap();
        let old =
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(120));
        filetime::set_file_mtime(&dir, old).unwrap();
        assert!(
            acquire(&dir, Duration::from_secs(60), Duration::from_secs(2))
                .unwrap()
                .is_some()
        );

        let future_dir = temp.path().join(".future.lock");
        std::fs::create_dir(&future_dir).unwrap();
        let future =
            filetime::FileTime::from_system_time(SystemTime::now() + Duration::from_secs(4 * 3600));
        filetime::set_file_mtime(&future_dir, future).unwrap();
        assert!(acquire(
            &future_dir,
            Duration::from_secs(60),
            Duration::from_millis(300)
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn reaped_holder_never_touches_or_deletes_replacement_lock() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".lock");
        let first = acquire(&dir, Duration::from_secs(60), Duration::ZERO)
            .unwrap()
            .unwrap();
        // 模拟 first 被判 stale 回收后，另一持有者同名重建。
        std::fs::remove_dir(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        let old = filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(30));
        filetime::set_file_mtime(&dir, old).unwrap();
        std::thread::sleep(HEARTBEAT_INTERVAL + Duration::from_millis(300));
        assert!(!first.still_held());
        drop(first);
        assert!(dir.exists(), "old holder deleted the replacement lock");
        let mtime = std::fs::metadata(&dir).unwrap().modified().unwrap();
        assert_eq!(filetime::FileTime::from_system_time(mtime), old);
    }
}
