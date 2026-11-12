//! Pi `auth.json` 多 slot 读写：`{slot: {"type": "api_key", "key": "..."}}`。
//!
//! 只合并调用方自己的 slot；其他 slot 原样保留。写入经临时文件 + rename，
//! 成功后回读校验（Pi JS 用 proper-lockfile 目录锁，Rust fs2 锁与它互不可见，
//! 回读是发现并发互盖的最后一道网）。

use sha2::{Digest, Sha256};
use subswap_core::error::{Error, Result};

/// 火山方舟 Coding Plan 在 Pi 池里的 OpenAI 兼容端点（区别于 Claude 路径 `.../api/coding`）。
pub const PI_VOLCENGINE_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/coding/v3";
/// 火山方舟 Coding Plan 在 Pi 池里的默认模型。
pub const PI_VOLCENGINE_MODEL: &str = "kimi-k3.1-pro";
/// 标准火山 key 的 Pi channel（与 Pi 扩展 `DEFAULT_VOLCENGINE_CHANNEL` 同形）。
pub fn volcengine_channel() -> String {
    format!("volcengine:{PI_VOLCENGINE_BASE_URL}#{PI_VOLCENGINE_MODEL}")
}

/// 由 slot + key 生成稳定账号 id：`pi-` + SHA-256 前 12 hex。
pub fn account_id_for(slot: &str, api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(slot.trim().as_bytes());
    hasher.update([0u8]);
    hasher.update(api_key.trim().as_bytes());
    let digest = hasher.finalize();
    format!(
        "pi-{}",
        digest
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// 人类可读标签：slot 名 + key 末四位（不含密钥本身）。
pub fn label_for(slot: &str, api_key: &str) -> String {
    let suffix: String = api_key
        .trim()
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{slot} …{suffix}")
}

/// 读某个 slot 的 key；文件缺失 / slot 缺失返回 `None`（调用方再定是导入还是报错）。
pub fn read_slot(agent_dir: &std::path::Path, slot: &str) -> Result<Option<String>> {
    let path = super::paths::auth_json_path(agent_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Provider(format!(
                "read Pi auth {}: {e}",
                path.display()
            )))
        }
    };
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| {
            Error::Provider(format!("parse Pi auth {}: {e}", path.display()))
        })?;
    Ok(map.get(slot).and_then(api_key_from_entry))
}

/// 列出 live `auth.json` 全部 slot（文件名缺失视为空池，不报错）。
pub fn list_slots(agent_dir: &std::path::Path) -> Result<Vec<(String, String)>> {
    let path = super::paths::auth_json_path(agent_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Provider(format!(
                "read Pi auth {}: {e}",
                path.display()
            )))
        }
    };
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| {
            Error::Provider(format!("parse Pi auth {}: {e}", path.display()))
        })?;
    Ok(map
        .into_iter()
        .filter_map(|(slot, entry)| api_key_from_entry(&entry).map(|key| (slot, key)))
        .collect())
}

fn api_key_from_entry(entry: &serde_json::Value) -> Option<String> {
    entry
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 把单个 slot 合并进 live `auth.json` 并原子落盘（0600），回读校验后返回。
///
/// 其他 slot 原样保留；live 缺失则新建。回读校验失败说明写入被并发覆盖，
/// 调用方应按失败处理（`swap_with_snapshot` 会回滚同一事务里的其他文件）。
pub fn write_slot_merged(
    agent_dir: &std::path::Path,
    slot: &str,
    api_key: &str,
) -> Result<()> {
    let path = super::paths::auth_json_path(agent_dir);
    let mut map: serde_json::Map<String, serde_json::Value> = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| {
                Error::Provider(format!("parse Pi auth {}: {e}", path.display()))
            })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            return Err(Error::Provider(format!(
                "read Pi auth {}: {e}",
                path.display()
            )))
        }
    };
    map.insert(
        slot.to_string(),
        serde_json::json!({"type": "api_key", "key": api_key.trim()}),
    );
    write_auth_atomic(&path, &serde_json::Value::Object(map))?;
    // 回读校验：确认本 slot 确实落盘（防 Pi / 其他进程并发互盖）。
    let back = read_slot(agent_dir, slot)?.unwrap_or_default();
    if back != api_key.trim() {
        return Err(Error::Provider(format!(
            "Pi auth {} slot {slot} read-back mismatch; concurrent writer?",
            path.display()
        )));
    }
    Ok(())
}

