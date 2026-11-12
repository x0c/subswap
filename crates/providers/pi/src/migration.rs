//! 把顶层 `volcengine` 账号池一次性搬进 Pi 抽屉（`pi` provider）。
//!
//! 语义：幂等 copy-to-pi-final。
//! - 每个 `volcengine` 账号按同一 id 在 `pi` 下建副本：secret 从
//!   `volcengine/<id>/api_key` 拷到 `pi/<id>/api_key`；`extra` 换成 Pi 形态
//!   （`kind=api` + `channel` + `pi_api{native_provider,base_url,model,auth_slot}`
//!   + `manual_only=true`，`billing` 沿用旧值）；label / 时间戳 / priority 原样保留。
//! - 火山 Coding Plan 的 Pi 池端点与 Claude 路径不同：`base_url` 固定改写为
//!   Pi 池 OpenAI 兼容地址（`.../api/coding/v3`），`model` 固定为 `kimi-k3.1-pro`，
//!   `channel` 取标准火山 channel；`auth_slot` 为 `volcengine`。
//! - registry 整表一次 `save`（原子替换），成功后再删旧 store 字段
//!   （`api_key` + 无 Pi 对应用途的 `access_key_id` / `secret_access_key`）。
//! - 不碰任何 live 文件（Claude `settings.json` / Pi `auth.json` / `settings.json` 都不动）；
//!   配额缓存不动（新 `pi::<id>` key 自然没有旧 `observed_at`，不继承）。
//! - 重复运行无新副本（`pi` 下已有同 id 即视为已搬，只清 `volcengine` 残留）；
//!   无 `volcengine` 账号时直接返回 0。
//!
//! 调用方（CLI / daemon 启动时）在单进程里顺序执行；与 daemon 并发各跑一次时
//! 后跑的一次看到空 `volcengine` 列表即 no-op，不会产生重复副本。

use subswap_core::error::Result;
use subswap_core::{Account, AccountRegistry, CredentialStore};

use super::auth::{PI_VOLCENGINE_BASE_URL, PI_VOLCENGINE_MODEL, volcengine_channel};
use super::{API_KEY_FIELD, PROVIDER_ID};

/// 旧顶层 provider id（只读兼容保留，见 lib 文档）。
const LEGACY_PROVIDER_ID: &str = "volcengine";
/// 旧推理 Key 字段（与 `subswap-provider-volcengine` 的 `API_KEY_FIELD` 同值）。
const LEGACY_API_KEY_FIELD: &str = "api_key";
/// 旧额度 AK/SK 字段（Pi 永不查额度，无对应去处，随搬迁删除，见模块文档）。
const LEGACY_QUOTA_FIELDS: [&str; 2] = ["access_key_id", "secret_access_key"];
/// 搬迁后 Pi 侧的 auth slot。
const VOLCENGINE_AUTH_SLOT: &str = "volcengine";

/// 执行搬迁，返回新建的 `pi` 副本数（0 表示无事可做）。
pub fn migrate_legacy_volcengine_to_pi(
    store: &dyn CredentialStore,
    registry: &AccountRegistry,
) -> Result<usize> {
    let mut accounts = registry.load()?;
    let legacy: Vec<Account> = accounts
        .iter()
        .filter(|a| a.provider == LEGACY_PROVIDER_ID)
        .cloned()
        .collect();
    if legacy.is_empty() {
        return Ok(0);
    }
    let mut created = 0;
    // 本轮实际完成搬迁（含历史残留清理）的旧 id。
    let mut moved_ids: Vec<subswap_core::AccountId> = Vec::new();
    for old in &legacy {
        let already = accounts
            .iter()
            .any(|a| a.provider == PROVIDER_ID && a.id == old.id);
        if already {
            // 历史残留：pi 副本已在，只需清掉 volcengine 侧。
            moved_ids.push(old.id.clone());
            continue;
        }
        let Some(secret) = store.get(LEGACY_PROVIDER_ID, &old.id.0, LEGACY_API_KEY_FIELD)?
        else {
            // 无 secret 的旧条目不搬（避免造出不可用的 pi 空号），留待人工处理。
            tracing::warn!(account = %old.id, "volcengine account has no stored API key; skipping Pi migration");
            continue;
        };
        store.set(PROVIDER_ID, &old.id.0, API_KEY_FIELD, &secret)?;
        accounts.push(pi_account_for(old));
        moved_ids.push(old.id.clone());
        created += 1;
    }
    // 同一轮里被标 active 的搬迁副本独占 pi active（沿用 set_active 单 active 语义）。
    normalize_pi_active(&mut accounts);
    // 原子替换整表：先落盘成功，才删旧 secret。
    accounts.retain(|a| a.provider != LEGACY_PROVIDER_ID || !moved_ids.contains(&a.id));
    registry.save(&accounts)?;
    for id in &moved_ids {
        let _ = store.delete(LEGACY_PROVIDER_ID, &id.0, LEGACY_API_KEY_FIELD);
        for field in LEGACY_QUOTA_FIELDS {
            let _ = store.delete(LEGACY_PROVIDER_ID, &id.0, field);
        }
    }
    Ok(created)
}

