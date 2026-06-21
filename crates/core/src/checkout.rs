//! 账号 checkout：环境隔离启动（`subswap run`）时为每个并发会话准备独立私有目录。
//!
//! 设计见 [docs/design/ACCOUNT_ISOLATION_DESIGN.md]。
//! 同一账号可以并发开启多个隔离会话（不再有独占锁）；每个 `Checkout` 实例获得一个唯一的
//! `env_dir`（路径带序列号后缀），保证两个并发 claude/codex 进程各自有独立的
//! CLAUDE_CONFIG_DIR / CODEX_HOME，不会互相污染会话文件。
//!
//! 注意：OAuth refresh token 是一次性轮换；若多个会话同时触发 token 刷新，
//! 其中一方会因「refresh token already used」失败需重新登录。实践中 token 有效期较长
//! （数月），短任务执行期间触发轮换的概率极低；有需要时用 absorb 的写时校验降低冲突。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Result;

/// 单调递增序列号，用于生成同账号并发 checkout 的唯一目录名。
static CHECKOUT_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一次账号 checkout。`Drop` 时清理私有 env 目录（best-effort）。
pub struct Checkout {
    provider: String,
    id: String,
    env_dir: PathBuf,
}

impl Checkout {
    /// 为 `(provider, id)` 准备一个新的私有隔离目录并返回 Checkout 句柄。
    /// 同一账号可并发调用，每次返回不同的 `env_dir`（路径含单调序列号后缀）。
    pub fn acquire(data_dir: &Path, provider: &str, id: &str) -> Result<Self> {
        let seq = CHECKOUT_SEQ.fetch_add(1, Ordering::Relaxed);
        // 路径格式：<data_dir>/envs/<provider>/<sanitized_id>/<seq>
        let env_dir = data_dir
            .join("envs")
            .join(provider)
            .join(sanitize(id))
            .join(seq.to_string());
        std::fs::create_dir_all(&env_dir)?;
        harden_dir(&env_dir);

        Ok(Self {
            provider: provider.to_string(),
            id: id.to_string(),
            env_dir,
        })
    }

    /// 隔离环境私有目录（`CODEX_HOME` / `CLAUDE_CONFIG_DIR` 指向它）。
    pub fn env_dir(&self) -> &Path {
        &self.env_dir
    }

    /// 被 checkout 的账号 `(provider, id)`。
    pub fn account(&self) -> (&str, &str) {
        (&self.provider, &self.id)
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // 清理私有 env 目录；失败时静默忽略（例如 absorb 后目录已删）。
        let _ = std::fs::remove_dir_all(&self.env_dir);
    }
}

/// 隔离环境基础目录（账号级，不含序列号后缀）：`<data_dir>/envs/<provider>/<sanitized id>/`。
/// 各 Checkout 实例在此目录下再建带序列号的子目录；此函数供外部查询账号级目录（如 absorb 路径计算）。
pub fn env_dir(data_dir: &Path, provider: &str, id: &str) -> PathBuf {
    data_dir.join("envs").join(provider).join(sanitize(id))
}

/// 该账号当前是否有活跃的隔离会话。
/// 检测方式：账号级 env 基础目录下是否存在以纯数字命名的子目录（`Checkout::acquire` 写入的序列号目录）。
/// 只认数字名子目录，忽略旧格式遗留的 CLAUDE_CONFIG_DIR 文件/符号链接，避免把历史数据误判为活跃会话。
/// Drop 时子目录被清理，进程崩溃时目录会残留——可视为近似指标，不影响安全性。
pub fn is_checked_out(data_dir: &Path, provider: &str, id: &str) -> bool {
    let base = env_dir(data_dir, provider, id);
    std::fs::read_dir(&base)
        .ok()
        .and_then(|d| {
            d.filter_map(|e| e.ok()).find(|e| {
                e.file_type().ok().is_some_and(|ft| ft.is_dir())
                    && e.file_name()
                        .to_str()
                        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            })
        })
        .is_some()
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
    fn same_account_concurrent_acquire_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Checkout::acquire(tmp.path(), "codex", "a@x.com").unwrap();
        // 同账号并发 checkout 应成功，不再报「已被占用」。
        let second = Checkout::acquire(tmp.path(), "codex", "a@x.com")
            .expect("concurrent checkout of same account must succeed");
        // 两个实例的 env_dir 路径不同（各有独立序列号后缀）。
        assert_ne!(first.env_dir(), second.env_dir(), "concurrent checkouts must have distinct env dirs");
    }

    #[test]
    fn different_accounts_acquire_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Checkout::acquire(tmp.path(), "codex", "a@x.com").unwrap();
        let b = Checkout::acquire(tmp.path(), "codex", "b@x.com").unwrap();
        assert_ne!(a.env_dir(), b.env_dir());
    }

    #[test]
    fn env_dir_cleaned_up_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir;
        {
            let checkout = Checkout::acquire(tmp.path(), "codex", "a@x.com").unwrap();
            dir = checkout.env_dir().to_path_buf();
            assert!(dir.exists(), "env_dir 应在 checkout 持有期间存在");
        }
        // Drop 后目录应被清理。
        assert!(!dir.exists(), "env_dir 应在 checkout drop 后被清理");
    }

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(sanitize("a/b\\c"), "a_b_c");
        assert_eq!(sanitize("user@host.com"), "user@host.com");
    }
}
