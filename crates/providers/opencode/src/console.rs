//! OpenCode Console 官方登录账号（V2 `opencode auth login opencode` / V1 `opencode console login`）。
//!
//! 与 Go API key（V2 官方数据库的 `opencode-go` 凭证；V1 `auth.json`）分开处理：
//! - Console 账号只存元数据进 registry，secret 永远只读 live `opencode.db`，subswap 绝不刷新
//!   （刷新由官方客户端负责，避免一次性 refresh token 争抢）。
//! - 切换走官方 `opencode auth switch`；额度走 Console `/api/go/status`（Go 的
//!   `/zen/go/v1/usage` 对 Console token 返回 401，不能混用）。
//! - SQLite 与官方子进程调用都是阻塞操作，调用方必须包进 `spawn_blocking`。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::Utc;
use rusqlite::OptionalExtension;
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

use crate::PROVIDER_ID;

/// registry `extra["kind"]` 中 Console 账号的标记值。Go 账号无此键。
pub const KIND_CONSOLE: &str = "console";

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// 是否为 Console 账号（否则按 Go API key 账号处理）。
pub fn is_console_account(account: &Account) -> bool {
    account.extra.get("kind").and_then(|v| v.as_str()) == Some(KIND_CONSOLE)
}

/// live Console 数据库：`<home>/opencode.db`（与 `paths::opencode_home` 同目录）。
pub fn console_db_path(home: &Path) -> PathBuf {
    home.join("opencode.db")
}

/// live 数据库里当前 Console 登录的非敏感元数据 + 仅内存使用的 token。
/// token 只用于当次额度查询，绝不落盘、不进日志。
pub struct ConsoleLive {
    pub credential_id: String,
    pub credential_label: String,
    pub account_id: String,
    pub active: bool,
    pub email: String,
    pub org_id: String,
    pub org_name: String,
    pub server: String,
    pub access_token: String,
}

/// V2 官方数据库中的 Go API key。密钥只留在内存中，不进 registry 或日志。
pub struct GoKeyLive {
    pub credential_id: String,
    pub key: String,
    pub active: bool,
}

