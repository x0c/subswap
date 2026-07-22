//! Kimi OAuth 刷新：POST {oauth_host}/api/oauth/token（form-urlencoded, grant_type=refresh_token）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fs2::FileExt;
use sha2::{Digest, Sha256};
use subswap_core::error::{Error, Result};
use subswap_core::Account;
use subswap_provider_common::{extract_access_token, extract_refresh_token, RefreshOutcome};

use crate::kimi_files::decode_jwt_payload;
use crate::{kimi_files, paths};

const MIN_COORDINATED_REFRESH_VERSION: (u64, u64, u64) = (1, 31, 0);
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const TYPESCRIPT_STALE_AFTER: Duration = Duration::from_secs(10);
const STALE_RECHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RefreshLockProtocol {
    PythonFile,
    TypeScriptDirectory,
}

/// 解析 OAuth host：`KIMI_CODE_OAUTH_HOST` > `https://auth.kimi.com`。
fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .unwrap_or_else(|_| "https://auth.kimi.com".into())
        .trim_end_matches('/')
        .to_string()
}

/// 从 blob 的 access_token JWT 里取 client_id（刷新请求需要）。
fn client_id_from_blob(blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    let token = v.get("access_token")?.as_str()?;
    decode_jwt_payload(token)?
        .get("client_id")?
        .as_str()
        .map(String::from)
}

/// 用 blob 里的 refresh_token 换新令牌，返回轮换后的完整 blob（合并回原 JSON 结构）。
pub async fn refresh_blob(blob: &str) -> Result<RefreshOutcome> {
    refresh_blob_at(blob, &oauth_host()).await
}

async fn refresh_blob_at(blob: &str, oauth_base: &str) -> Result<RefreshOutcome> {
    let Some(refresh) = extract_refresh_token(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };
    let Some(client_id) = client_id_from_blob(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };

    let url = format!("{}/api/oauth/token", oauth_base.trim_end_matches('/'));
    let form = [
        ("client_id", client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
    ];

    let client = reqwest::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::Provider(format!("kimi refresh client failed: {e}")))?;
    let resp = client
        .post(&url)
        .header("User-Agent", "subswap")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("kimi refresh request failed: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(RefreshOutcome::DeadToken);
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    if parsed.get("error").and_then(|v| v.as_str()) == Some("invalid_grant") {
        return Ok(RefreshOutcome::DeadToken);
    }
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "kimi refresh HTTP {status}: {body}"
        )));
    }
    let access = parsed.get("access_token").and_then(|v| v.as_str());
    let Some(access) = access else {
        return Err(Error::Provider(
            "kimi refresh response missing access_token".into(),
        ));
    };

    // 合并回原 blob 结构，保留未知字段。
    let mut merged: serde_json::Value = serde_json::from_str(blob).unwrap_or(serde_json::json!({}));
    let obj = merged.as_object_mut().unwrap();
    obj.insert(
        "access_token".into(),
        serde_json::Value::String(access.into()),
    );
    for key in ["refresh_token", "scope", "token_type", "expires_in"] {
        if let Some(v) = parsed.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    if let Some(exp) = parsed.get("expires_in").and_then(|v| v.as_i64()) {
        let now = chrono::Utc::now().timestamp();
        obj.insert("expires_at".into(), serde_json::Value::from(now + exp));
    }
    Ok(RefreshOutcome::Rotated(merged.to_string()))
}

/// active 账号用量查询 401 后，与官方 Kimi 协调一次令牌恢复。
///
/// Python 版 1.31+ 使用 flock 锁；TypeScript 版 0.x 在 Unix 使用 proper-lockfile 目录锁。
/// 不支持协调锁的平台、旧版或版本未知时宁可保留 401，也不争抢一次性 refresh token。
pub async fn recover_active_401(
    attempted_access_token: &str,
    account: &Account,
) -> Result<Option<String>> {
    let Some(protocol) = installed_kimi_refresh_lock_protocol().await else {
        tracing::warn!(
            "Kimi CLI refresh-lock protocol is unavailable; skip active token refresh to avoid refresh-token races"
        );
        return Ok(None);
    };
    recover_active_401_at(
        attempted_access_token,
        account,
        &paths::kimi_home(),
        &oauth_host(),
        protocol,
    )
    .await
}

