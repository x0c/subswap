//! 通用「切换激活账号」骨架。
//!
//! Provider 不应该自己再写 flock → snapshot → 写文件 → 回滚 这套流程，
//! 用 [`swap_with_snapshot`] 声明「要写哪些目标 + 提交回调」即可。
//!
//! 设计要点：
//! - 阻塞 IO。调用方需要确保自己已在 `tokio::task::spawn_blocking` 内。
//! - 每个目标按顺序：(a) 备份到 snapshot 目录 + 内存；(b) 调用 writer 写新内容。
//! - 任一步失败：把所有 target 回滚到备份状态（原本存在的恢复内容；原本不存在的删除）。
//! - writer 成功后还会调用 `commit`，常见用途是 `AccountRegistry::set_active`。
//!   commit 失败同样回滚已写入的文件。
//! - 锁文件位于 `provider_home/.subswap.lock`；与各 provider 保持兼容。

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use fs2::FileExt;

use crate::error::{Error, Result};
use crate::paths::AppPaths;

/// 写入新内容到目标路径的函数对象。各 Provider 用 [`Box::new`] 自己造，
/// 把序列化、权限设置等细节封进去。
pub type SwapWriter<'a> = Box<dyn FnOnce(&Path) -> Result<()> + Send + 'a>;

/// 一次 swap 中需要原子更新的文件。
pub struct SwapTarget<'a> {
    /// 快照子目录内的文件名（snapshot 用，不需要包含路径）。
    pub snapshot_name: &'a str,
    /// 上游客户端读取的真实路径。
    pub live_path: PathBuf,
    /// 写入函数；只负责把新内容写到 `live_path`。
    pub writer: SwapWriter<'a>,
}

/// 执行一次受保护的切换。
///
/// 参数：
/// - `provider_id`：用于快照目录命名（`<provider>-<ts>`）。
/// - `provider_home`：放锁文件的目录（如 `~/.claude` / `~/.codex`）。
/// - `targets`：要原子写入的文件列表。
/// - `commit`：所有文件写入成功后执行的最后一步（典型：把 registry 标 active）。
///
/// 失败时已写入的文件会回滚到调用前的状态。
pub fn swap_with_snapshot<'a, C>(
    provider_id: &str,
    provider_home: &Path,
    targets: Vec<SwapTarget<'a>>,
    commit: C,
) -> Result<()>
where
    C: FnOnce() -> Result<()>,
{
    fs::create_dir_all(provider_home)?;
    let lock_path = provider_home.join(".subswap.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file
        .lock_exclusive()
        .map_err(|e| Error::Provider(format!("lock {} failed: {e}", lock_path.display())))?;

    let result = swap_locked(provider_id, targets, commit);
    let _ = FileExt::unlock(&lock_file);
    result
}

fn swap_locked<'a, C>(provider_id: &str, targets: Vec<SwapTarget<'a>>, commit: C) -> Result<()>
where
    C: FnOnce() -> Result<()>,
{
    // 1. 创建快照目录。
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let snap_dir = AppPaths::resolve()?
        .snapshots_dir()
        .join(format!("{provider_id}-{ts}"));
    fs::create_dir_all(&snap_dir)?;

    // 2. 备份现有文件（原本存在的进 snapshot 目录 + 内存）。
    let mut plans: Vec<TargetPlan> = Vec::with_capacity(targets.len());
    for target in targets {
        let backup = if target.live_path.exists() {
            let raw = fs::read_to_string(&target.live_path)?;
            fs::write(snap_dir.join(target.snapshot_name), &raw)?;
            Some(raw)
        } else {
            None
        };
        plans.push(TargetPlan {
            live_path: target.live_path,
            writer: target.writer,
            backup,
        });
    }
    tracing::info!(snapshot = %snap_dir.display(), "pre-swap snapshot saved");

    // 3. 顺序写入；任一失败回滚所有 plan（即使 writer 没跑到也尝试 restore，让状态一致）。
    let backups: Vec<(PathBuf, Option<String>)> = plans
        .iter()
        .map(|p| (p.live_path.clone(), p.backup.clone()))
        .collect();

    for plan in plans {
        let TargetPlan {
            live_path, writer, ..
        } = plan;
        if let Err(e) = writer(&live_path) {
            rollback(&backups);
            return Err(e);
        }
    }

    // 4. 收尾 commit（典型：registry.set_active）。失败同样回滚。
    if let Err(e) = commit() {
        rollback(&backups);
        return Err(e);
    }

    Ok(())
}

struct TargetPlan<'a> {
    live_path: PathBuf,
    writer: SwapWriter<'a>,
    backup: Option<String>,
}

fn rollback(backups: &[(PathBuf, Option<String>)]) {
    for (path, backup) in backups {
        match backup {
            Some(raw) => {
                if let Err(e) = fs::write(path, raw) {
                    tracing::error!(err=%e, path=%path.display(), "rollback restore failed");
                }
            }
            None => {
                // 原本不存在 → 把刚写入的删掉，保持「未登录」状态。
                let _ = fs::remove_file(path);
            }
        }
    }
}