/// `None` 表示没有 V2 凭证表（V1/旧版），`Some` 表示 V2 是权威凭证源。
pub fn read_v2_go_keys(home: &Path) -> Result<Option<Vec<GoKeyLive>>> {
    let db = console_db_path(home);
    if !db.exists() {
        return Ok(None);
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| {
                Error::Provider(format!("open OpenCode database {}: {e}", db.display()))
            })?;
    let has_v2: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='credential')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| Error::Provider(format!("inspect OpenCode database: {e}")))?;
    if !has_v2 {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT id, value, COALESCE(active, 0) FROM credential WHERE integration_id = 'opencode-go' ORDER BY COALESCE(active, 0) DESC, id",
    ).map_err(|e| Error::Provider(format!("query OpenCode Go credentials: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .map_err(|e| Error::Provider(format!("read OpenCode Go credentials: {e}")))?;
    rows.map(|row| {
        let (credential_id, value, active) =
            row.map_err(|e| Error::Provider(format!("read OpenCode Go credential: {e}")))?;
        let v: serde_json::Value = serde_json::from_str(&value)
            .map_err(|e| Error::Provider(format!("parse OpenCode Go credential: {e}")))?;
        let key = v
            .get("key")
            .and_then(|k| k.as_str())
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| Error::Provider("OpenCode Go credential has no API key".into()))?
            .to_string();
        Ok(GoKeyLive {
            credential_id,
            key,
            active,
        })
    })
    .collect::<Result<Vec<_>>>()
    .map(Some)
}

/// 读取官方客户端里的所有 Console 凭证，供停用账号查额度和自动切换。
/// V1 的 `account` 表没有可离线列举的 workspace，只导入当前选中的 workspace。
pub fn read_console_accounts(home: &Path) -> Result<Vec<ConsoleLive>> {
    let db = console_db_path(home);
    if !db.exists() {
        return Ok(Vec::new());
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| {
                Error::Provider(format!("open OpenCode database {}: {e}", db.display()))
            })?;
    let has_v2: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='credential')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| Error::Provider(format!("inspect OpenCode database: {e}")))?;
    if !has_v2 {
        return read_v1_account(&conn);
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, label, value, COALESCE(active, 0) FROM credential \
         WHERE integration_id = 'opencode' ORDER BY COALESCE(active, 0) DESC, id",
        )
        .map_err(|e| Error::Provider(format!("query OpenCode Console credentials: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })
        .map_err(|e| Error::Provider(format!("read OpenCode Console credentials: {e}")))?;
    rows.map(|row| {
        let (credential_id, credential_label, value, active) =
            row.map_err(|e| Error::Provider(format!("read OpenCode Console credential: {e}")))?;
        parse_v2_credential(credential_id, credential_label, value, active)
    })
    .collect()
}

/// 官方客户端当前选中的 Console 凭证。
pub fn read_console_live(home: &Path) -> Result<Option<ConsoleLive>> {
    Ok(read_console_accounts(home)?.into_iter().find(|a| a.active))
}

fn read_v1_account(conn: &rusqlite::Connection) -> Result<Vec<ConsoleLive>> {
    let has_v1: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='account_state')",
        [], |row| row.get(0),
    ).map_err(|e| Error::Provider(format!("inspect OpenCode V1 account database: {e}")))?;
    if !has_v1 {
        return Ok(Vec::new());
    }
    let row: Option<(String, String, String, String, String)> = conn
        .query_row(
            "SELECT a.id, a.email, a.url, a.access_token, s.active_org_id \
         FROM account_state s JOIN account a ON a.id = s.active_account_id \
         WHERE s.id = 1 AND s.active_org_id IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| Error::Provider(format!("read OpenCode V1 account: {e}")))?;
    let Some((account_id, email, server, access_token, org_id)) = row else {
        return Ok(Vec::new());
    };
    Ok(vec![ConsoleLive {
        credential_id: account_id.clone(),
        credential_label: email.clone(),
        account_id,
        active: true,
        email,
        org_id,
        org_name: String::new(),
        server,
        access_token,
    }])
}

fn parse_v2_credential(
    credential_id: String,
    credential_label: String,
    value: String,
    active: bool,
) -> Result<ConsoleLive> {
    let v: serde_json::Value = serde_json::from_str(&value)
        .map_err(|e| Error::Provider(format!("parse OpenCode Console credential: {e}")))?;
    let access_token = v
        .get("access")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Provider("OpenCode Console credential has no access token".into()))?
        .to_string();
    let meta = v.get("metadata");
    let field = |keys: &[&str]| {
        keys.iter()
            .filter_map(|k| meta.and_then(|m| m.get(k)).and_then(|s| s.as_str()))
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from)
    };
    let org_id = field(&["orgID", "org_id", "workspaceID", "workspace_id"]).ok_or_else(|| {
        Error::Provider("OpenCode Console credential has no organization id".into())
    })?;
    let account_id = field(&["accountID", "account_id", "userID", "user_id"])
        .unwrap_or_else(|| credential_id.clone());
    let email = field(&["email"]).unwrap_or_default();
    let org_name = field(&["orgName", "org_name"]).unwrap_or_default();
    let server = field(&["server", "enterpriseUrl", "enterprise_url"])
        .ok_or_else(|| Error::Provider("OpenCode Console credential has no server URL".into()))?;
    Ok(ConsoleLive {
        credential_id,
        credential_label,
        account_id,
        active,
        email,
        org_id,
        org_name,
        server,
        access_token,
    })
}

/// Console 账号主键同时包含官方账号与 workspace，防止同一 workspace 的不同用户串号。
pub fn account_id_for(live: &ConsoleLive) -> AccountId {
    AccountId(format!("console-{}-{}", live.account_id, live.org_id))
}

