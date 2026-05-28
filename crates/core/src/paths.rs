//! 统一路径解析。所有 Provider 元数据、审计日志、状态文件都从这里取。
//!
//! 遵循 XDG（Linux）/ Library/Application Support（macOS）/ AppData（Windows）。

use crate::error::{Error, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

/// 项目维度的标准路径集合。
pub struct AppPaths {
    /// 配置：registry.toml、provider 元数据。
    pub config_dir: PathBuf,
    /// 数据：审计日志、备份快照。
    pub data_dir: PathBuf,
    /// 运行时状态：当前激活账号缓存、daemon pid 等。
    pub state_dir: PathBuf,
    /// 缓存：额度查询缓存等可丢弃数据。
    pub cache_dir: PathBuf,
}

impl AppPaths {
    /// 解析默认路径；目录不存在时会自动创建。
    pub fn resolve() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "subswap", "subswap")
            .ok_or_else(|| Error::Config("cannot resolve user directories".into()))?;

        let config_dir = dirs.config_dir().to_path_buf();
        let data_dir = dirs.data_dir().to_path_buf();
        let cache_dir = dirs.cache_dir().to_path_buf();
        // ProjectDirs 没有 state_dir 抽象，按平台约定挂在 data_dir 下。
        let state_dir = data_dir.join("state");

        for d in [&config_dir, &data_dir, &state_dir, &cache_dir] {
            std::fs::create_dir_all(d)?;
        }

        Ok(Self {
            config_dir,
            data_dir,
            state_dir,
            cache_dir,
        })
    }

    /// 账号注册表路径：`<config_dir>/registry.toml`。
    pub fn registry_file(&self) -> PathBuf {
        self.config_dir.join("registry.toml")
    }

    /// 数值调优配置文件路径：`<config_dir>/config.toml`。
    ///
    /// 文件可缺失：缺则使用 [`crate::defaults`] 中的编译期默认值。详见 [`crate::settings`]。
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// 审计日志：`<data_dir>/audit.log`。
    pub fn audit_log(&self) -> PathBuf {
        self.data_dir.join("audit.log")
    }

    /// 切换前快照根目录：`<state_dir>/snapshots/`。
    pub fn snapshots_dir(&self) -> PathBuf {
        self.state_dir.join("snapshots")
    }

    /// subswapd 守护进程 PID 文件:`<state_dir>/subswapd.pid`。
    /// 通过 fs2 文件锁标识唯一存活实例;退出后保留 PID 仅作信息参考。
    pub fn daemon_pid_file(&self) -> PathBuf {
        self.state_dir.join("subswapd.pid")
    }

    /// subswapd 守护进程日志文件:`<data_dir>/subswapd.log`。
    /// 用 append 模式打开,后续可由 logrotate 切割。
    pub fn daemon_log_file(&self) -> PathBuf {
        self.data_dir.join("subswapd.log")
    }
}