async fn installed_kimi_refresh_lock_protocol() -> Option<RefreshLockProtocol> {
    tokio::task::spawn_blocking(|| {
        Command::new("kimi")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                lock_protocol_from_version_output(&stdout)
                    .or_else(|| lock_protocol_from_version_output(&stderr))
            })
            .and_then(|protocol| {
                protocol_with_lock_setting(
                    protocol,
                    std::env::var("KIMI_DISABLE_OAUTH_LOCK").as_deref() == Ok("1"),
                )
            })
    })
    .await
    .unwrap_or(None)
}

fn protocol_with_lock_setting(
    protocol: RefreshLockProtocol,
    lock_disabled: bool,
) -> Option<RefreshLockProtocol> {
    if lock_disabled && protocol == RefreshLockProtocol::TypeScriptDirectory {
        None
    } else {
        Some(protocol)
    }
}

fn lock_protocol_from_version_output(output: &str) -> Option<RefreshLockProtocol> {
    let trimmed = output.trim();
    let version = parse_version(trimmed)?;
    if version.0 == 0 {
        // 新 TypeScript 客户端输出裸 semver；旧 Python 0.x 带 kimi/kimi-cli/version 前缀。
        if trimmed.split_whitespace().count() != 1 || trimmed != format_version(version) {
            return None;
        }
    }
    lock_protocol_for_version(version)
}

fn format_version((major, minor, patch): (u64, u64, u64)) -> String {
    format!("{major}.{minor}.{patch}")
}

fn lock_protocol_for_version(version: (u64, u64, u64)) -> Option<RefreshLockProtocol> {
    if version.0 == 0 {
        #[cfg(not(windows))]
        return Some(RefreshLockProtocol::TypeScriptDirectory);
        #[cfg(windows)]
        return None;
    }
    (version.0 == 1 && version >= MIN_COORDINATED_REFRESH_VERSION)
        .then_some(RefreshLockProtocol::PythonFile)
}

fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    output.split_whitespace().find_map(|part| {
        let candidate = part.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let mut numbers = candidate.split('.');
        let major = numbers.next()?.parse().ok()?;
        let minor = numbers.next()?.parse().ok()?;
        let patch = numbers
            .next()
            .unwrap_or("0")
            .trim_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()?;
        Some((major, minor, patch))
    })
}

enum RefreshLockKind {
    File(File),
    Directory {
        path: PathBuf,
        stop_heartbeat: Sender<()>,
        heartbeat: JoinHandle<()>,
    },
}

struct RefreshLockGuard {
    kind: Option<RefreshLockKind>,
}

impl RefreshLockGuard {
    fn new(kind: RefreshLockKind) -> Self {
        Self { kind: Some(kind) }
    }
}

impl Drop for RefreshLockGuard {
    fn drop(&mut self) {
        let Some(kind) = self.kind.take() else {
            return;
        };
        match kind {
            RefreshLockKind::File(lock) => {
                let _ = FileExt::unlock(&lock);
            }
            RefreshLockKind::Directory {
                path,
                stop_heartbeat,
                heartbeat,
            } => {
                let _ = stop_heartbeat.send(());
                let _ = heartbeat.join();
                let _ = std::fs::remove_dir(path);
            }
        }
    }
}

struct LockedCredentials {
    guard: RefreshLockGuard,
    raw: String,
    dead_refresh_fingerprint: Option<String>,
}

