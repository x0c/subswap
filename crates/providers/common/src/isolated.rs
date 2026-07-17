//! 隔离运行的对象安全抽象：让 `subswap run/shell/env` 的 dispatch 不必按 provider 硬编码分支。
//!
//! 任何基于 [`crate::FileBlobProvider`] 组装的 provider（Codex、Kimi……）都自动获得本 trait 的实现；
//! 新增第三个文件型 provider 时，只需在 CLI 侧的查表里插一行，不必再改 `run.rs` 的分支逻辑。

use std::fs;
use std::path::Path;

use subswap_core::error::{Error, Result};
use subswap_core::{AccountId, Provider};

use crate::engine::FileBlobProvider;
use crate::runtime::FileBlobRuntime;

/// 对象安全的隔离运行抽象：`subswap run/shell/env` 只依赖这几个方法即可完成
/// 物化 / 吸收 / 环境变量名 / 原生 CLI 名的分发，无需知道具体 provider 类型。
pub trait IsolatedProvider: Send + Sync {
    /// provider 标识，如 "codex" / "kimi"。
    fn provider_id(&self) -> &'static str;
    /// 隔离环境变量名（如 `CODEX_HOME` / `KIMI_CODE_HOME`）。
    fn isolation_env_var(&self) -> &'static str;
    /// 原生 CLI 可执行名。
    fn native_cli(&self) -> &'static str;
    /// 把账号凭证物化进隔离目录：live 文件写到 `env_dir` 下对应相对位置 + provider 额外物化。
    fn materialize(&self, id: &AccountId, env_dir: &Path) -> Result<()>;
    /// 隔离会话结束后吸收（可能被原生 CLI 轮换过的）凭证，仅更新该账号的 store 副本。
    fn absorb(&self, id: &AccountId, env_dir: &Path) -> Result<()>;
}

impl<A: FileBlobRuntime> IsolatedProvider for FileBlobProvider<A> {
    fn provider_id(&self) -> &'static str {
        Provider::id(self)
    }

    fn isolation_env_var(&self) -> &'static str {
        self.isolation().env_var
    }

    fn native_cli(&self) -> &'static str {
        self.isolation().native_cli
    }

    fn materialize(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let blob = self.export_blob(id)?;
        let dest = env_dir.join(self.live_rel_path());
        FileBlobProvider::<A>::write_blob(&dest, &blob)?;
        self.materialize_extra_into(env_dir);
        Ok(())
    }

    fn absorb(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let dest = env_dir.join(self.live_rel_path());
        let raw = fs::read_to_string(&dest).map_err(|e| {
            Error::Provider(format!(
                "read isolated credentials at {}: {e}",
                dest.display()
            ))
        })?;
        self.absorb_blob(id, &raw)
    }
}
