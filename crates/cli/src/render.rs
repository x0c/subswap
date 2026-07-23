//! 默认入口的渐进式渲染：先出账号骨架，quota 拉到一个刷一个。
//!
//! 设计要点：
//! - 交互终端用 ANSI `\x1b[NA\x1b[J` 回到块首再重绘；`N` 按终端物理行数计算，避免长行软换行后旧帧残留。
//!   非交互场景仅在最终态打印一次。
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
use unicode_width::UnicodeWidthChar;

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
        self.rendered_lines = rendered_line_count(&output, terminal_width());
        Ok(())
    }
}

fn terminal_width() -> usize {
    terminal_width_from_os()
        .or_else(terminal_width_from_env)
        .unwrap_or(80)
        .max(1)
}

#[cfg(unix)]
fn terminal_width_from_os() -> Option<usize> {
    use std::os::fd::AsRawFd;

    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // 只读 stdout 的 TTY 尺寸；失败时回退到 COLUMNS/80。
    let result = unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 {
        Some(size.ws_col as usize)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn terminal_width_from_os() -> Option<usize> {
    None
}

fn terminal_width_from_env() -> Option<usize> {
    std::env::var("COLUMNS").ok()?.parse::<usize>().ok()
}

fn rendered_line_count(output: &str, terminal_width: usize) -> usize {
    let width = terminal_width.max(1);
    output
        .split_inclusive('\n')
        .map(|segment| {
            let has_newline = segment.ends_with('\n');
            let text = segment.strip_suffix('\n').unwrap_or(segment);
            if has_newline || !text.is_empty() {
                physical_rows(text, width)
            } else {
                0
            }
        })
        .sum()
}

fn physical_rows(line: &str, terminal_width: usize) -> usize {
    let visible_width = visible_width(line);
    visible_width.div_ceil(terminal_width).max(1)
}

fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            skip_ansi_sequence(&mut chars);
            continue;
        }
        width += ch.width().unwrap_or(0);
    }
    width
}

