//! active 账号凭证过期时，在 Claude Code 官方刷新锁内续期。
//!
//! 协议取自已安装 Claude Code 2.1.310 bundle（见 docs/PROVIDER_KNOWLEDGE_BASE.md
//! 「Active credential renewal under the official lock」）：
//! - 依次持有 `<dir>/.oauth_refresh.lock` 与旧版 `<realpath(dir)>.lock` 两把 proper-lockfile 目录锁；
//! - 持锁后重读凭证，发现别人已轮换就直接采用；
//! - 刷新后按「存储里的 refresh token 仍是本次提交的那枚」做比较写入，否则采用更新的写入。
//!
//! 官方运行中的实例刷新前会清缓存重读凭证，因此 subswap 持同一把锁写回的新令牌会被采用，
//! 而不是互相作废一次性 refresh token。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use subswap_core::error::{Error, Result};
use subswap_provider_common::proper_lock;

use crate::claude_files::CredentialsFile;
use crate::oauth::RefreshResponse;

/// 已从 bundle 核实锁协议的最低 Claude Code 版本。
const MIN_LOCK_PROTOCOL_VERSION: (u64, u64, u64) = (2, 1, 310);
/// 与官方 proper-lockfile `stale: 60000` 一致。
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60);
/// 官方遇锁最多重试 5 次、每次 1~2 秒；subswap 等待同一量级后放弃本轮。
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(12);
/// 与官方刷新请求的 30 秒超时一致。
pub(crate) const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// 官方锁目录：`CLAUDE_SECURESTORAGE_CONFIG_DIR` 优先，否则为 Claude 配置目录。
pub(crate) struct LockPaths {
    primary: PathBuf,
    legacy: PathBuf,
    wait: Duration,
}

impl LockPaths {
    pub(crate) fn for_claude_home(claude_home: &Path) -> Self {
        let dir = std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| claude_home.to_path_buf());
        let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let mut legacy = real.into_os_string();
        legacy.push(".lock");
        Self {
            primary: dir.join(".oauth_refresh.lock"),
            legacy: PathBuf::from(legacy),
            wait: LOCK_WAIT_TIMEOUT,
        }
    }
}

/// live 凭证的实际存储（macOS 钥匙串 / 其他平台 `.credentials.json`）与当前账号身份。
pub(crate) trait LiveBackend {
    fn account_email(&self) -> Result<Option<String>>;
    fn read(&self) -> Result<Option<CredentialsFile>>;
    fn write(&self, creds: &CredentialsFile) -> Result<()>;
}

/// 本机 `claude --version` 是否声明了已核实的官方刷新锁协议。进程内缓存一次探测结果。
pub(crate) fn lock_protocol_available() -> bool {
    // 单元测试永不探测本机 claude，也就不会走到真实钥匙串与刷新端点。
    if cfg!(test) {
        return false;
    }
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        Command::new("claude")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| version_supported(&String::from_utf8_lossy(&output.stdout)))
    })
}

fn version_supported(output: &str) -> bool {
    output
        .split_whitespace()
        .next()
        .and_then(parse_version)
        .is_some_and(|version| version >= MIN_LOCK_PROTOCOL_VERSION)
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

/// 在官方锁内取得可用的 active 凭证：别人已轮换则采用，否则刷新一次并比较写入。
///
/// `attempted_access` 是本次失败（过期或 usage 401）的 access token。
/// `refresh` 执行一次 OAuth 刷新；调用方保证它不会被外层取消。
pub(crate) fn renew_blocking(
    backend: &dyn LiveBackend,
    paths: &LockPaths,
    account: &str,
    attempted_access: &str,
    dead_refresh: &Mutex<HashSet<String>>,
    refresh: impl FnOnce(&CredentialsFile) -> Result<RefreshResponse>,
) -> Result<CredentialsFile> {
    let Some(primary) = proper_lock::acquire(&paths.primary, LOCK_STALE_AFTER, paths.wait)? else {
        return adopt_after_contention(backend, attempted_access);
    };
    let Some(legacy) = proper_lock::acquire(&paths.legacy, LOCK_STALE_AFTER, paths.wait)? else {
        return adopt_after_contention(backend, attempted_access);
    };
    // 两把锁按声明逆序释放：先旧版锁、后主锁，与官方 release 顺序一致。

    if backend.account_email()?.as_deref() != Some(account) {
        return Err(Error::QuotaFetch(
            "Claude account changed before credential renewal".into(),
        ));
    }
    let live = backend.read()?.ok_or_else(|| missing_live_error(account))?;
    if live.oauth.access_token != attempted_access && !super::is_expired_or_soon(&live, 0) {
        return Ok(live);
    }
    let posted = live
        .oauth
        .refresh_token
        .clone()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| missing_live_error(account))?;
    let fingerprint = super::refresh_fingerprint(&posted);
    let dead = dead_refresh
        .lock()
        .map(|set| set.contains(&fingerprint))
        .unwrap_or(false);
    if dead || super::refresh_token_expired(&live) {
        return Err(relogin_error(account));
    }
    let response = match refresh(&live) {
        Ok(response) => response,
        Err(error) if super::is_invalid_grant(&error) => {
            if let Ok(mut set) = dead_refresh.lock() {
                set.insert(fingerprint);
            }
            return Err(relogin_error(account));
        }
        Err(error) => return Err(error),
    };

    // 比较写入：存储里的 refresh token 已不是本次提交的那枚，说明有更新的写入，采用它。
    let mut current = backend.read()?.ok_or_else(|| missing_live_error(account))?;
    if current.oauth.refresh_token.as_deref() != Some(posted.as_str()) {
        return Ok(current);
    }
    if !primary.still_held() || !legacy.still_held() {
        // 与官方一致：失锁后仍做比较写入，丢弃已轮换的新令牌会让所有客户端掉登录。
        tracing::warn!(
            account,
            "Claude refresh lock lost during renewal; saving with compare-and-swap"
        );
    }
    super::apply_refresh_response(&mut current, response);
    backend.write(&current)?;
    tracing::info!(
        account,
        "Claude active credential renewed under official lock"
    );
    Ok(current)
}

