//! `subswap swap [<id|N>]`：显式切换激活账号。手动入口，不依赖网络/quota。
//!
//! 引用形式：
//! - 数字 `N`：与 `subswap` 默认入口列出的全局编号一致（见 [`crate::cmd::resolve_account`]）。
//! - id / label / `provider/id`：保留原 `find_unique` 行为，脚本可继续用。
//! - 无参：打印带编号的账号列表 + Usage 提示；不做任何切换。

use std::io::{self, IsTerminal};

use anyhow::Result;
use serde::Serialize;
use subswap_core::AuditEvent;

use crate::app::AppContext;
use crate::cmd::default::print_status_overview;
use crate::cmd::resolve_account;

/// `subswap --json swap <target>` 的结构化回执：恰好一个 JSON 对象。
/// `notice` 直接取 Provider 能力声明（`post_swap_notice`），不按 provider 名分发；
/// 无提醒时省略该字段，供 Apple 端 `Codable` 可选解码。
#[derive(Serialize)]
struct SwapReceipt<'a> {
    provider: &'a str,
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    notice: Option<&'a str>,
}

pub async fn run(
    ctx: &AppContext,
    id_input: Option<&str>,
    json: bool,
    update_default: bool,
) -> Result<()> {
    let Some(input) = id_input else {
        print_listing(ctx)?;
        return Ok(());
    };

    let acc = resolve_account(ctx, input)?;
    let p = ctx.providers.get(&acc.provider)?;
    let res = p.activate_with_default(&acc.id, update_default).await;
    match res {
        Ok(()) => {
            ctx.audit.append(AuditEvent::ok(
                "activate",
                &acc.provider,
                Some(acc.id.0.as_str()),
            ));
            // 手动保持：该 provider 暂停自动切换一段时间，避免下一轮采样把显式选择掰回去。
            // 写失败只 warn，不挡切换本身（fail-open）。
            if let Err(e) = subswap_core::record_manual_swap(&acc.provider) {
                tracing::warn!(err = %e, provider = %acc.provider, "record manual hold failed");
            }
            if json {
                // 结构化回执：原生激活与审计/registry 落盘已在上面完成；只输出一个
                // JSON 对象，不查 quota、不打状态表、不做 human ack，供 Apple 端消费。
                let receipt = SwapReceipt {
                    provider: acc.provider.as_str(),
                    id: acc.id.0.as_str(),
                    notice: p.post_swap_notice(),
                };
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                println!("swap → {}/{}", acc.provider, acc.id);
                if let Some(notice) = p.post_swap_notice() {
                    println!("{notice}");
                }
                print_status_overview(ctx).await?;
            }
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
        println!(
            "No accounts. Sign in to a supported client, then run `subswap login <provider>`."
        );
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