/// 原子写 live `auth.json`：tmp + rename + 0600（与 file-blob 引擎 `write_blob` 同策略）。
fn write_auth_atomic(path: &std::path::Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Provider(format!("create Pi agent dir {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(value).map_err(Error::from)?).map_err(|e| {
        Error::Provider(format!("write Pi auth {}: {e}", tmp.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            Error::Provider(format!("chmod Pi auth {}: {e}", tmp.display()))
        })?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::Provider(format!("replace Pi auth {}: {e}", path.display())))?;
    Ok(())
}

/// `rm` 用：live 里正好是目标 key 才清除该 slot（保留其他 slot），返回是否删过。
/// 加锁后重读-改-写，避免与并发 `activate` 互盖；回读确认 slot 已消失。
pub fn remove_slot_if_matching(
    agent_dir: &std::path::Path,
    slot: &str,
    api_key: &str,
) -> Result<bool> {
    let path = super::paths::auth_json_path(agent_dir);
    let current = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(Error::Provider(format!(
                "read Pi auth {}: {e}",
                path.display()
            )))
        }
    };
    let is_target: bool = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&current)
        .ok()
        .and_then(|map| map.get(slot).cloned())
        .and_then(|entry| api_key_from_entry(&entry))
        .is_some_and(|live_key| live_key == api_key.trim());
    if !is_target {
        return Ok(false);
    }
    let lock_path = agent_dir.join(".subswap.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| Error::Provider(format!("open Pi lock {}: {e}", lock_path.display())))?;
    fs2_lock(&lock_file).map_err(|e| {
        Error::Provider(format!("lock Pi credentials: {e}"))
    })?;
    let result = (|| -> Result<bool> {
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            Error::Provider(format!("read Pi auth {}: {e}", path.display()))
        })?;
        let mut map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&raw).unwrap_or_default();
        let still_target = map
            .get(slot)
            .and_then(api_key_from_entry)
            .is_some_and(|live_key| live_key == api_key.trim());
        if !still_target {
            return Ok(false);
        }
        map.remove(slot);
        write_auth_atomic(&path, &serde_json::Value::Object(map))?;
        Ok(true)
    })();
    let _ = fs2_unlock(&lock_file);
    result
}

#[cfg(unix)]
fn fs2_lock(file: &std::fs::File) -> std::io::Result<()> {
    use fs2::FileExt;
    file.lock_exclusive()
}

#[cfg(not(unix))]
fn fs2_lock(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn fs2_unlock(file: &std::fs::File) -> std::io::Result<()> {
    use fs2::FileExt;
    FileExt::unlock(file)
}

#[cfg(not(unix))]
fn fs2_unlock(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// 检查 store 里的 key 是否与 live slot 一致（供测试与诊断用）。
pub fn slot_matches(slot_key: Option<&str>, api_key: &str) -> bool {
    slot_key.is_some_and(|live| live.trim() == api_key.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_agent() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        (tmp, agent)
    }

    #[test]
    fn write_keeps_other_slots_and_sets_mode() {
        let (_tmp, agent) = tmp_agent();
        write_slot_merged(&agent, "deepseek", "sk-deep").unwrap();
        write_slot_merged(&agent, "volcengine", "sk-volc").unwrap();
        assert_eq!(read_slot(&agent, "deepseek").unwrap().as_deref(), Some("sk-deep"));
        assert_eq!(read_slot(&agent, "volcengine").unwrap().as_deref(), Some("sk-volc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(super::super::paths::auth_json_path(&agent))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn remove_only_matching_slot() {
        let (_tmp, agent) = tmp_agent();
        write_slot_merged(&agent, "deepseek", "sk-deep").unwrap();
        write_slot_merged(&agent, "volcengine", "sk-volc").unwrap();
        assert!(!remove_slot_if_matching(&agent, "volcengine", "sk-other").unwrap());
        assert_eq!(read_slot(&agent, "volcengine").unwrap().as_deref(), Some("sk-volc"));
        assert!(remove_slot_if_matching(&agent, "volcengine", "sk-volc").unwrap());
        assert!(read_slot(&agent, "volcengine").unwrap().is_none());
        assert_eq!(read_slot(&agent, "deepseek").unwrap().as_deref(), Some("sk-deep"));
        assert!(!remove_slot_if_matching(&agent, "volcengine", "sk-volc").unwrap());
    }

    #[test]
    fn fingerprint_is_stable_per_slot_and_key() {
        assert_eq!(account_id_for("deepseek", "k"), account_id_for("deepseek", "k"));
        assert_ne!(account_id_for("deepseek", "k"), account_id_for("deepseek", "k2"));
        assert_ne!(account_id_for("deepseek", "k"), account_id_for("volcengine", "k"));
        assert!(account_id_for("deepseek", "k").starts_with("pi-"));
    }
}
