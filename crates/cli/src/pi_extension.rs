//! Pi 扩展（`integrations/pi`）的默认安装链：`subswap login pi` 显式入口与
//! 默认入口尾部 best-effort 调用共用。
//!
//! 幂等约束（与 `ensure_daemon_running` 同类的 best-effort 语义）：
//! - 缺 `pi` 只 hint，返回 `Ok`，绝不报错；
//! - 扩展 link 不存在才 `ln -s`（比较时 canonicalize），指向别处永不覆盖只警告；
//! - `node_modules` 缺失才 `npm install --ignore-scripts`，`check` 失败只警告；
//! - `SUBSWAP_NO_PI=1` 时直接跳过（测试 / 用户禁用逃生口，类比 `SUBSWAP_NO_DAEMON`）。

use std::path::{Path, PathBuf};

/// Pi 扩展在用户 home 下的固定落点：`~/.pi/agent/extensions/subswap-pi`。
pub fn pi_link_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|base| {
        base.home_dir()
            .join(".pi")
            .join("agent")
            .join("extensions")
            .join("subswap-pi")
    })
}

/// Pi 扩展源码目录。`SUBSWAP_PI_SRC` 显式覆盖优先（测试用），否则用编译期
/// checkout 位置（`crates/cli/../../integrations/pi`）；release 产物不带源码
/// 时返回 `None`，调用方只 hint 不报错。
pub fn pi_source_dir() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("SUBSWAP_PI_SRC") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let candidate = PathBuf::from(trimmed);
            if candidate.join("package.json").exists() {
                return Some(candidate);
            }
            return None;
        }
    }
    let compiled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../integrations/pi");
    if compiled.join("package.json").exists() {
        Some(compiled)
    } else {
        None
    }
}

/// 扩展 link 的分类：缺失 / 已指到源码 / 指向别处 / 存在但不是 symlink。
pub enum PiLinkState {
    Missing,
    LinkedToSource,
    PointsElsewhere(PathBuf),
    NotSymlink,
}

/// 只读分类 link 状态，不改任何文件。比较时走 canonicalize，避免 `..`
/// 或相对路径造成的误判；dangling link 回退到 readlink 字面解析。
pub fn classify_link(link: &Path, source: &Path) -> PiLinkState {
    let meta = match std::fs::symlink_metadata(link) {
        Ok(meta) => meta,
        Err(_) => return PiLinkState::Missing,
    };
    if !meta.file_type().is_symlink() {
        return PiLinkState::NotSymlink;
    }
    let target = match std::fs::read_link(link) {
        Ok(target) => target,
        Err(_) => return PiLinkState::Missing,
    };
    if let (Ok(link_canon), Ok(source_canon)) =
        (std::fs::canonicalize(link), std::fs::canonicalize(source))
    {
        if link_canon == source_canon {
            return PiLinkState::LinkedToSource;
        }
        return PiLinkState::PointsElsewhere(target);
    }
    let resolved = if target.is_absolute() {
        target.clone()
    } else {
        link.parent()
            .map(|parent| parent.join(&target))
            .unwrap_or(target.clone())
    };
    if resolved == source {
        return PiLinkState::LinkedToSource;
    }
    if let Ok(source_canon) = std::fs::canonicalize(source) {
        if std::fs::canonicalize(&resolved)
            .ok()
            .is_some_and(|resolved_canon| resolved_canon == source_canon)
        {
            return PiLinkState::LinkedToSource;
        }
    }
    PiLinkState::PointsElsewhere(target)
}

/// 解析 `node --version` 输出（如 `v22.22.3`）为三元组；解析失败返回 `None`。
pub fn parse_node_version(output: &str) -> Option<(u64, u64, u64)> {
    let trimmed = output.trim().strip_prefix('v').unwrap_or(output.trim());
    let mut parts = trimmed.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    let patch = parts.next()?.trim().parse().ok()?;
    Some((major, minor, patch))
}

/// Pi 扩展要求 `node >= 22.19`（见 `integrations/pi/package.json` engines）。
pub fn node_meets_requirement(version: (u64, u64, u64)) -> bool {
    version.0 > 22 || (version.0 == 22 && version.1 >= 19)
}

/// 同步探针：`pi --version`，只读。缺失 / 非零退出返回 `None`。
pub fn probe_pi_version_sync() -> Option<String> {
    let output = std::process::Command::new("pi")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let text = raw.trim();
    let text = if text.is_empty() {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    } else {
        text.to_string()
    };
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 同步探针：`node --version`，只读。返回原文与解析后的三元组。
pub fn probe_node_version_sync() -> Option<(String, (u64, u64, u64))> {
    let output = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let version = parse_node_version(&raw)?;
    Some((raw, version))
}

fn create_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(source, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (source, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks unsupported on this platform",
        ))
    }
}

fn run_npm(source: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new("npm")
        .args(args)
        .current_dir(source)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to start `npm {}`: {e}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    anyhow::bail!("`npm {}` failed: {tail}", args.join(" "))
}

