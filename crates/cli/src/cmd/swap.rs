//! `subswap swap [<id|N>]`：显式切换激活账号。手动入口，不依赖网络/quota。
//!
//! 引用形式：
//! - 数字 `N`：与 `subswap` 默认入口列出的全局编号一致（见 [`crate::cmd::resolve_account`]）。
//! - id / label / `provider/id`：保留原 `find_unique` 行为，脚本可继续用。
//! - 无参：打印带编号的账号列表 + Usage 提示；不做任何切换。

use std::io::{self, IsTerminal};

use anyhow::Result;
use subswap_core::AuditEvent;

use crate::app::AppContext;
use crate::cmd::resolve_account;

pub async fn run(ctx: &AppContext, id_input: Option<&str>) -> Result<()> {
    let Some(input) = id_input else {
        print_listing(ctx)?;
        return Ok(());
    };

    let acc = resolve_account(ctx, input)?;
    let p = ctx.providers.get(&acc.provider)?;
    let res = p.activate(&acc.id).await;
    match res {
        Ok(()) => {
            ctx.audit.append(AuditEvent::ok(
                "activate",
                &acc.provider,
                Some(acc.id.0.as_str()),
            ));
            println!("swap → {}/{}", acc.provider, acc.id);
            Ok(())
        }
        Err(e) => {
            ctx.audit.append(AuditEvent::err(
                "activate",
                &acc.provider,
                Some(acc.id.0.as_str()),
                &e.to_string(),
            ));
            Err(anyhow::Error::from(e).context(format!("swap {}/{} failed", acc.provider, acc.id)))
        }
    }
}

/// 无参 `subswap swap`：列出编号 + 用法。**故意不查 quota**，保持「manual swap 不依赖网络」的不变量。
fn print_listing(ctx: &AppContext) -> Result<()> {
    let ordered = ctx.list_ordered()?;
    if ordered.is_empty() {
        println!("No accounts. Log in to Claude Code or Codex CLI, then re-run `subswap`.");
        return Ok(());
    }

    let color = io::stdout().is_terminal();
    println!("Usage: subswap swap <N | id | provider/id>");
    println!();
    for (idx, acc) in ordered.iter().enumerate() {
        let n = idx + 1;
        let star_plain = if acc.active { "*" } else { " " };
        let star = paint(color, if acc.active { "1;36" } else { "" }, star_plain);
        let num = paint(color, "2", &format!("{n:>2}"));
        let qualified = format!("{}/{}", acc.provider, acc.id);
        let name = if acc.active {
            qualified
        } else {
            paint(color, "2", &qualified)
        };
        println!("  {star} {num} {name}");
    }
    Ok(())
}

fn paint(color: bool, sgr: &str, body: &str) -> String {
    if !color || sgr.is_empty() {
        return body.to_string();
    }
    format!("\x1b[{sgr}m{body}\x1b[0m")
}
