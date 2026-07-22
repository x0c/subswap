//! Kimi / Moonshot Provider。基于 subswap-provider-common 的文件型引擎。

mod kimi_files;
mod kimi_usage;
mod oauth;
mod paths;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, AccountRegistry, CredentialStore, Quota};
use subswap_provider_common::{
    BlobMetadata, FileBlobProvider, FileBlobRuntime, IsolationSpec, RefreshOutcome,
};

pub const PROVIDER_ID: &str = "kimi";

/// Kimi runtime adapter：只提供差异点，机制在引擎。
pub struct KimiRuntime;

#[async_trait]
impl FileBlobRuntime for KimiRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "Kimi / Moonshot"
    }
    fn home(&self) -> PathBuf {
        paths::kimi_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::active_cred_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        kimi_files::parse_metadata(blob)
    }
    fn isolation(&self) -> IsolationSpec {
        IsolationSpec {
            env_var: "KIMI_CODE_HOME",
            native_cli: "kimi",
        }
    }
    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
        oauth::refresh_blob(blob).await
    }
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        kimi_usage::fetch_quota_with_active_recovery(access_token, account).await
    }
}

/// 便捷别名：Kimi Provider = 文件型引擎 + Kimi adapter。
pub type KimiProvider = FileBlobProvider<KimiRuntime>;

/// 构造 KimiProvider（与 CodexProvider/ClaudeProvider 的 `::new` 调用面一致）。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> KimiProvider {
    FileBlobProvider::new(KimiRuntime, store, registry)
}

#[cfg(test)]
mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    pub struct MockServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockServer {
        pub fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    let request = String::from_utf8_lossy(&buffer[..count]);
                    captured
                        .lock()
                        .unwrap()
                        .push(request.lines().next().unwrap_or_default().to_string());
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            Self {
                base_url,
                requests,
                handle: Some(handle),
            }
        }

        pub fn base_url(&self) -> &str {
            &self.base_url
        }

        pub fn finish(mut self) -> Vec<String> {
            self.handle.take().unwrap().join().unwrap();
            Arc::try_unwrap(self.requests)
                .unwrap()
                .into_inner()
                .unwrap()
        }
    }
}