pub(crate) async fn recover_active_401_at(
    attempted_access_token: &str,
    account: &Account,
    home: &Path,
    oauth_base: &str,
    protocol: RefreshLockProtocol,
) -> Result<Option<String>> {
    let credential_path = paths::active_cred_path(home);
    let Some(locked) =
        acquire_and_read(home.to_path_buf(), credential_path.clone(), protocol).await?
    else {
        tracing::warn!("Timed out waiting for Kimi credential refresh lock");
        return Ok(None);
    };

    let metadata = kimi_files::parse_metadata(&locked.raw);
    if metadata.primary_id.as_deref() != Some(account.id.0.as_str()) {
        release_lock(locked.guard).await;
        return Ok(None);
    }

    let latest_access = extract_access_token(&locked.raw);
    if latest_access
        .as_deref()
        .is_some_and(|token| token != attempted_access_token)
    {
        release_lock(locked.guard).await;
        return Ok(latest_access);
    }

    let current_refresh = extract_refresh_token(&locked.raw);
    let current_refresh_fingerprint = current_refresh.as_deref().map(refresh_fingerprint);
    if current_refresh_fingerprint.is_some()
        && current_refresh_fingerprint.as_deref() == locked.dead_refresh_fingerprint.as_deref()
    {
        release_lock(locked.guard).await;
        return Ok(None);
    }

    let outcome = refresh_blob_at(&locked.raw, oauth_base).await;
    match outcome {
        Ok(RefreshOutcome::Rotated(rotated)) => {
            let fresh_access = extract_access_token(&rotated);
            let write_result = write_credentials_and_release(
                locked.guard,
                credential_path,
                paths::dead_refresh_fingerprint_path(home),
                rotated,
            )
            .await;
            write_result?;
            Ok(fresh_access)
        }
        Ok(RefreshOutcome::DeadToken) => {
            if let Some(refresh) = current_refresh {
                write_dead_fingerprint_and_release(
                    locked.guard,
                    paths::dead_refresh_fingerprint_path(home),
                    refresh_fingerprint(&refresh),
                )
                .await?;
            } else {
                release_lock(locked.guard).await;
            }
            Ok(None)
        }
        Ok(RefreshOutcome::Unsupported) => {
            release_lock(locked.guard).await;
            Ok(None)
        }
        Err(error) => {
            release_lock(locked.guard).await;
            Err(error)
        }
    }
}

async fn acquire_and_read(
    home: PathBuf,
    credential_path: PathBuf,
    protocol: RefreshLockProtocol,
) -> Result<Option<LockedCredentials>> {
    acquire_and_read_with_timeout(home, credential_path, protocol, LOCK_WAIT_TIMEOUT).await
}

