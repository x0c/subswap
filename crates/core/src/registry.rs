//! ProviderRegistry：CLI / daemon 启动时注册所有 Provider 实例，按 id 取用。

use crate::error::{Error, Result};
use crate::provider::Provider;
use std::collections::BTreeMap;
use std::sync::Arc;

/// 简单的有序注册表。BTreeMap 保证 list 输出稳定顺序。
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    inner: BTreeMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 Provider。重复注册同 id 会覆盖。
    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.inner.insert(provider.id().to_string(), provider);
    }

    /// 按 id 获取 Provider。
    pub fn get(&self, id: &str) -> Result<Arc<dyn Provider>> {
        self.inner
            .get(id)
            .cloned()
            .ok_or_else(|| Error::ProviderNotFound(id.to_string()))
    }

    /// 遍历所有已注册 Provider。
    pub fn all(&self) -> Vec<Arc<dyn Provider>> {
        self.inner.values().cloned().collect()
    }

    /// 已注册的 Provider id 列表。
    pub fn ids(&self) -> Vec<String> {
        self.inner.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}
