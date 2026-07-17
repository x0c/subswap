//! CLI 进程级共享上下文：注册 Provider、打开 keyring、加载 registry。

use std::sync::Arc;

use anyhow::Result;
use subswap_core::{
    paths::AppPaths, Account, AccountRegistry, AuditLog, CredentialStore, FileStore, KeyringStore,
    ProviderRegistry,
};
use subswap_provider_claude::ClaudeProvider;
use subswap_provider_codex::CodexProvider;
use subswap_provider_kimi::KimiProvider;

pub struct AppContext {
    pub store: Arc<dyn CredentialStore>,
    pub registry: Arc<AccountRegistry>,
    pub claude: Arc<ClaudeProvider>,
    pub codex: Arc<CodexProvider>,
    pub kimi: Arc<KimiProvider>,
    pub providers: ProviderRegistry,
    pub audit: AuditLog,
}

impl AppContext {
    pub fn build() -> Result<Self> {
        // 凭证后端：明文文件 + 旧钥匙串懒迁移。避免 macOS 每次读凭证弹钥匙串授权框。
        let paths = AppPaths::resolve()?;
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
            paths.credentials_file(),
            KeyringStore::new(),
        ));
        let registry = Arc::new(AccountRegistry::from_default_paths()?);

        let claude = Arc::new(ClaudeProvider::new(store.clone(), registry.clone()));
        let codex = Arc::new(subswap_provider_codex::new(store.clone(), registry.clone()));
        let kimi = Arc::new(subswap_provider_kimi::new(store.clone(), registry.clone()));

        let mut providers = ProviderRegistry::new();
        providers.register(claude.clone());
        providers.register(codex.clone());
        providers.register(kimi.clone());

        let audit = AuditLog::from_default_paths()?;

        Ok(Self {
            store,
            registry,
            claude,
            codex,
            kimi,
            providers,
            audit,
        })
    }

    /// 按「全局显示顺序」展开所有账号：先按 provider id 字母序（与 `ProviderRegistry` 一致），
    /// 再按 registry.toml 文件内的存储顺序。`subswap` 默认入口、`subswap swap N`、`subswap rm N`
    /// 共享同一编号映射，保证 `swap 3` 切到屏幕上看见的第 3 行。
    pub fn list_ordered(&self) -> Result<Vec<Account>> {
        let mut out = Vec::new();
        for pid in self.providers.ids() {
            out.extend(self.registry.list_by_provider(&pid)?);
        }
        Ok(out)
    }
}
