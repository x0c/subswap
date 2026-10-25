//! 与 Node `proper-lockfile` 互斥兼容的目录锁。
//!
//! 原生客户端（Kimi TS、Claude Code）用 `mkdir <lockfile>` 抢锁，持有期间周期性刷新目录 mtime，
//! 超过 `stale` 未刷新视为死锁可被回收。subswap 复用同一约定才能与官方刷新互斥，
//! 不自创一套互不相认的锁。

use std::fs::File;
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
    /// 创建时打开的目录句柄。持有它会让 Unix inode / Windows file ID 在锁被删除后
    /// 仍然保持分配，防止同名重建的目录复用旧身份。
    held: Option<Arc<File>>,
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
        !self.lost.load(Ordering::SeqCst) && owns(&self.path, self.held.as_deref(), self.identity)
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
        if owns(&self.path, self.held.as_deref(), self.identity) {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

#[cfg(not(windows))]
fn identity_of_metadata(metadata: &std::fs::Metadata) -> Option<Identity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let created = metadata
            .created()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?;
        Some((created.as_secs(), u64::from(created.subsec_nanos())))
    }
}

/// `BY_HANDLE_FILE_INFORMATION` 的稳定子集：卷序列号 + 文件索引。
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    #[link_name = "GetFileInformationByHandle"]
    fn get_file_information_by_handle(
        file: *mut std::ffi::c_void,
        info: *mut ByHandleFileInformation,
    ) -> i32;
}

#[cfg(windows)]
fn identity_of_handle(file: &File) -> Option<Identity> {
    use std::os::windows::io::AsRawHandle;
    let mut info = ByHandleFileInformation::default();
    let ok = unsafe {
        get_file_information_by_handle(file.as_raw_handle() as *mut std::ffi::c_void, &mut info)
    };
    (ok != 0).then(|| {
        (
            u64::from(info.volume_serial_number),
            (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
        )
    })
}

#[cfg(unix)]
fn identity_of_handle(file: &File) -> Option<Identity> {
    identity_of_metadata(&file.metadata().ok()?)
}

#[cfg(not(any(unix, windows)))]
fn identity_of_handle(file: &File) -> Option<Identity> {
    identity_of_metadata(&file.metadata().ok()?)
}

fn identity_of_path(path: &Path) -> Option<Identity> {
    #[cfg(windows)]
    {
        // 用同一个目录句柄读取 file index，避免 metadata 与句柄身份口径不一致。
        identity_of_handle(&open_dir_handle(path).ok()?)
    }
    #[cfg(not(windows))]
    {
        identity_of_metadata(&std::fs::metadata(path).ok()?)
    }
}

/// 打开目录句柄。Unix 上普通 `File::open` 即可；Windows 目录必须带
/// `FILE_FLAG_BACKUP_SEMANTICS`，并允许共享删除，避免破坏官方客户端的 rmdir 协议。
fn open_dir_handle(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        File::open(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory handles unavailable",
        ))
    }
}

/// 持有目录句柄；不支持的平台返回 None，退回旧的身份检查。
fn hold_directory(path: &Path) -> std::io::Result<Option<Arc<File>>> {
    #[cfg(any(unix, windows))]
    {
        Ok(Some(Arc::new(open_dir_handle(path)?)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(None)
    }
}

/// 身份未知（平台不提供）时无法区分重建目录，退回按路径持有。
///
/// 支持目录句柄的平台必须同时满足：句柄自身仍是原 inode/file ID，且当前路径仍是
/// 同一身份。句柄阻止身份被复用，因此 Linux 上 `unlink` 后同 inode 复用不会误判。
fn owns(path: &Path, held: Option<&File>, identity: Option<Identity>) -> bool {
    let Some(expected) = identity else {
        return path.exists();
    };
    #[cfg(any(unix, windows))]
    {
        let Some(held) = held else {
            return false;
        };
        identity_of_handle(held) == Some(expected) && identity_of_path(path) == Some(expected)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = held;
        identity_of_path(path) == Some(expected)
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
    let held = match hold_directory(lock_dir) {
        Ok(held) => held,
        Err(error) => {
            let _ = std::fs::remove_dir(lock_dir);
            return Err(error.into());
        }
    };
    let identity = held
        .as_deref()
        .and_then(identity_of_handle)
        .or_else(|| identity_of_path(lock_dir));
    if held.is_some() && identity.is_none() {
        let _ = std::fs::remove_dir(lock_dir);
        return Err(std::io::Error::other("cannot prove lock directory identity").into());
    }
    let lost = Arc::new(AtomicBool::new(false));
    let (stop, heartbeat) =
        start_heartbeat(lock_dir.to_path_buf(), held.clone(), identity, lost.clone());
    Ok(Some(ProperLock {
        path: lock_dir.to_path_buf(),
        held,
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
    held: Option<Arc<File>>,
    identity: Option<Identity>,
    lost: Arc<AtomicBool>,
) -> (Sender<()>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || loop {
        match receiver.recv_timeout(HEARTBEAT_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !owns(&path, held.as_deref(), identity) {
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
    fn replacement_lock_survives_old_holder_heartbeat_and_drop() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".lock");
        let first = acquire(&dir, Duration::from_secs(60), Duration::ZERO)
            .unwrap()
            .unwrap();
        // 模拟旧锁被回收后，另一个真实持有者立即同名重建。
        std::fs::remove_dir(&dir).unwrap();
        let second = acquire(&dir, Duration::from_secs(60), Duration::ZERO)
            .unwrap()
            .unwrap();

        std::thread::sleep(HEARTBEAT_INTERVAL + Duration::from_millis(300));
        assert!(!first.still_held());
        assert!(second.still_held());

        drop(first);
        assert!(dir.exists(), "old holder deleted the replacement lock");
        assert!(second.still_held());

        drop(second);
        assert!(!dir.exists(), "current holder did not release its lock");
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
