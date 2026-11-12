//! Pi `settings.json` 默认选择读写：只碰 `defaultProvider` / `defaultModel`。
//!
//! 其他 key（含 `packages` / `retry` / 扩展状态）原样保留；`models.json` 与
//! `sessions/` 永不触碰。文件缺失时新建最小对象；内容不是 JSON 对象时报错
//! （不静默覆盖用户配置）。

use std::path::Path;

use subswap_core::error::{Error, Result};

/// 读当前默认选择（缺失时对应项为 `None`，不报错）。
pub fn read_default(agent_dir: &Path) -> Result<(Option<String>, Option<String>)> {
    let path = super::paths::settings_json_path(agent_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(e) => {
            return Err(Error::Provider(format!(
                "read Pi settings {}: {e}",
                path.display()
            )))
        }
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| {
            Error::Provider(format!("parse Pi settings {}: {e}", path.display()))
        })?;
    let obj = value.as_object().ok_or_else(|| {
        Error::Provider(format!(
            "Pi settings {} is not a JSON object; refusing to modify",
            path.display()
        ))
    })?;
    let provider = obj
        .get("defaultProvider")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let model = obj
        .get("defaultModel")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    Ok((provider, model))
}

/// 把默认选择指向指定 slot + model 并原子落盘，回读校验后返回。
///
/// `model` 为空时不碰 `defaultModel`（保留用户现有选择）。
pub fn write_default(agent_dir: &Path, provider: &str, model: &str) -> Result<()> {
    let path = super::paths::settings_json_path(agent_dir);
    let mut obj: serde_json::Map<String, serde_json::Value> =
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                    Error::Provider(format!("parse Pi settings {}: {e}", path.display()))
                })?;
                value.as_object().cloned().ok_or_else(|| {
                    Error::Provider(format!(
                        "Pi settings {} is not a JSON object; refusing to modify",
                        path.display()
                    ))
                })?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
            Err(e) => {
                return Err(Error::Provider(format!(
                    "read Pi settings {}: {e}",
                    path.display()
                )))
            }
        };
    obj.insert("defaultProvider".into(), provider.trim().into());
    if !model.trim().is_empty() {
        obj.insert("defaultModel".into(), model.trim().into());
    }
    write_settings_atomic(agent_dir, &path, &serde_json::Value::Object(obj))?;
    // 回读校验：确认默认选择确实落盘。
    let (back_provider, _) = read_default(agent_dir)?;
    if back_provider.as_deref() != Some(provider.trim()) {
        return Err(Error::Provider(format!(
            "Pi settings {} defaultProvider read-back mismatch; concurrent writer?",
            path.display()
        )));
    }
    Ok(())
}

/// 原子写 `settings.json`：tmp + rename；已有文件保留原权限，新建默认 0644
///（无密钥内容，不需要 0600；沿用 Pi 官方文件的常规权限）。
fn write_settings_atomic(
    agent_dir: &Path,
    path: &Path,
    value: &serde_json::Value,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Provider(format!("create Pi agent dir {}: {e}", parent.display()))
        })?;
    }
    let _ = agent_dir;
    #[cfg(unix)]
    let existing_mode = std::fs::metadata(path)
        .ok()
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o777
        });
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(value).map_err(Error::from)?).map_err(|e| {
        Error::Provider(format!("write Pi settings {}: {e}", tmp.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = existing_mode.unwrap_or(0o644);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| {
            Error::Provider(format!("chmod Pi settings {}: {e}", tmp.display()))
        })?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::Provider(format!("replace Pi settings {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_preserves_unrelated_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            super::super::paths::settings_json_path(&agent),
            r#"{"theme":"dark","retry":{"enabled":true}}"#,
        )
        .unwrap();
        write_default(&agent, "deepseek", "deepseek-chat").unwrap();
        let raw = std::fs::read_to_string(super::super::paths::settings_json_path(&agent)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["defaultProvider"], "deepseek");
        assert_eq!(v["defaultModel"], "deepseek-chat");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["retry"]["enabled"], true);

        // 空 model 不碰已有 defaultModel。
        write_default(&agent, "volcengine", "").unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(super::super::paths::settings_json_path(&agent)).unwrap(),
        )
        .unwrap();
        assert_eq!(v["defaultProvider"], "volcengine");
        assert_eq!(v["defaultModel"], "deepseek-chat");
    }

    #[test]
    fn missing_file_reads_empty_and_write_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        assert_eq!(read_default(&agent).unwrap(), (None, None));
        write_default(&agent, "deepseek", "deepseek-chat").unwrap();
        assert_eq!(
            read_default(&agent).unwrap(),
            (Some("deepseek".into()), Some("deepseek-chat".into()))
        );
    }

    #[test]
    fn non_object_settings_refuses_to_modify() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(super::super::paths::settings_json_path(&agent), "[1,2]").unwrap();
        assert!(write_default(&agent, "deepseek", "m").is_err());
    }
}