/// 由 live 元数据构造 registry 账号（不写 secret；调用方 upsert + set_active）。
pub fn account_from_live(live: &ConsoleLive, existing: Option<&Account>) -> Account {
    let id = account_id_for(live);
    let label = if !live.email.trim().is_empty() {
        live.email.clone()
    } else if !live.org_name.trim().is_empty() {
        live.org_name.clone()
    } else {
        id.0.clone()
    };
    let mut extra = serde_json::Map::new();
    extra.insert("kind".into(), serde_json::Value::from(KIND_CONSOLE));
    extra.insert("email".into(), serde_json::Value::from(live.email.clone()));
    extra.insert(
        "account_id".into(),
        serde_json::Value::from(live.account_id.clone()),
    );
    extra.insert(
        "org_id".into(),
        serde_json::Value::from(live.org_id.clone()),
    );
    extra.insert(
        "org_name".into(),
        serde_json::Value::from(live.org_name.clone()),
    );
    extra.insert(
        "credential_id".into(),
        serde_json::Value::from(live.credential_id.clone()),
    );
    extra.insert(
        "credential_label".into(),
        serde_json::Value::from(live.credential_label.clone()),
    );
    extra.insert(
        "server".into(),
        serde_json::Value::from(live.server.clone()),
    );
    extra.insert("dedup_key".into(), serde_json::Value::from(id.0.clone()));
    Account {
        provider: PROVIDER_ID.into(),
        id,
        label,
        active: existing.is_some_and(|a| a.active),
        created_at: existing.map(|a| a.created_at).unwrap_or_else(Utc::now),
        last_used_at: existing.and_then(|a| a.last_used_at),
        priority: existing.map(|a| a.priority).unwrap_or(100),
        extra,
    }
}

/// 探测本机 `opencode` 主版本：`opencode --version` → `opencode v2.0.16` 取 `2`。
/// 二进制缺失或解析失败 → `Err`（明确报错，不静默回退）。
pub fn detect_major_version() -> Result<u64> {
    let out = Command::new("opencode")
        .arg("--version")
        .output()
        .map_err(|e| {
            Error::Provider(format!(
                "`opencode --version` failed; is OpenCode installed: {e}"
            ))
        })?;
    parse_major_version(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| Error::Provider("cannot parse `opencode --version` output".into()))
}

fn parse_major_version(output: &str) -> Option<u64> {
    let token = output.split(|c: char| c.is_whitespace()).find_map(|t| {
        let t = t.trim_start_matches('v');
        t.chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
            .then_some(t)
    })?;
    let major = token.split('.').next()?;
    major.parse().ok()
}

/// 官方登录命令参数（不含程序名）。V2 `auth login opencode`，V1 `console login`。
/// V1 未在本机实测，保留分发不断言行为。
pub fn login_args(major: u64) -> Vec<String> {
    if major >= 2 {
        vec!["auth".into(), "login".into(), "opencode".into()]
    } else {
        vec!["console".into(), "login".into()]
    }
}

/// 官方登出命令参数（不含程序名）。V2 `auth logout <integration> <credential>`，非交互。
pub fn logout_args(integration: &str, credential_id: &str) -> Vec<String> {
    vec![
        "auth".into(),
        "logout".into(),
        integration.into(),
        credential_id.into(),
    ]
}

