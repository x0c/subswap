//! CLI 进程级共享上下文：注册 Provider、打开 keyring、加载 registry。

use std::sync::Arc;

use anyhow::Result;
use subswap_core::{
    Account, AccountRegistry, AuditLog, CredentialStore, KeyringStore, ProviderRegistry,
};
use subswap_provider_claude::ClaudeProvider;
use subswap_provider_codex::CodexProvider;

pub struct AppContext {
    pub store: Arc<dyn CredentialStore>,
    pub registry: Arc<AccountRegistry>,
    pub claude: Arc<ClaudeProvider>,
    pub codex: Arc<CodexProvider>,
    pub providers: ProviderRegistry,
    pub audit: AuditLog,
}

impl AppContext {
    pub fn build() -> Result<Self> {
        let store: Arc<dyn CredentialStore> = Arc::new(KeyringStore::new());
        let registry = Arc::new(AccountRegistry::from_default_paths()?);

        let claude = Arc::new(ClaudeProvider::new(store.clone(), registry.clone()));
        let codex = Arc::new(CodexProvider::new(store.clone(), registry.clone()));

        let mut providers = ProviderRegistry::new();
        providers.register(claude.clone());
        providers.register(codex.clone());

        let audit = AuditLog::from_default_paths()?;

        Ok(Self {
            store,
            registry,
            claude,
            codex,
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
