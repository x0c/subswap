//! 账号 checkout 锁：环境隔离启动（`subswap run`）时给账号加独占占用。
//!
//! 设计见 [docs/design/ACCOUNT_ISOLATION_DESIGN.md]。核心不变量：**同一账号同一时刻只能被一个
//! 隔离环境借走**。否则两个原生客户端会从同一份 refresh token 各自轮换，必有一方被服务端作废
//! （`refresh token already used`）。
//!
//! 实现用 `fs2` 文件锁：`subswap run` 进程在整个子进程生命周期内持有该账号 `.lock` 的独占锁。
//! 选文件锁而非 PID 文件的理由——**进程崩溃 / 被强杀时操作系统自动释放锁**，从根上避免陈旧锁泄漏。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::{Error, Result};

/// 一次账号占用。持有期间锁生效；`Drop` 时释放（关闭文件即解锁，显式 unlock 只为清晰）。
pub struct Checkout {
    provider: String,
    id: String,
    lock_file: File,
    env_dir: PathBuf,
}

impl Checkout {
    /// 占用 `(provider, id)`：在 `<data_dir>/checkouts/` 下取独占文件锁，并准备
    /// `<data_dir>/envs/<provider>/<id>/` 隔离目录（`0700`）。
    ///
    /// 锁已被其他存活进程持有时返回 [`Error::Provider`]，提示该账号正被另一个隔离会话使用。
    pub fn acquire(data_dir: &Path, provider: &str, id: &str) -> Result<Self> {
        let checkouts_dir = data_dir.join("checkouts");
        std::fs::create_dir_all(&checkouts_dir)?;

        let lock_path = checkouts_dir.join(format!("{provider}__{}.lock", sanitize(id)));
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;

        match lock_file.try_lock_exclusive() {
            Ok(()) => {}
            Err(_) => {
                return Err(Error::Provider(format!(
                    "{provider}/{id} is already checked out by another isolated session; \
                     close it first or pick another account"
                )));
            }
        }

        let env_dir = env_dir(data_dir, provider, id);
        std::fs::create_dir_all(&env_dir)?;
        harden_dir(&env_dir);

        Ok(Self {
            provider: provider.to_string(),
            id: id.to_string(),
            lock_file,
            env_dir,
        })
    }

    /// 隔离环境私有目录（`CODEX_HOME` / `CLAUDE_CONFIG_DIR` 指向它）。
    pub fn env_dir(&self) -> &Path {
        &self.env_dir
    }

    /// 被占用的账号 `(provider, id)`。
    pub fn account(&self) -> (&str, &str) {
        (&self.provider, &self.id)
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

/// 该账号当前是否正被某个隔离会话占用（daemon 保活据此跳过，避免抢刷活账号 token）。
///
/// 用「非阻塞独占锁探测」实现：能立刻拿到锁说明无人持有（随即释放）；拿不到说明被占用。
pub fn is_checked_out(data_dir: &Path, provider: &str, id: &str) -> bool {
    let lock_path = data_dir
        .join("checkouts")
        .join(format!("{provider}__{}.lock", sanitize(id)));
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
        // 锁文件不存在 → 从未被占用。
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// 隔离环境私有目录路径：`<data_dir>/envs/<provider>/<sanitized id>/`。
pub fn env_dir(data_dir: &Path, provider: &str, id: &str) -> PathBuf {
    data_dir.join("envs").join(provider).join(sanitize(id))
}

/// 把账号 id 清洗成安全文件名：保留字母数字与 `._@-`，其余（含路径分隔符）转 `_`。
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(unix)]
fn harden_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_same_account_fails_while_held() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Checkout::acquire(tmp.path(), "codex", "a@x.com").unwrap();
        assert!(is_checked_out(tmp.path(), "codex", "a@x.com"));

        let second = Checkout::acquire(tmp.path(), "codex", "a@x.com");
        assert!(
            second.is_err(),
            "second checkout must fail while first held"
        );

        drop(first);
        // 释放后可重新占用。
        let third = Checkout::acquire(tmp.path(), "codex", "a@x.com");
        assert!(third.is_ok());
    }

    #[test]
    fn different_accounts_acquire_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Checkout::acquire(tmp.path(), "codex", "a@x.com").unwrap();
        let b = Checkout::acquire(tmp.path(), "codex", "b@x.com").unwrap();
        assert_ne!(a.env_dir(), b.env_dir());
    }

    #[test]
    fn not_checked_out_when_no_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_checked_out(tmp.path(), "codex", "never@x.com"));
    }

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(sanitize("a/b\\c"), "a_b_c");
        assert_eq!(sanitize("user@host.com"), "user@host.com");
    }
}