/// 锁被占用到超时：持有者通常正在刷新，重读一次采用其结果；仍是旧令牌则本轮放弃。
fn adopt_after_contention(
    backend: &dyn LiveBackend,
    attempted_access: &str,
) -> Result<CredentialsFile> {
    if let Some(live) = backend.read()? {
        if live.oauth.access_token != attempted_access && !super::is_expired_or_soon(&live, 0) {
            return Ok(live);
        }
    }
    Err(Error::QuotaFetch(
        "Claude Code is renewing its login; usage will refresh shortly".into(),
    ))
}

fn missing_live_error(account: &str) -> Error {
    Error::QuotaFetch(format!(
        "re-login required for claude:{account}; live credential incomplete, log in again in Claude Code"
    ))
}

fn relogin_error(account: &str) -> Error {
    Error::QuotaFetch(format!(
        "re-login required for claude:{account}; refresh token invalid, log in again in Claude Code"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_files::ClaudeOauth;
    use chrono::Utc;
    use std::cell::RefCell;

    struct MemoryBackend {
        email: Option<String>,
        creds: RefCell<Option<CredentialsFile>>,
        writes: RefCell<u32>,
    }

    impl LiveBackend for MemoryBackend {
        fn account_email(&self) -> Result<Option<String>> {
            Ok(self.email.clone())
        }
        fn read(&self) -> Result<Option<CredentialsFile>> {
            Ok(self.creds.borrow().clone())
        }
        fn write(&self, creds: &CredentialsFile) -> Result<()> {
            *self.writes.borrow_mut() += 1;
            *self.creds.borrow_mut() = Some(creds.clone());
            Ok(())
        }
    }

    fn creds(access: &str, refresh: &str, expires_in_ms: i64) -> CredentialsFile {
        let mut other = serde_json::Map::new();
        other.insert("subscriptionType".into(), "pro".into());
        CredentialsFile {
            oauth: ClaudeOauth {
                access_token: access.into(),
                refresh_token: Some(refresh.into()),
                expires_at: Some(Utc::now().timestamp_millis() + expires_in_ms),
                scopes: vec!["user:inference".into()],
                other,
            },
            other: serde_json::Map::new(),
        }
    }

    fn backend(c: CredentialsFile) -> MemoryBackend {
        MemoryBackend {
            email: Some("a@x.com".into()),
            creds: RefCell::new(Some(c)),
            writes: RefCell::new(0),
        }
    }

    fn paths(dir: &Path) -> LockPaths {
        LockPaths {
            primary: dir.join("claude/.oauth_refresh.lock"),
            legacy: dir.join("claude.lock"),
            wait: Duration::from_millis(200),
        }
    }

    fn response(access: &str, refresh: &str) -> RefreshResponse {
        RefreshResponse {
            access_token: access.into(),
            expires_in: Some(28_800),
            refresh_token: Some(refresh.into()),
            refresh_token_expires_in: Some(30 * 86_400),
            scope: None,
        }
    }

    #[test]
    fn recognizes_verified_lock_protocol_versions_only() {
        assert!(version_supported("2.1.310 (Claude Code)\n"));
        assert!(version_supported("2.2.0 (Claude Code)"));
        assert!(!version_supported("2.1.296 (Claude Code)"));
        assert!(!version_supported("garbage"));
        assert!(!version_supported(""));
    }

    #[test]
    fn expired_active_is_refreshed_once_and_written_with_extra_fields() {
        let temp = tempfile::tempdir().unwrap();
        let live = backend(creds("old-access", "rt-1", -1_000));
        let dead = Mutex::new(HashSet::new());
        let mut calls = 0;
        let renewed = renew_blocking(
            &live,
            &paths(temp.path()),
            "a@x.com",
            "old-access",
            &dead,
            |c| {
                calls += 1;
                assert_eq!(c.oauth.refresh_token.as_deref(), Some("rt-1"));
                Ok(response("new-access", "rt-2"))
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(renewed.oauth.access_token, "new-access");
        assert_eq!(renewed.oauth.refresh_token.as_deref(), Some("rt-2"));
        assert!(!crate::is_expired_or_soon(&renewed, 60_000));
        assert_eq!(renewed.oauth.other["subscriptionType"], "pro");
        assert!(
            renewed.oauth.other["refreshTokenExpiresAt"]
                .as_i64()
                .unwrap()
                > Utc::now().timestamp_millis()
        );
        assert_eq!(*live.writes.borrow(), 1);
        // 两把锁都已释放。
        assert!(!temp.path().join("claude/.oauth_refresh.lock").exists());
        assert!(!temp.path().join("claude.lock").exists());
    }

    #[test]
    fn sibling_rotation_is_adopted_without_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let live = backend(creds("sibling-access", "rt-2", 3_600_000));
        let dead = Mutex::new(HashSet::new());
        let renewed = renew_blocking(
            &live,
            &paths(temp.path()),
            "a@x.com",
            "old-access",
            &dead,
            |_| panic!("must not refresh when a sibling already rotated"),
        )
        .unwrap();
        assert_eq!(renewed.oauth.access_token, "sibling-access");
        assert_eq!(*live.writes.borrow(), 0);
    }

    #[test]
    fn busy_official_lock_never_refreshes() {
        let temp = tempfile::tempdir().unwrap();
        let p = paths(temp.path());
        for held in [&p.primary, &p.legacy] {
            std::fs::create_dir_all(held).unwrap();
            let live = backend(creds("old-access", "rt-1", -1_000));
            let dead = Mutex::new(HashSet::new());
            let err = renew_blocking(&live, &p, "a@x.com", "old-access", &dead, |_| {
                panic!("must not refresh while Claude Code holds the lock")
            })
            .unwrap_err();
            assert!(err.to_string().contains("renewing"), "{err}");
            // 官方持有者在等待期间完成轮换 → 直接采用。
            *live.creds.borrow_mut() = Some(creds("sibling-access", "rt-2", 3_600_000));
            let adopted = renew_blocking(&live, &p, "a@x.com", "old-access", &dead, |_| {
                panic!("must not refresh while Claude Code holds the lock")
            })
            .unwrap();
            assert_eq!(adopted.oauth.access_token, "sibling-access");
            std::fs::remove_dir(held).unwrap();
        }
        // 旧版锁被占时，主锁必须已释放。
        assert!(!p.primary.exists());
    }

    #[test]
    fn invalid_grant_marks_dead_and_second_attempt_sends_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let live = backend(creds("old-access", "rt-1", -1_000));
        let dead = Mutex::new(HashSet::new());
        let err = renew_blocking(
            &live,
            &paths(temp.path()),
            "a@x.com",
            "old-access",
            &dead,
            |_| {
                Err(Error::QuotaFetch(
                    "refresh returned 400: invalid_grant".into(),
                ))
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("re-login"), "{err}");
        let err = renew_blocking(
            &live,
            &paths(temp.path()),
            "a@x.com",
            "old-access",
            &dead,
            |_| panic!("dead refresh token must not be posted again"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("re-login"), "{err}");
        // 绝不清空 live 凭证。
        assert_eq!(*live.writes.borrow(), 0);
        assert!(live.creds.borrow().is_some());
    }

    #[test]
    fn account_change_aborts_before_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let mut live = backend(creds("old-access", "rt-1", -1_000));
        live.email = Some("b@x.com".into());
        let dead = Mutex::new(HashSet::new());
        let err = renew_blocking(
            &live,
            &paths(temp.path()),
            "a@x.com",
            "old-access",
            &dead,
            |_| panic!("must not refresh another account's credential"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
    }

    #[test]
    fn newer_write_during_refresh_is_adopted_not_overwritten() {
        struct RacingBackend(MemoryBackend);
        impl LiveBackend for RacingBackend {
            fn account_email(&self) -> Result<Option<String>> {
                self.0.account_email()
            }
            fn read(&self) -> Result<Option<CredentialsFile>> {
                self.0.read()
            }
            fn write(&self, creds: &CredentialsFile) -> Result<()> {
                self.0.write(creds)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let live = RacingBackend(backend(creds("old-access", "rt-1", -1_000)));
        let dead = Mutex::new(HashSet::new());
        let renewed = renew_blocking(
            &live,
            &paths(temp.path()),
            "a@x.com",
            "old-access",
            &dead,
            |_| {
                // 模拟未持锁的写入者在刷新期间写入了更新的凭证。
                *live.0.creds.borrow_mut() = Some(creds("other-access", "rt-9", 3_600_000));
                Ok(response("new-access", "rt-2"))
            },
        )
        .unwrap();
        assert_eq!(renewed.oauth.access_token, "other-access");
        assert_eq!(*live.0.writes.borrow(), 0);
    }
}