async fn acquire_and_read_with_timeout(
    home: PathBuf,
    credential_path: PathBuf,
    protocol: RefreshLockProtocol,
    wait_timeout: Duration,
) -> Result<Option<LockedCredentials>> {
    tokio::task::spawn_blocking(move || {
        let guard = RefreshLockGuard::new(match protocol {
            RefreshLockProtocol::PythonFile => {
                let lock_path = paths::credentials_lock_path(&home);
                if let Some(parent) = lock_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut lock = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(lock_path)?;
                if lock.metadata()?.len() == 0 {
                    lock.write_all(&[0])?;
                    lock.flush()?;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                let deadline = Instant::now() + wait_timeout;
                loop {
                    match lock.try_lock_exclusive() {
                        Ok(()) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return Ok(None);
                            }
                            std::thread::sleep(LOCK_RETRY_INTERVAL);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                RefreshLockKind::File(lock)
            }
            RefreshLockProtocol::TypeScriptDirectory => {
                let sentinel = paths::oauth_lock_sentinel_path(&home);
                let lock_dir = paths::oauth_lock_dir_path(&home);
                if let Some(parent) = sentinel.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(sentinel)?;
                let deadline = Instant::now() + wait_timeout;
                let mut stale_candidate = None;
                loop {
                    match std::fs::create_dir(&lock_dir) {
                        Ok(()) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            if try_reap_stale_directory(&lock_dir, &mut stale_candidate)? {
                                continue;
                            }
                            if Instant::now() >= deadline {
                                return Ok(None);
                            }
                            std::thread::sleep(LOCK_RETRY_INTERVAL);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                let (stop_heartbeat, heartbeat) = start_directory_heartbeat(lock_dir.clone());
                RefreshLockKind::Directory {
                    path: lock_dir,
                    stop_heartbeat,
                    heartbeat,
                }
            }
        });
        let dead_refresh_fingerprint =
            std::fs::read_to_string(paths::dead_refresh_fingerprint_path(&home))
                .ok()
                .map(|value| value.trim().to_string());
        match std::fs::read_to_string(credential_path) {
            Ok(raw) => Ok(Some(LockedCredentials {
                guard,
                raw,
                dead_refresh_fingerprint,
            })),
            Err(error) => {
                release_lock_blocking(guard);
                Err(error.into())
            }
        }
    })
    .await
    .map_err(|e| Error::Provider(format!("kimi credential lock task failed: {e}")))?
}

fn try_reap_stale_directory(
    lock_dir: &Path,
    candidate: &mut Option<(std::time::SystemTime, Instant)>,
) -> Result<bool> {
    let metadata = match std::fs::metadata(lock_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let modified = metadata.modified()?;
    if modified.elapsed().unwrap_or_default() <= TYPESCRIPT_STALE_AFTER {
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
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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

async fn release_lock(guard: RefreshLockGuard) {
    let _ = tokio::task::spawn_blocking(move || {
        release_lock_blocking(guard);
    })
    .await;
}

fn release_lock_blocking(guard: RefreshLockGuard) {
    drop(guard);
}

fn start_directory_heartbeat(path: PathBuf) -> (Sender<()>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
            }
        }
    });
    (sender, handle)
}

async fn write_credentials_and_release(
    guard: RefreshLockGuard,
    path: PathBuf,
    dead_fingerprint_path: PathBuf,
    contents: String,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let result = (|| {
            let parent = path.parent().ok_or_else(|| {
                Error::Provider("kimi credential path has no parent directory".into())
            })?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(contents.as_bytes())?;
            temporary.as_file().sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temporary
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            temporary
                .persist(&path)
                .map_err(|error| Error::Io(error.error))?;
            match std::fs::remove_file(dead_fingerprint_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(())
        })();
        release_lock_blocking(guard);
        result
    })
    .await
    .map_err(|e| Error::Provider(format!("kimi credential write task failed: {e}")))?
}

async fn write_dead_fingerprint_and_release(
    guard: RefreshLockGuard,
    path: PathBuf,
    fingerprint: String,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let result = atomic_private_write(&path, &fingerprint);
        release_lock_blocking(guard);
        result
    })
    .await
    .map_err(|e| Error::Provider(format!("kimi dead-token guard task failed: {e}")))?
}

fn atomic_private_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Provider("kimi guard path has no parent directory".into()))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

