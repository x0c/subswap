//! 默认入口的渐进式渲染：先出账号骨架，quota 拉到一个刷一个。
//!
//! 设计要点：
//! - 交互终端用 ANSI `\x1b[NA\x1b[J` 回到块首再重绘；非交互场景仅在最终态打印一次。
//! - 不区分 stdout / stderr：所有可视输出都走 stdout 一条线，便于 `subswap | tee` 抓取。
//! - **视觉分层**：交互终端下用 ANSI dim/color 区分轻重点 —— 次要信息（编号、reset 时间、ok 状态）灰掉，
//!   告警（warn=黄、full=红）和当前激活账号（cyan/bold *）保留醒目色；非交互或重定向时全部退化为纯文本。
//! - **编号**：每行前打全局编号（跨 provider 连续）。`subswap swap N` 与 `subswap rm N` 使用 `AppContext::list_ordered`
//!   生成相同顺序，保证编号一致。

use std::io::{self, Write};

use anyhow::Result;
use chrono::{DateTime, Utc};
use subswap_core::{
    AccountWithQuotas, ProviderSnapshot, Quota, QuotaFetchState, QuotaStatus, QuotaWindow,
};

pub struct InlineRenderer {
    enabled: bool,
    rendered_lines: usize,
}

impl InlineRenderer {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            rendered_lines: 0,
        }
    }

    pub fn render(
        &mut self,
        snapshots: &[ProviderSnapshot],
        auto_lines: &[AutoLine],
    ) -> Result<()> {
        let output = render_to_string(snapshots, auto_lines, self.enabled);
        if self.enabled && self.rendered_lines > 0 {
            print!("\x1b[{}A\x1b[J", self.rendered_lines);
        }
        print!("{output}");
        io::stdout().flush()?;
        self.rendered_lines = output.lines().count();
        Ok(())
    }
}

/// 默认入口里被 AutoSwapPolicy 触发的提示行。挂在对应 Provider 块顶部展示。
pub struct AutoLine {
    pub provider: String,
    pub text: String,
    pub kind: AutoLineKind,
}

#[derive(Clone, Copy)]
pub enum AutoLineKind {
    /// 成功切换、信息类。bold cyan。
    Info,
    /// 失败 / degrade。red。
    Error,
}