/// 同步版安装链：显式 `login pi` 与单测直接调用。默认入口经
/// [`ensure_pi_extension`] 走 `spawn_blocking`，避免 `npm install` 阻塞 runtime。
pub fn ensure_pi_extension_sync() -> anyhow::Result<()> {
    if std::env::var_os("SUBSWAP_NO_PI").is_some() {
        tracing::debug!("Pi extension setup disabled via SUBSWAP_NO_PI");
        return Ok(());
    }
    match probe_pi_version_sync() {
        Some(version) => tracing::debug!(pi_version = %version, "pi found"),
        None => {
            tracing::debug!(
                "pi not found; skipping Pi extension setup (install Pi, then run `subswap login pi`)"
            );
            return Ok(());
        }
    }
    let Some(source) = pi_source_dir() else {
        tracing::debug!(
            "Pi extension source not found; run `make install-pi` from a Subswap checkout, then `subswap login pi`"
        );
        return Ok(());
    };
    let source_canon = std::fs::canonicalize(&source).unwrap_or(source.clone());
    let Some(link) = pi_link_path() else {
        tracing::warn!("cannot resolve home directory; skipping Pi extension link");
        return Ok(());
    };
    match classify_link(&link, &source_canon) {
        PiLinkState::LinkedToSource => {
            tracing::debug!(link = %link.display(), "Pi extension link already points at source");
        }
        PiLinkState::Missing => {
            if let Some(parent) = link.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(err = %e, link = %link.display(), "cannot create Pi extensions dir; skipping link");
                    return Ok(());
                }
            }
            if let Err(e) = create_symlink(&source_canon, &link) {
                tracing::warn!(err = %e, link = %link.display(), "cannot link Pi extension; run `make install-pi` manually");
                return Ok(());
            }
            tracing::info!(link = %link.display(), "linked Pi extension");
        }
        PiLinkState::PointsElsewhere(target) => {
            tracing::warn!(
                link = %link.display(),
                target = %target.display(),
                "Pi extension link points elsewhere; never overwriting (remove it manually to reinstall)"
            );
            return Ok(());
        }
        PiLinkState::NotSymlink => {
            tracing::warn!(
                link = %link.display(),
                "Pi extension path exists but is not a symlink; never overwriting"
            );
            return Ok(());
        }
    }
    match probe_node_version_sync() {
        Some((raw, version)) if node_meets_requirement(version) => {
            tracing::debug!(node = %raw, "node meets Pi extension requirement");
        }
        Some((raw, _)) => {
            tracing::warn!(node = %raw, "node is older than 22.19; skipping Pi extension deps");
            return Ok(());
        }
        None => {
            tracing::warn!("node not found; Pi extension needs node >= 22.19");
            return Ok(());
        }
    }
    if !source.join("node_modules").exists() {
        tracing::info!("installing Pi extension dependencies (`npm install --ignore-scripts`)");
        if let Err(e) = run_npm(&source, &["install", "--ignore-scripts"]) {
            tracing::warn!(err = %e, "npm install failed; run `make install-pi` manually");
            return Ok(());
        }
    }
    if let Err(e) = run_npm(&source, &["run", "check"]) {
        tracing::warn!(err = %e, "Pi extension check failed; continuing");
        return Ok(());
    }
    tracing::debug!("Pi extension ready");
    Ok(())
}

/// 异步包一层，供 async 调用方（显式 `login pi`、默认入口尾部）使用。
pub async fn ensure_pi_extension() -> anyhow::Result<()> {
    tokio::task::spawn_blocking(ensure_pi_extension_sync)
        .await
        .map_err(|e| anyhow::anyhow!("Pi extension setup task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_versions() {
        assert_eq!(parse_node_version("v22.19.0"), Some((22, 19, 0)));
        assert_eq!(parse_node_version("v22.22.3\n"), Some((22, 22, 3)));
        assert_eq!(parse_node_version("20.1.0"), Some((20, 1, 0)));
        assert!(parse_node_version("v22.19").is_none());
        assert!(parse_node_version("not-a-version").is_none());
    }

    #[test]
    fn node_requirement_floor_is_22_19() {
        assert!(!node_meets_requirement((22, 18, 9)));
        assert!(node_meets_requirement((22, 19, 0)));
        assert!(node_meets_requirement((22, 22, 3)));
        assert!(node_meets_requirement((23, 0, 0)));
        assert!(!node_meets_requirement((20, 19, 0)));
    }

    #[test]
    fn link_classification_is_idempotent_and_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let link = tmp.path().join("subswap-pi");

        assert!(matches!(
            classify_link(&link, &source),
            PiLinkState::Missing
        ));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, &link).unwrap();
        assert!(matches!(
            classify_link(&link, &source),
            PiLinkState::LinkedToSource
        ));
        // 指向别处：只分类，不覆盖。
        assert!(matches!(
            classify_link(&link, &other),
            PiLinkState::PointsElsewhere(_)
        ));
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());

        let regular = tmp.path().join("regular");
        std::fs::write(&regular, "x").unwrap();
        assert!(matches!(
            classify_link(&regular, &source),
            PiLinkState::NotSymlink
        ));
    }
}