fn refresh_fingerprint(refresh_token: &str) -> String {
    format!("{:x}", Sha256::digest(refresh_token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use subswap_core::AccountId;

    #[test]
    fn recognizes_first_lock_capable_version() {
        assert_eq!(parse_version("kimi-cli 1.31.0"), Some((1, 31, 0)));
        assert_eq!(parse_version("kimi 1.49.0\n"), Some((1, 49, 0)));
        assert_eq!(parse_version("kimi-cli 0.28.1"), Some((0, 28, 1)));
        assert_eq!(lock_protocol_from_version_output("kimi-cli 0.28.1"), None);
        assert_eq!(lock_protocol_from_version_output("kimi 0.28.1"), None);
        assert_eq!(
            lock_protocol_for_version((1, 31, 0)),
            Some(RefreshLockProtocol::PythonFile)
        );
        assert_eq!(lock_protocol_for_version((1, 30, 9)), None);
        assert_eq!(lock_protocol_for_version((2, 0, 0)), None);
        assert_eq!(lock_protocol_for_version((3, 1, 0)), None);
        #[cfg(not(windows))]
        {
            assert_eq!(
                lock_protocol_from_version_output("0.28.1"),
                Some(RefreshLockProtocol::TypeScriptDirectory)
            );
            assert_eq!(
                protocol_with_lock_setting(RefreshLockProtocol::TypeScriptDirectory, true),
                None
            );
        }
        #[cfg(windows)]
        assert_eq!(lock_protocol_for_version((0, 28, 1)), None);
    }

    #[tokio::test]
    async fn waits_for_official_lock_then_reuses_rotated_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let lock_path = paths::credentials_lock_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let old_access = jwt("OLD");
        std::fs::write(&credential_path, blob(&old_access, "R1")).unwrap();

        let mut official_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        official_lock.write_all(&[0]).unwrap();
        official_lock.lock_exclusive().unwrap();

        let home = temporary.path().to_path_buf();
        let account = active_account();
        let attempted = old_access.clone();
        let recovery = tokio::spawn(async move {
            recover_active_401_at(
                &attempted,
                &account,
                &home,
                "http://127.0.0.1:1",
                RefreshLockProtocol::PythonFile,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!recovery.is_finished());

        let new_access = jwt("NEW");
        std::fs::write(&credential_path, blob(&new_access, "R2")).unwrap();
        official_lock.unlock().unwrap();

        assert_eq!(recovery.await.unwrap().unwrap(), Some(new_access));
        assert_eq!(
            extract_refresh_token(&std::fs::read_to_string(credential_path).unwrap()).as_deref(),
            Some("R2")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(paths::credentials_lock_path(temporary.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn waits_for_proper_lock_directory_then_reuses_rotated_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let sentinel_path = paths::oauth_lock_sentinel_path(temporary.path());
        let lock_dir = paths::oauth_lock_dir_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(sentinel_path.parent().unwrap()).unwrap();
        std::fs::write(&sentinel_path, "").unwrap();
        std::fs::create_dir(&lock_dir).unwrap();
        let old_access = jwt("OLD");
        std::fs::write(&credential_path, blob(&old_access, "R1")).unwrap();

        let home = temporary.path().to_path_buf();
        let account = active_account();
        let attempted = old_access.clone();
        let recovery = tokio::spawn(async move {
            recover_active_401_at(
                &attempted,
                &account,
                &home,
                "http://127.0.0.1:1",
                RefreshLockProtocol::TypeScriptDirectory,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!recovery.is_finished());

        let new_access = jwt("NEW");
        std::fs::write(&credential_path, blob(&new_access, "R2")).unwrap();
        std::fs::remove_dir(&lock_dir).unwrap();

        assert_eq!(recovery.await.unwrap().unwrap(), Some(new_access));
        assert!(sentinel_path.is_file());
        assert!(!lock_dir.exists());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn proper_lock_heartbeat_keeps_directory_fresh_beyond_stale_window() {
        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let lock_dir = paths::oauth_lock_dir_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        std::fs::write(&credential_path, blob(&jwt("OLD"), "R1")).unwrap();

        let locked = acquire_and_read(
            temporary.path().to_path_buf(),
            credential_path,
            RefreshLockProtocol::TypeScriptDirectory,
        )
        .await
        .unwrap()
        .unwrap();
        let initial_mtime = std::fs::metadata(&lock_dir).unwrap().modified().unwrap();
        tokio::time::sleep(Duration::from_millis(5_200)).await;
        let refreshed_mtime = std::fs::metadata(&lock_dir).unwrap().modified().unwrap();

        assert!(refreshed_mtime > initial_mtime);
        assert!(refreshed_mtime.elapsed().unwrap() < Duration::from_secs(3));
        assert_eq!(
            std::fs::create_dir(&lock_dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        release_lock(locked.guard).await;
        assert!(!lock_dir.exists());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn stale_proper_lock_is_atomically_reaped_but_fresh_lock_is_not() {
        let stale_home = tempfile::tempdir().unwrap();
        let stale_credentials = paths::active_cred_path(stale_home.path());
        let stale_lock = paths::oauth_lock_dir_path(stale_home.path());
        std::fs::create_dir_all(stale_credentials.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stale_lock.parent().unwrap()).unwrap();
        std::fs::write(&stale_credentials, blob(&jwt("OLD"), "R1")).unwrap();
        std::fs::create_dir(&stale_lock).unwrap();
        filetime::set_file_mtime(
            &stale_lock,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - Duration::from_secs(20),
            ),
        )
        .unwrap();
        let stale_acquired = acquire_and_read_with_timeout(
            stale_home.path().to_path_buf(),
            stale_credentials,
            RefreshLockProtocol::TypeScriptDirectory,
            Duration::from_secs(1),
        )
        .await
        .unwrap()
        .unwrap();
        release_lock(stale_acquired.guard).await;
        assert!(!stale_lock.exists());

        let fresh_home = tempfile::tempdir().unwrap();
        let fresh_credentials = paths::active_cred_path(fresh_home.path());
        let fresh_lock = paths::oauth_lock_dir_path(fresh_home.path());
        std::fs::create_dir_all(fresh_credentials.parent().unwrap()).unwrap();
        std::fs::create_dir_all(fresh_lock.parent().unwrap()).unwrap();
        std::fs::write(&fresh_credentials, blob(&jwt("OLD"), "R1")).unwrap();
        std::fs::create_dir(&fresh_lock).unwrap();
        let fresh_acquired = acquire_and_read_with_timeout(
            fresh_home.path().to_path_buf(),
            fresh_credentials,
            RefreshLockProtocol::TypeScriptDirectory,
            Duration::from_millis(250),
        )
        .await
        .unwrap();
        assert!(fresh_acquired.is_none());
        assert!(fresh_lock.is_dir());
        std::fs::remove_dir(fresh_lock).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn dead_refresh_fingerprint_prevents_repeated_http_requests() {
        use crate::test_support::MockServer;

        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let access = jwt("OLD");
        std::fs::write(&credential_path, blob(&access, "R1")).unwrap();
        let server = MockServer::start(vec![("401 Unauthorized", r#"{"error":"invalid_grant"}"#)]);

        assert!(recover_active_401_at(
            &access,
            &active_account(),
            temporary.path(),
            server.base_url(),
            RefreshLockProtocol::TypeScriptDirectory,
        )
        .await
        .unwrap()
        .is_none());
        assert!(recover_active_401_at(
            &access,
            &active_account(),
            temporary.path(),
            server.base_url(),
            RefreshLockProtocol::TypeScriptDirectory,
        )
        .await
        .unwrap()
        .is_none());

        assert_eq!(server.finish().len(), 1);
        let guard_path = paths::dead_refresh_fingerprint_path(temporary.path());
        let fingerprint = std::fs::read_to_string(&guard_path).unwrap();
        assert_eq!(fingerprint, refresh_fingerprint("R1"));
        assert!(!fingerprint.contains("R1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(guard_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn cancelling_refresh_releases_proper_lock_and_stops_heartbeat() {
        use std::io::Read;
        use std::net::TcpListener;

        let temporary = tempfile::tempdir().unwrap();
        let credential_path = paths::active_cred_path(temporary.path());
        let lock_dir = paths::oauth_lock_dir_path(temporary.path());
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let access = jwt("OLD");
        std::fs::write(&credential_path, blob(&access, "R1")).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let oauth_base = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            accepted_tx.send(()).unwrap();
            let _ = finish_rx.recv_timeout(Duration::from_secs(2));
        });
        let home = temporary.path().to_path_buf();
        let account = active_account();
        let recovery = tokio::spawn(async move {
            recover_active_401_at(
                &access,
                &account,
                &home,
                &oauth_base,
                RefreshLockProtocol::TypeScriptDirectory,
            )
            .await
        });
        tokio::task::spawn_blocking(move || accepted_rx.recv_timeout(Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
        assert!(lock_dir.is_dir());
        recovery.abort();
        assert!(recovery.await.unwrap_err().is_cancelled());
        assert!(!lock_dir.exists());
        finish_tx.send(()).unwrap();
        server.join().unwrap();
    }

    fn jwt(suffix: &str) -> String {
        format!("header.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEifQ.{suffix}")
    }

    fn blob(access: &str, refresh: &str) -> String {
        format!(r#"{{"access_token":"{access}","refresh_token":"{refresh}"}}"#)
    }

    fn active_account() -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId("u-123".into()),
            label: "u-123".into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }
}