pub fn render_to_string(
    snapshots: &[ProviderSnapshot],
    auto_lines: &[AutoLine],
    color: bool,
) -> String {
    let mut out = String::new();
    let has_any = snapshots.iter().any(|s| !s.accounts.is_empty());
    if !has_any {
        out.push_str("No accounts. Log in to Claude Code or Codex CLI, then re-run `subswap`.\n");
        return out;
    }

    let mut global_index: usize = 0;
    for snap in snapshots {
        if snap.accounts.is_empty() {
            continue;
        }
        out.push_str(&format!("{}\n", style(color, "1", &snap.provider)));

        for line in auto_lines
            .iter()
            .filter(|line| line.provider == snap.provider)
        {
            let sgr = match line.kind {
                AutoLineKind::Info => "1;36",
                AutoLineKind::Error => "31",
            };
            out.push_str(&format!(
                "  {}\n",
                style(color, sgr, &format!("! {}", line.text))
            ));
        }

        let name_width = snap
            .accounts
            .iter()
            .map(|a| account_name(a).chars().count())
            .max()
            .unwrap_or(0)
            .clamp(16, 36);
        for awq in &snap.accounts {
            global_index += 1;
            out.push_str(&render_row(awq, global_index, name_width, color));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn render_row(awq: &AccountWithQuotas, index: usize, name_width: usize, color: bool) -> String {
    let active = awq.account.active;
    let star_plain = if active { "*" } else { " " };
    let star = if active {
        style(color, "1;36", star_plain)
    } else {
        star_plain.into()
    };
    let num_plain = format!("{index:>2}");
    let num = style(color, "2", &num_plain);

    let name_plain = truncate_to_width(&account_name(awq), name_width);
    let name_padded = format!("{name_plain:<name_width$}");
    // 激活账号保持默认色（视觉上比 dim 的兄弟亮），非激活整体灰掉。
    let name = if active {
        name_padded
    } else {
        style(color, "2", &name_padded)
    };

    let body = match &awq.fetch_state {
        QuotaFetchState::Loading => style(color, "2", "quota loading"),
        QuotaFetchState::Failed(err) => {
            let text = format!("quota {}", compact_error(err));
            // auth/rate-limit 用红，其他错误（network/timeout）也用红——
            // 失败状态本身就是高 signal，不需要再细分。
            style(color, "31", &text)
        }
        QuotaFetchState::Ready => {
            let parts: Vec<String> = awq
                .quotas
                .iter()
                .filter(|q| quota_has_display_value(q))
                .map(|q| format_quota_compact(q, color))
                .collect();
            if parts.is_empty() {
                if awq.quotas.is_empty() {
                    String::new()
                } else {
                    style(color, "2", "quota unknown")
                }
            } else {
                parts.join("  ")
            }
        }
    };

    if body.is_empty() {
        format!("  {star} {num} {name}")
    } else {
        format!("  {star} {num} {name}  {body}")
    }
}

pub fn account_name(awq: &AccountWithQuotas) -> String {
    if awq.account.label.trim().is_empty() {
        account_ref(&awq.account.id.0)
    } else {
        awq.account.label.clone()
    }
}

/// 把形如 `org-foo::alice@x.com` 的复合 id 截取展示用尾段。
pub fn account_ref(value: &str) -> String {
    value
        .rsplit_once("::")
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_else(|| value.to_string())
}

pub fn truncate_to_width(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    let keep = width - 1;
    format!("{}…", value.chars().take(keep).collect::<String>())
}

/// 用于把 auto policy 的 reason 字符串归类到易读短语；正则/关键词依赖与 core 一致。
pub fn compact_policy_reason(reason: &str) -> String {
    if reason.contains("no swap candidate") {
        "no candidate".into()
    } else if reason.contains("quota fetch failed") {
        "quota unavailable".into()
    } else {
        compact_error(reason)
    }
}

/// 把 reqwest/底层错误压成一行用户友好短语。语义粗判，不需要精确分类。
pub fn compact_error(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("invalid authentication credentials")
    {
        return "401 auth failed".into();
    }
    if lower.contains("429") || lower.contains("rate limit") {
        return "429 rate limited".into();
    }
    if lower.contains("timeout") {
        return "timeout".into();
    }
    if lower.contains("network") || lower.contains("request ") {
        return "network error".into();
    }
    if lower.contains("parse") || lower.contains("not json") {
        return "bad response".into();
    }
    if lower.contains("missing") {
        return "missing metadata".into();
    }
    err.split(':').next().unwrap_or("error").trim().to_string()
}

pub fn format_quota_compact(q: &Quota, color: bool) -> String {
    let w_label = match q.window {
        QuotaWindow::FiveHour => "5h",
        QuotaWindow::SevenDay => "7d",
        QuotaWindow::Month => "mo",
        QuotaWindow::Custom => "--",
    };
    let s_label = match q.status {
        QuotaStatus::Ok => "ok",
        QuotaStatus::Warn => "warn",
        QuotaStatus::Exhausted => "full",
        QuotaStatus::Unknown => "--",
    };
    let usage_plain = if q.limit > 0 {
        format!("{:>3}%", q.used)
    } else {
        "--".into()
    };
    let reset_plain = q
        .reset_at
        .map(format_reset_at)
        .unwrap_or_else(|| "--".into());

    let usage_sgr = status_sgr(q.status);
    let status_sgr_str = status_sgr(q.status);

    let w_styled = style(color, "2", &format!("{w_label:<2}"));
    let bracket_l = style(color, "2", "[");
    let bracket_r = style(color, "2", "]");
    let usage_padded = format!("{usage_plain:>4}");
    let usage = style(color, usage_sgr, &usage_padded);
    let status_padded = format!("{s_label:<4}");
    let status = style(color, status_sgr_str, &status_padded);
    let reset_padded = format!("reset {reset_plain:<6}");
    let reset = style(color, "2", &reset_padded);

    format!("{w_styled} {bracket_l}{usage} {status} {reset}{bracket_r}")
}

fn status_sgr(status: QuotaStatus) -> &'static str {
    match status {
        // ok 灰掉，让用户视线略过；warn 黄，full 红+加粗，Unknown 也灰。
        QuotaStatus::Ok | QuotaStatus::Unknown => "2",
        QuotaStatus::Warn => "33",
        QuotaStatus::Exhausted => "1;31",
    }
}

pub fn quota_has_display_value(q: &Quota) -> bool {
    q.limit > 0 || q.reset_at.is_some() || !matches!(q.status, QuotaStatus::Unknown)
}

pub fn format_reset_at(reset_at: DateTime<Utc>) -> String {
    let delta = reset_at.signed_duration_since(Utc::now());
    let seconds = delta.num_seconds();
    if seconds <= 0 {
        "now".into()
    } else if seconds < 90 * 60 {
        format!("in {}m", (seconds + 59) / 60)
    } else if seconds < 48 * 60 * 60 {
        format!("in {}h", (seconds + 3599) / 3600)
    } else {
        format!("in {}d", (seconds + 86_399) / 86_400)
    }
}

/// ANSI SGR 包装；`color=false` 时直接返回原文，便于管道 / `--no-color` 场景。
fn style(color: bool, sgr: &str, body: &str) -> String {
    if !color || sgr.is_empty() {
        return body.to_string();
    }
    format!("\x1b[{sgr}m{body}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use subswap_core::AccountId;

    fn quota(window: QuotaWindow, used: u64, limit: u64, status: QuotaStatus) -> Quota {
        Quota {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            window,
            used,
            limit,
            reset_at: Some(Utc::now() + chrono::Duration::hours(2)),
            status,
            note: None,
        }
    }

    #[test]
    fn quota_format_is_block_like_plain() {
        let text = format_quota_compact(
            &quota(QuotaWindow::FiveHour, 6, 100, QuotaStatus::Ok),
            false,
        );
        assert!(text.starts_with("5h [  6% ok"));
        assert!(text.contains("reset in 2h"));
        assert!(!text.contains('\x1b'), "plain mode must not emit escapes");
    }

    #[test]
    fn quota_format_paints_warn_yellow_full_red() {
        let warn = format_quota_compact(
            &quota(QuotaWindow::FiveHour, 95, 100, QuotaStatus::Warn),
            true,
        );
        assert!(
            warn.contains("\x1b[33m"),
            "warn must use yellow SGR: {warn:?}"
        );
        let full = format_quota_compact(
            &quota(QuotaWindow::FiveHour, 100, 100, QuotaStatus::Exhausted),
            true,
        );
        assert!(
            full.contains("\x1b[1;31m"),
            "exhausted must use bold red: {full:?}"
        );
    }

    #[test]
    fn unknown_quota_without_data_is_hidden() {
        let q = Quota {
            provider: "test".into(),
            account_id: AccountId("a".into()),
            window: QuotaWindow::Month,
            used: 0,
            limit: 0,
            reset_at: None,
            status: QuotaStatus::Unknown,
            note: None,
        };
        assert!(!quota_has_display_value(&q));
    }

    fn make_awq(id: &str, active: bool, fetch: QuotaFetchState) -> AccountWithQuotas {
        AccountWithQuotas {
            account: subswap_core::Account {
                provider: "test".into(),
                id: AccountId(id.into()),
                label: id.into(),
                active,
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                extra: serde_json::Map::new(),
            },
            quotas: Vec::new(),
            fetch_state: fetch,
        }
    }

    #[test]
    fn render_to_string_emits_global_numbers() {
        let snap_a = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![
                make_awq("a@x.com", true, QuotaFetchState::Loading),
                make_awq("b@x.com", false, QuotaFetchState::Loading),
            ],
        };
        let snap_b = ProviderSnapshot {
            provider: "codex".into(),
            accounts: vec![make_awq("c@x.com", false, QuotaFetchState::Loading)],
        };
        let text = render_to_string(&[snap_a, snap_b], &[], false);
        assert!(text.contains(" 1 a@x.com"), "{text}");
        assert!(text.contains(" 2 b@x.com"), "{text}");
        assert!(text.contains(" 3 c@x.com"), "{text}");
    }

    #[test]
    fn render_active_row_has_cyan_star_in_color_mode() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            accounts: vec![make_awq("a@x.com", true, QuotaFetchState::Loading)],
        };
        let text = render_to_string(&[snap], &[], true);
        assert!(text.contains("\x1b[1;36m*"), "{text:?}");
    }
}