/// 由旧 `volcengine` 账号构造 Pi 副本（secret 由调用方另行拷贝）。
fn pi_account_for(old: &Account) -> Account {
    let mut extra = serde_json::Map::new();
    extra.insert("kind".into(), "api".into());
    extra.insert("channel".into(), volcengine_channel().into());
    extra.insert(
        "pi_api".into(),
        serde_json::json!({
            "native_provider": "volcengine",
            "base_url": PI_VOLCENGINE_BASE_URL,
            "model": PI_VOLCENGINE_MODEL,
            "auth_slot": VOLCENGINE_AUTH_SLOT,
        }),
    );
    extra.insert("manual_only".into(), true.into());
    let billing = old
        .extra
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "flat".into());
    extra.insert("billing".into(), billing);
    Account {
        provider: PROVIDER_ID.into(),
        id: old.id.clone(),
        label: old.label.clone(),
        active: old.active,
        created_at: old.created_at,
        last_used_at: old.last_used_at,
        priority: old.priority,
        extra,
    }
}

/// 同一 provider 下只保留一个 active：被搬过来的 active 副本优先。
fn normalize_pi_active(accounts: &mut [Account]) {
    let migrated_active = accounts
        .iter()
        .any(|a| a.provider == PROVIDER_ID && a.active);
    if !migrated_active {
        return;
    }
    let mut first = true;
    for account in accounts.iter_mut().filter(|a| a.provider == PROVIDER_ID) {
        if account.active && !first {
            account.active = false;
        }
        if account.active {
            first = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use subswap_core::{AccountId, FileStore};

    fn seed_old(registry: &AccountRegistry, store: &FileStore, id: &str, active: bool) {
        let mut extra = serde_json::Map::new();
        extra.insert("kind".into(), "api".into());
        extra.insert("manual_only".into(), true.into());
        extra.insert("billing".into(), "flat".into());
        registry
            .upsert(Account {
                provider: LEGACY_PROVIDER_ID.into(),
                id: AccountId(id.into()),
                label: format!("Ark …{id}"),
                active,
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                extra,
            })
            .unwrap();
        store.set(LEGACY_PROVIDER_ID, id, LEGACY_API_KEY_FIELD, "sk-live-key").unwrap();
        store.set(LEGACY_PROVIDER_ID, id, "access_key_id", "AK").unwrap();
        store.set(LEGACY_PROVIDER_ID, id, "secret_access_key", "SK").unwrap();
    }

    #[test]
    fn migrates_once_and_leaves_no_legacy_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let store = FileStore::new(tmp.path().join("credentials.json"));
        seed_old(&registry, &store, "ark-aaa", true);
        seed_old(&registry, &store, "ark-bbb", false);

        assert_eq!(migrate_legacy_volcengine_to_pi(&store, &registry).unwrap(), 2);
        // 幂等：第二轮无事可做。
        assert_eq!(migrate_legacy_volcengine_to_pi(&store, &registry).unwrap(), 0);

        let accounts = registry.load().unwrap();
        assert!(accounts.iter().all(|a| a.provider != LEGACY_PROVIDER_ID));
        let pi: Vec<_> = accounts.iter().filter(|a| a.provider == PROVIDER_ID).collect();
        assert_eq!(pi.len(), 2);
        let active: Vec<_> = pi.iter().filter(|a| a.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id.0, "ark-aaa");
        for account in pi {
            assert_eq!(account.extra["manual_only"], true);
            assert_eq!(account.extra["kind"], "api");
            assert_eq!(account.extra["pi_api"]["auth_slot"], "volcengine");
            assert_eq!(account.extra["pi_api"]["base_url"], PI_VOLCENGINE_BASE_URL);
            assert_eq!(account.extra["pi_api"]["model"], PI_VOLCENGINE_MODEL);
            assert!(store.get(PROVIDER_ID, &account.id.0, API_KEY_FIELD).unwrap().is_some());
        }
        assert!(store.get(LEGACY_PROVIDER_ID, "ark-aaa", LEGACY_API_KEY_FIELD).unwrap().is_none());
        assert!(store.get(LEGACY_PROVIDER_ID, "ark-aaa", "access_key_id").unwrap().is_none());
    }

    #[test]
    fn skips_keyless_legacy_entry_without_deleting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let store = FileStore::new(tmp.path().join("credentials.json"));
        let mut extra = serde_json::Map::new();
        extra.insert("manual_only".into(), true.into());
        registry
            .upsert(Account {
                provider: LEGACY_PROVIDER_ID.into(),
                id: AccountId("ark-nokey".into()),
                label: "Ark …key".into(),
                active: false,
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                extra,
            })
            .unwrap();

        assert_eq!(migrate_legacy_volcengine_to_pi(&store, &registry).unwrap(), 0);
        assert!(registry.find(LEGACY_PROVIDER_ID, &AccountId("ark-nokey".into())).unwrap().is_some());
    }
}