/// 断开官方客户端里指定集成的凭证（V2 非交互 logout）。
/// V1 无可验证的等价命令：明确报错，调用方回退到只清本地。
pub fn logout_credential(integration: &str, credential_id: &str, home: &Path) -> Result<()> {
    let major = detect_major_version()?;
    if major < 2 {
        return Err(Error::Provider(
            "removing OpenCode credentials on V1 is not automated; \
             disconnect in the official client first, then re-run rm"
                .into(),
        ));
    }
    let mut cmd = Command::new("opencode");
    cmd.args(logout_args(integration, credential_id));
    apply_home_env(&mut cmd, home);
    let out = cmd
        .output()
        .map_err(|e| Error::Provider(format!("`opencode auth logout` failed to start: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Provider(format!(
            "`opencode auth logout {integration} {credential_id}` failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}
/// 官方切号到指定集成的凭证（V2 `opencode auth switch <integration> <credential>`，非交互）。
/// V1 无可验证的等价命令：明确报错，提示用户先在官方客户端切好再同步。
pub fn switch_to(integration: &str, credential_id: &str, major: u64, home: &Path) -> Result<()> {
    if major < 2 {
        return Err(Error::Provider(
            "switching OpenCode Console accounts on V1 is not automated; \
             switch in the official client first, then re-run sync"
                .into(),
        ));
    }
    let mut cmd = Command::new("opencode");
    cmd.args(["auth", "switch", integration, credential_id]);
    apply_home_env(&mut cmd, home);
    let out = cmd
        .output()
        .map_err(|e| Error::Provider(format!("`opencode auth switch` failed to start: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Provider(format!(
            "`opencode auth switch {integration} {credential_id}` failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// `SUBSWAP_OPENCODE_HOME` 覆盖了数据目录时，让官方子进程看到同一份 DB：
/// 把子进程的 `XDG_DATA_HOME` 指到覆盖目录的父目录（常规布局即 `<xdg>/opencode`）。
/// 未覆盖时不动环境。
fn apply_home_env(cmd: &mut Command, home: &Path) {
    if let Ok(v) = std::env::var("SUBSWAP_OPENCODE_HOME") {
        if v.trim().is_empty() {
            return;
        }
        let parent = Path::new(&v)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.to_path_buf());
        cmd.env("XDG_DATA_HOME", parent);
    }
}

fn console_base(live: &ConsoleLive) -> String {
    std::env::var("SUBSWAP_OPENCODE_CONSOLE_BASE")
        .unwrap_or_else(|_| live.server.clone())
        .trim_end_matches('/')
        .to_string()
}

fn go_status_url(base: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|e| Error::QuotaFetch(format!("invalid OpenCode Console server URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::QuotaFetch(
            "OpenCode Console server must use HTTP(S)".into(),
        ));
    }
    let root = url.path().trim_end_matches('/');
    let path = if root.ends_with("/console") {
        format!("{root}/api/go/status")
    } else {
        format!("{root}/console/api/go/status")
    };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn micro_cents(v: Option<&serde_json::Value>) -> Option<f64> {
    let v = v?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return Some(n as f64);
    }
    v.as_str()?.replace(',', "").trim().parse().ok()
}

fn reset_at(meter: &serde_json::Value) -> Option<chrono::DateTime<Utc>> {
    let s = meter.get("resetsAt")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn quota_from_meter(
    meter: &serde_json::Value,
    kind: QuotaWindow,
    account: &Account,
) -> Option<Quota> {
    let used = micro_cents(meter.get("usedMicroCents"))?;
    let limit = micro_cents(meter.get("limitMicroCents"))?;
    if !used.is_finite() || !limit.is_finite() || limit <= 0.0 {
        return None;
    }
    let pct = 100.0 * used / limit;
    if !pct.is_finite() {
        return None;
    }
    let used_pct = pct.round().clamp(0.0, 100.0) as u64;
    Some(Quota {
        provider: PROVIDER_ID.into(),
        account_id: account.id.clone(),
        window: kind,
        used: used_pct,
        limit: 100,
        reset_at: reset_at(meter),
        status: QuotaStatus::from_percent(pct),
        note: None,
    })
}

/// 解析 `/console/api/go/status` 响应：`access.meters.fiveHour/week/month`。
/// 缺失窗口直接跳过（未知≠0，绝不把查不出画成 0%）。
pub fn parse_go_status(body: &str, account: &Account) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let meters = v.get("access").and_then(|a| a.get("meters"));
    let Some(meters) = meters else {
        return vec![];
    };
    let mut out = Vec::new();
    for (key, kind) in [
        ("fiveHour", QuotaWindow::FiveHour),
        ("week", QuotaWindow::SevenDay),
        ("month", QuotaWindow::Month),
    ] {
        if let Some(m) = meters.get(key) {
            if let Some(q) = quota_from_meter(m, kind, account) {
                out.push(q);
            }
        }
    }
    out
}

/// 用 Console token 查该 workspace 的 Go 余量（Bearer + `x-org-id`）。
pub async fn fetch_console_quota(live: &ConsoleLive, account: &Account) -> Result<Vec<Quota>> {
    fetch_console_quota_at(live, account, &console_base(live)).await
}

async fn fetch_console_quota_at(
    live: &ConsoleLive,
    account: &Account,
    console_base: &str,
) -> Result<Vec<Quota>> {
    let url = go_status_url(console_base)?;
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::QuotaFetch(format!("opencode console status client failed: {e}")))?;
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", live.access_token))
        .header("x-org-id", &live.org_id)
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            format!("subswap/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("opencode console status request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(Error::QuotaFetch(format!(
            "opencode console status HTTP {status}: needs re-login"
        )));
    }
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!(
            "opencode console status HTTP {status}: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    Ok(parse_go_status(&body, account))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS_SAMPLE: &str = r#"{
      "access": {
        "startsAt": "2026-09-22T06:47:23.000Z",
        "endsAt": "2026-10-22T06:47:23.000Z",
        "meters": {
          "fiveHour": {"startsAt":"2026-09-29T09:06:54.974Z","resetsAt":"2026-09-29T14:06:54.974Z","limitMicroCents":"1200000000","usedMicroCents":"3838750"},
          "week": {"startsAt":"2026-09-28T00:00:00.000Z","resetsAt":"2026-10-05T00:00:00.000Z","limitMicroCents":"3000000000","usedMicroCents":"3838750"},
          "month": {"resetsAt":"2026-10-22T06:47:23.000Z","limitMicroCents":"6000000000","usedMicroCents":"3003838750"}
        }
      }
    }"#;

    fn sample_account() -> Account {
        Account {
            provider: "opencode".into(),
            id: AccountId("console-wrk_x".into()),
            label: "test@example.com".into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn parses_three_meters_as_percent() {
        let q = parse_go_status(STATUS_SAMPLE, &sample_account());
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].window, QuotaWindow::FiveHour);
        assert_eq!((q[0].used, q[0].limit), (0, 100));
        assert_eq!(q[1].window, QuotaWindow::SevenDay);
        assert_eq!(q[1].used, 0);
        assert_eq!(q[2].window, QuotaWindow::Month);
        // 3003838750/6000000000 = 50.06% → 50
        assert_eq!(q[2].used, 50);
        assert!(q[0].reset_at.is_some());
    }

    #[test]
    fn missing_meters_are_unknown_not_zero() {
        let q = parse_go_status(r#"{"access":{}}"#, &sample_account());
        assert!(q.is_empty());
        let q = parse_go_status("not json", &sample_account());
        assert!(q.is_empty());
    }

    #[test]
    fn zero_limit_window_is_skipped() {
        let body =
            r#"{"access":{"meters":{"fiveHour":{"usedMicroCents":"1","limitMicroCents":"0"}}}}"#;
        assert!(parse_go_status(body, &sample_account()).is_empty());
    }

    #[test]
    fn full_meter_is_exhausted() {
        let body = r#"{"access":{"meters":{"week":{"usedMicroCents":"3000000000","limitMicroCents":"3000000000"}}}}"#;
        let q = parse_go_status(body, &sample_account());
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].used, 100);
        assert_eq!(q[0].status, QuotaStatus::Exhausted);
    }

    #[test]
    fn numeric_micro_cents_accepted() {
        let body = r#"{"access":{"meters":{"month":{"usedMicroCents":1500000000,"limitMicroCents":6000000000}}}}"#;
        let q = parse_go_status(body, &sample_account());
        assert_eq!(q[0].used, 25);
    }

    #[test]
    fn console_quota_url_stays_on_credential_server() {
        let url = go_status_url("https://enterprise.example.test/console").unwrap();
        assert_eq!(
            url.as_str(),
            "https://enterprise.example.test/console/api/go/status"
        );
        let url = go_status_url("https://opencode.ai").unwrap();
        assert_eq!(url.as_str(), "https://opencode.ai/console/api/go/status");
    }

    #[test]
    fn console_credential_without_server_is_rejected() {
        let value =
            serde_json::json!({"access": "tok", "metadata": {"accountID": "user", "orgID": "wrk"}})
                .to_string();
        assert!(
            parse_v2_credential("cred".into(), "Account".into(), value, true)
                .err()
                .unwrap()
                .to_string()
                .contains("no server URL")
        );
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_major_version("opencode v2.0.16\n"), Some(2));
        assert_eq!(parse_major_version("opencode 1.18.30"), Some(1));
        assert_eq!(parse_major_version(""), None);
    }

    #[test]
    fn login_args_split_by_major() {
        assert_eq!(login_args(2), vec!["auth", "login", "opencode"]);
        assert_eq!(login_args(1), vec!["console", "login"]);
    }

    #[test]
    fn logout_args_target_integration_and_credential() {
        assert_eq!(
            logout_args("opencode-go", "cred_x"),
            vec!["auth", "logout", "opencode-go", "cred_x"]
        );
    }

    #[test]
    fn account_id_and_kind() {
        let live = ConsoleLive {
            credential_id: "cred_x".into(),
            credential_label: "Default".into(),
            account_id: "user_x".into(),
            active: true,
            email: "a@b.c".into(),
            org_id: "wrk_1".into(),
            org_name: "Default".into(),
            server: "https://opencode.ai".into(),
            access_token: "tok".into(),
        };
        let acc = account_from_live(&live, None);
        assert_eq!(acc.id.0, "console-user_x-wrk_1");
        assert!(is_console_account(&acc));
        assert!(!is_console_account(&sample_account()));
    }

    #[test]
    fn missing_db_is_not_logged_in() {
        let home = std::env::temp_dir().join("subswap-opencode-no-such-home-xyz");
        assert!(read_console_live(&home).unwrap().is_none());
    }

    #[test]
    fn v2_keeps_two_users_in_one_workspace_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let db = rusqlite::Connection::open(console_db_path(tmp.path())).unwrap();
        db.execute_batch("CREATE TABLE credential (id TEXT, integration_id TEXT, label TEXT, value TEXT, active INTEGER);").unwrap();
        for (id, user, active) in [("cred_a", "user_a", 1), ("cred_b", "user_b", 0)] {
            let value = serde_json::json!({"access": format!("tok_{user}"), "metadata": {"accountID": user, "orgID": "wrk_shared", "email": format!("{user}@example.com"), "server": "https://opencode.ai/console"}}).to_string();
            db.execute(
                "INSERT INTO credential VALUES (?1, 'opencode', ?2, ?3, ?4)",
                rusqlite::params![id, user, value, active],
            )
            .unwrap();
        }
        let accounts = read_console_accounts(tmp.path()).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_ne!(account_id_for(&accounts[0]), account_id_for(&accounts[1]));
        assert_eq!(
            read_console_live(tmp.path()).unwrap().unwrap().account_id,
            "user_a"
        );
    }

    #[test]
    fn v1_reads_selected_account_and_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let db = rusqlite::Connection::open(console_db_path(tmp.path())).unwrap();
        db.execute_batch("CREATE TABLE account (id TEXT, email TEXT, url TEXT, access_token TEXT); CREATE TABLE account_state (id INTEGER, active_account_id TEXT, active_org_id TEXT); INSERT INTO account VALUES ('user_1', 'v1@example.com', 'https://opencode.ai/console', 'tok'); INSERT INTO account_state VALUES (1, 'user_1', 'wrk_1');").unwrap();
        let live = read_console_live(tmp.path()).unwrap().unwrap();
        assert_eq!(account_id_for(&live).0, "console-user_1-wrk_1");
        assert_eq!(live.email, "v1@example.com");
    }
}
