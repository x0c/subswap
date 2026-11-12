//! `subswap doctor`：自检路径、keyring、各 Provider 客户端探针，以及 Pi 扩展探针。

use anyhow::Result;
use subswap_core::{CredentialStore, KeyringStore};

use crate::app::AppContext;
use crate::pi_extension::{
    classify_link, node_meets_requirement, pi_link_path, pi_source_dir, probe_node_version_sync,
    probe_pi_version_sync, PiLinkState,
};

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

    // Pi 扩展探针：只读，不改任何状态（不建 link、不跑 npm）。
    println!();
    println!("[pi] Pi extension (subswap-pi)");
    let name = "pi";
    match probe_pi_version_sync() {
        Some(version) => println!("  [ok ] {name:<24} {version}"),
        None => {
            let detail = "not found (install Pi to use subswap-pi; skipping)";
            println!("  [mis] {name:<24} {detail}");
        }
    }
    let name = "extension link";
    match pi_link_path() {
        None => {
            let detail = "cannot resolve home directory";
            println!("  [mis] {name:<24} {detail}");
        }
        Some(link) => match std::fs::symlink_metadata(&link) {
            Err(_) => {
                let detail = format!("missing: {}", link.display());
                println!("  [mis] {name:<24} {detail}");
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&link)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|e| format!("<unreadable: {e}>"));
                let (tag, detail) = match pi_source_dir() {
                    Some(source) => match classify_link(&link, &source) {
                        PiLinkState::LinkedToSource => {
                            ("ok ", format!("{} -> {target}", link.display()))
                        }
                        _ => (
                            "mis",
                            format!(
                                "{} -> {target} (points elsewhere; never overwriting)",
                                link.display()
                            ),
                        ),
                    },
                    None => ("ok ", format!("{} -> {target}", link.display())),
                };
                println!("  [{tag}] {name:<24} {detail}");
            }
            Ok(_) => {
                let detail = format!("exists but is not a symlink: {}", link.display());
                println!("  [mis] {name:<24} {detail}");
            }
        },
    }
    let name = "node";
    match probe_node_version_sync() {
        None => {
            let detail = "not found (need >=22.19 for subswap-pi)";
            println!("  [mis] {name:<24} {detail}");
        }
        Some((raw, version)) if node_meets_requirement(version) => {
            println!("  [ok ] {name:<24} {raw} (>=22.19)");
        }
        Some((raw, _)) => {
            let detail = format!("{raw} (<22.19; upgrade to use subswap-pi)");
            println!("  [mis] {name:<24} {detail}");
        }
    }
    Ok(())
}
