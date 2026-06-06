//! 用户无感地拉起后台 daemon:已经在跑则什么都不做;否则 fork+setsid 一个 detached 子进程。
//!
//! 设计要点:
//! - 通过 PID 文件上的 fs2 排他锁判断「是否已经有实例在跑」(不依赖 kill -0 / PID 复用问题)。
//! - 拉起方式:fork(由 std::process::Command 完成) + 在 pre_exec 里 setsid + stdio 重定向到日志。
//! - 不等待子进程,父进程退出后子进程被 init 收养,作为正常后台进程持续跑。
//! - 后台入口统一用当前 `subswap __daemon`。`subswapd` 只是兼容壳。
//! - 非 Unix 平台:暂不自动拉起(M4 只承诺 Linux / macOS)。

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
#[cfg(unix)]
use std::path::Path;

pub fn ensure_daemon_running() -> Result<()> {
    // 测试 / 用户禁用逃生口:SUBSWAP_NO_DAEMON=1 时不拉。
    if std::env::var_os("SUBSWAP_NO_DAEMON").is_some() {
        return Ok(());
    }
    if !daemon_auto_start_enabled() {
        tracing::debug!("daemon auto-start disabled on macOS; set SUBSWAP_AUTO_DAEMON=1 to opt in");
        return Ok(());
    }
    #[cfg(unix)]
    {
        use subswap_core::paths::AppPaths;

        let paths = AppPaths::resolve()?;
        let pid_path = paths.daemon_pid_file();
        if daemon_alive(&pid_path)? {
            return Ok(());
        }
        let binary = std::env::current_exe().context("resolve current subswap executable")?;
        let log_path = paths.daemon_log_file();
        spawn_detached_daemon(&binary, &log_path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tracing::debug!("daemon auto-start not supported on this platform; run subswapd manually");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn daemon_auto_start_enabled() -> bool {
    // macOS Keychain 授权绑定到具体进程/二进制签名。后台 daemon 默认读 keychain
    // 会制造额外授权弹窗,所以 macOS 只在用户显式 opt-in 时自动拉起。
    std::env::var_os("SUBSWAP_AUTO_DAEMON").is_some()
}

#[cfg(not(target_os = "macos"))]
fn daemon_auto_start_enabled() -> bool {
    true
}

#[cfg(unix)]
fn daemon_alive(pid_path: &Path) -> Result<bool> {
    use fs2::FileExt;
    if !pid_path.exists() {
        return Ok(false);
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pid_path)
        .with_context(|| format!("open pid file {}", pid_path.display()))?;
    // 锁能拿到 → 没人在跑;拿不到 → 已有 daemon。
    match f.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&f);
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

#[cfg(unix)]
fn spawn_detached_daemon(binary: &Path, log_path: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log_out.try_clone().context("dup daemon log fd")?;

    // SAFETY: pre_exec 里只调用 async-signal-safe 的 setsid;不分配,不取锁。
    let mut cmd = Command::new(binary);
    cmd.arg("__daemon");
    cmd.stdin(Stdio::null()).stdout(log_out).stderr(log_err);
    unsafe {
        cmd.pre_exec(|| {
            // 脱离当前 session/process group,这样:
            // 1. 父进程退出不会带着 daemon 一起死(SIGHUP 不会发到 daemon);
            // 2. daemon 不再持有控制终端,不抢 stdin。
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // spawn 后不 wait;Child 句柄 drop 默认就是 detach(不发 SIGKILL)。
    let _child = cmd
        .spawn()
        .with_context(|| format!("spawn detached daemon {}", binary.display()))?;
    Ok(())
}
