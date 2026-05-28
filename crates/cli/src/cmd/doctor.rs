//! `subswap doctor`：自检路径、keyring、各 Provider 客户端探针。

use anyhow::Result;
use subswap_core::{CredentialStore, KeyringStore};

use crate::app::AppContext;

pub async fn run(ctx: &AppContext) -> Result<()> {
    println!("subswap doctor");
    println!("------------------------------------------------------------");
    match subswap_core::paths::AppPaths::resolve() {
        Ok(p) => {
            println!("[ok ] config dir   {}", p.config_dir.display());
            println!("[ok ] data dir     {}", p.data_dir.display());
            println!("[ok ] state dir    {}", p.state_dir.display());
            println!("[ok ] cache dir    {}", p.cache_dir.display());
            println!("[ok ] registry     {}", p.registry_file().display());
            println!("[ok ] audit log    {}", p.audit_log().display());
        }
        Err(e) => println!("[err] resolve paths: {e}"),
    }

    let store = KeyringStore::new();
    let probe_field = "doctor_probe";
    match store.set("subswap", "_doctor", probe_field, "ok") {
        Ok(()) => {
            let _ = store.delete("subswap", "_doctor", probe_field);
            println!("[ok ] system keyring");
        }
        Err(e) => println!("[err] system keyring: {e}"),
    }

    for p in ctx.providers.all() {
        println!();
        println!("[{}] {}", p.id(), p.display_name());
        for t in p.client_targets() {
            let tag = if t.probe_path.exists() { "ok " } else { "mis" };
            println!(
                "  [{tag}] {:<24} {}",
                t.display_name,
                t.probe_path.display()
            );
        }
    }
    Ok(())
}
