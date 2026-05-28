//! `subswap rm <id|N>`：从 registry 与 keyring 删除账号。
//!
//! 引用形式与 `subswap swap` 一致：数字编号 / id / label / `provider/id`，详见 [`crate::cmd::resolve_account`]。

use anyhow::Result;
use subswap_core::AuditEvent;

use crate::app::AppContext;
use crate::cmd::resolve_account;

pub async fn run(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;

    ctx.registry.remove(&acc.provider, &acc.id)?;

    let fields: &[&str] = match acc.provider.as_str() {
        "claude" => &["credentials_json"],
        "codex" => &["auth_json"],
        _ => &[],
    };
    for f in fields {
        if let Err(e) = ctx.store.delete(&acc.provider, acc.id.0.as_str(), f) {
            tracing::warn!(err=%e, field=%f, "keyring delete failed (continuing)");
        }
    }

    ctx.audit
        .append(AuditEvent::ok("rm", &acc.provider, Some(acc.id.0.as_str())));
    println!("removed {}/{}", acc.provider, acc.id);
    Ok(())
}