fn skip_ansi_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    if chars.next_if_eq(&'[').is_none() {
        return;
    }
    for ch in chars.by_ref() {
        if ('@'..='~').contains(&ch) {
            break;
        }
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
        out.push_str(
            "No accounts. Sign in to a supported client, then run `subswap login <provider>`.\n",
        );
        return out;
    }

    let layout = render_layout(snapshots);
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

        for awq in &snap.accounts {
            global_index += 1;
            out.push_str(&render_row(awq, global_index, layout, color));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[derive(Clone, Copy)]
struct RenderLayout {
    index_width: usize,
    name_width: usize,
    quota_width: usize,
}

fn render_layout(snapshots: &[ProviderSnapshot]) -> RenderLayout {
    let account_count = snapshots.iter().map(|s| s.accounts.len()).sum::<usize>();
    let index_width = account_count.to_string().len().max(2);
    let name_width = snapshots
        .iter()
        .flat_map(|s| s.accounts.iter())
        .map(|a| account_name(a).chars().count())
        .max()
        .unwrap_or(0)
        .clamp(16, 36);
    let quota_width = snapshots
        .iter()
        .flat_map(|s| s.accounts.iter())
        .flat_map(|a| a.quotas.iter())
        .filter(|q| quota_has_display_value(q))
        .map(|q| visible_width(&format_quota_compact(q, false)))
        .max()
        .unwrap_or(0);

    RenderLayout {
        index_width,
        name_width,
        quota_width,
    }
}

fn render_row(awq: &AccountWithQuotas, index: usize, layout: RenderLayout, color: bool) -> String {
    let active = awq.account.active;
    let star_plain = if active { "*" } else { " " };
    let star = if active {
        style(color, "1;36", star_plain)
    } else {
        star_plain.into()
    };
    let num_plain = format!("{index:>width$}", width = layout.index_width);
    let num = style(color, "2", &num_plain);

    let name_plain = truncate_to_width(&account_name(awq), layout.name_width);
    let name_padded = format!("{name_plain:<width$}", width = layout.name_width);
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
        QuotaFetchState::Ready => render_quota_parts(&awq.quotas, layout.quota_width, color),
        QuotaFetchState::Stale { cached_at, error } => {
            // 缓存数据 + 「为什么在用缓存」：年龄 + 压缩后的失败原因,让用户一眼看出是限流/网络等。
            let parts = render_quota_parts(&awq.quotas, layout.quota_width, color);
            let age = format_age(*cached_at);
            let reason = compact_error(error);
            let tag = style(color, "2", &format!("(cached ~{age} · {reason})"));
            if parts.is_empty() {
                tag
            } else {
                format!("{parts}  {tag}")
            }
        }
    };

    if body.is_empty() {
        if awq.account.manual_only() {
            let tag = style(color, "2", "custom");
            format!("  {star} {num} {name}  {tag}")
        } else {
            format!("  {star} {num} {name}")
        }
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
    if lower.contains("no keyring entry")
        || lower.contains("no matching entry")
        || lower.contains("no credentials")
    {
        if lower.contains("subswap login codex") {
            return "missing credentials; run `subswap login codex`".into();
        }
        if lower.contains("subswap login claude") {
            return "missing credentials; run `subswap login claude`".into();
        }
        return "missing credentials; re-login".into();
    }
    if lower.contains("credential store") {
        return "keyring error".into();
    }
    // refresh token 作废 → 必须在原生客户端重新登录(parked 自刷无法救回)。
    // 排在 401 之前:它本质也是鉴权失效,但提示更具体。
    if lower.contains("re-login") || lower.contains("invalid_grant") {
        return "needs re-login".into();
    }
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
        if let Some(attempts) = attempt_count(err) {
            return format!("timeout after {attempts} attempts");
        }
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

fn attempt_count(err: &str) -> Option<&str> {
    let after = err.rsplit_once(" after ")?.1;
    let (count, tail) = after.split_once(" attempt")?;
    if count.chars().all(|c| c.is_ascii_digit()) && (tail.starts_with('s') || tail.is_empty()) {
        Some(count)
    } else {
        None
    }
}

pub fn format_quota_compact(q: &Quota, color: bool) -> String {
    let w_label = match q.window {
        QuotaWindow::FiveHour => "5h",
        QuotaWindow::SevenDay => "7d",
        QuotaWindow::Month => "mo",
        QuotaWindow::FirstPartyModels => "First-Party Models",
        QuotaWindow::Api => "API",
        QuotaWindow::Custom => "--",
    };
    // 所有 Provider 统一显示余量；数据层 `Quota.used` 仍是已用百分比，仅在展示层翻转。
    let usage_plain = if q.limit > 0 {
        format!("{:>3}% left", q.limit.saturating_sub(q.used))
    } else {
        "--".into()
    };
    let reset_plain = q
        .reset_at
        .map(format_reset_at)
        .unwrap_or_else(|| "--".into());

    let usage_sgr = status_sgr(q.status);

    let w_styled = style(color, "2", &format!("{w_label:<2}"));
    let bracket_l = style(color, "2", "[");
    let bracket_r = style(color, "2", "]");
    let usage = style(color, usage_sgr, &usage_plain);
    let reset_padded = format!("reset {reset_plain:<6}");
    let reset = style(color, "2", &reset_padded);

    format!("{w_styled} {bracket_l}{usage} {reset}{bracket_r}")
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

/// 窗口的展示顺序：与 provider 返回顺序无关，保证不同 provider 的行在视觉上一致
/// （例如 Kimi 的 `/usages` 先给 7d 再给 5h，若不排序会跟 Claude 的 5h/7d 顺序相反）。
fn window_display_order(window: QuotaWindow) -> u8 {
    match window {
        QuotaWindow::FiveHour => 0,
        QuotaWindow::SevenDay => 1,
        QuotaWindow::Month => 2,
        QuotaWindow::FirstPartyModels => 3,
        QuotaWindow::Api => 4,
        QuotaWindow::Custom => 5,
    }
}

fn render_quota_parts(quotas: &[Quota], quota_width: usize, color: bool) -> String {
    let mut sorted: Vec<&Quota> = quotas
        .iter()
        .filter(|q| quota_has_display_value(q))
        .collect();
    sorted.sort_by_key(|q| window_display_order(q.window));
    let parts: Vec<String> = sorted
        .into_iter()
        .map(|q| pad_visible(format_quota_compact(q, color), quota_width))
        .collect();
    if parts.is_empty() {
        if quotas.is_empty() {
            String::new()
        } else {
            style(color, "2", "quota unknown")
        }
    } else {
        parts.join("  ")
    }
}

fn pad_visible(value: String, width: usize) -> String {
    let padding = width.saturating_sub(visible_width(&value));
    if padding == 0 {
        value
    } else {
        format!("{value}{}", " ".repeat(padding))
    }
}

fn format_age(cached_at: DateTime<Utc>) -> String {
    let delta = Utc::now().signed_duration_since(cached_at);
    let seconds = delta.num_seconds().max(0);
    if seconds < 90 * 60 {
        format!("{}m ago", (seconds + 59) / 60)
    } else if seconds < 48 * 60 * 60 {
        format!("{}h ago", (seconds + 3599) / 3600)
    } else {
        format!("{}d ago", (seconds + 86_399) / 86_400)
    }
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
        assert!(text.starts_with("5h [ 94% left"));
        assert!(text.contains("reset in 2h"));
        assert!(!text.contains("ok"), "status text block must be gone");
        assert!(!text.contains('\x1b'), "plain mode must not emit escapes");
    }

    #[test]
    fn cursor_quota_shows_remaining_like_other_providers() {
        let first_party = format_quota_compact(
            &quota(QuotaWindow::FirstPartyModels, 59, 100, QuotaStatus::Ok),
            false,
        );
        let api = format_quota_compact(&quota(QuotaWindow::Api, 57, 100, QuotaStatus::Ok), false);
        assert!(first_party.starts_with("First-Party Models [ 41% left"));
        assert!(api.starts_with("API [ 43% left"));
        assert!(!first_party.contains("used"));
        assert!(!api.contains("used"));
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

    #[test]
    fn compact_error_preserves_timeout_attempt_count() {
        let text = compact_error("quota fetch: quota fetch timeout after 2 attempts");
        assert_eq!(text, "timeout after 2 attempts");
    }

    #[test]
    fn compact_error_names_missing_credentials() {
        let text = compact_error(
            "credential store: no keyring entry for codex:x:auth_json; run `subswap login codex`",
        );
        assert_eq!(text, "missing credentials; run `subswap login codex`");
    }

    #[test]
    fn compact_error_names_claude_missing_credentials() {
        let text =
            compact_error("no credentials for claude:a@x.com; run `subswap login claude` ...");
        assert_eq!(text, "missing credentials; run `subswap login claude`");
    }

    #[test]
    fn compact_error_names_platform_missing_credentials() {
        let text = compact_error("credential store: No matching entry found in secure storage");
        assert_eq!(text, "missing credentials; re-login");
    }

    #[test]
    fn compact_error_names_dead_refresh_token() {
        // parked 账号 refresh token 作废:展示具体的 re-login 提示,而非泛化的 401。
        assert_eq!(
            compact_error(
                "quota fetch: re-login required for claude:a@x.com; refresh token invalid"
            ),
            "needs re-login"
        );
        assert_eq!(
            compact_error("refresh returned 400 Bad Request: {\"error\":\"invalid_grant\"}"),
            "needs re-login"
        );
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
    fn quota_parts_render_5h_before_7d_regardless_of_provider_return_order() {
        // Kimi 的 /usages 先给 7d(usage 字段)再给 5h(limits[]);Claude 反过来。
        // 渲染必须与 provider 返回顺序无关,统一展示 5h 在前。
        let reversed = vec![
            quota(QuotaWindow::SevenDay, 25, 100, QuotaStatus::Ok),
            quota(QuotaWindow::FiveHour, 23, 100, QuotaStatus::Ok),
        ];
        let rendered = render_quota_parts(&reversed, 0, false);
        let pos_5h = rendered.find("5h").expect("5h label present");
        let pos_7d = rendered.find("7d").expect("7d label present");
        assert!(pos_5h < pos_7d, "expected 5h before 7d, got: {rendered}");
    }

    #[test]
    fn render_to_string_aligns_columns_across_providers() {
        let mut claude = make_awq("long-address@example.com", false, QuotaFetchState::Ready);
        claude.quotas = vec![
            quota(QuotaWindow::FiveHour, 30, 100, QuotaStatus::Ok),
            quota(QuotaWindow::SevenDay, 40, 100, QuotaStatus::Ok),
        ];

        let mut custom = make_awq("DeepSeek", false, QuotaFetchState::Ready);
        custom
            .account
            .extra
            .insert("manual_only".into(), true.into());

        let mut codex = make_awq("x@y.io", true, QuotaFetchState::Ready);
        codex.quotas = vec![
            quota(QuotaWindow::FiveHour, 1, 100, QuotaStatus::Ok),
            quota(QuotaWindow::SevenDay, 92, 100, QuotaStatus::Warn),
        ];

        let text = render_to_string(
            &[
                ProviderSnapshot {
                    provider: "claude".into(),
                    accounts: vec![claude, custom],
                },
                ProviderSnapshot {
                    provider: "codex".into(),
                    accounts: vec![codex],
                },
            ],
            &[],
            false,
        );

        let claude_row = text
            .lines()
            .find(|line| line.contains("long-address@example.com"))
            .unwrap();
        let custom_row = text.lines().find(|line| line.contains("DeepSeek")).unwrap();
        let codex_row = text.lines().find(|line| line.contains("x@y.io")).unwrap();

        let five_hour_col = claude_row.find("5h [").unwrap();
        assert_eq!(codex_row.find("5h ["), Some(five_hour_col), "{text}");
        assert_eq!(custom_row.find("custom"), Some(five_hour_col), "{text}");

        let seven_day_col = claude_row.find("7d [").unwrap();
        assert_eq!(codex_row.find("7d ["), Some(seven_day_col), "{text}");
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

    #[test]
    fn rendered_line_count_includes_soft_wrapped_rows_and_blank_lines() {
        let output = "\x1b[2m1234567890X\x1b[0m\n\n";
        assert_eq!(output.lines().count(), 2);
        assert_eq!(rendered_line_count(output, 10), 3);
    }

    #[test]
    fn rendered_line_count_handles_wide_characters() {
        assert_eq!(rendered_line_count("账号账号账号\n", 5), 3);
    }
}
