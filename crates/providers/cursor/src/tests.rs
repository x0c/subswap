use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use base64::Engine;
use rusqlite::Connection;
use subswap_core::{AccountRegistry, FileStore, Provider, QuotaWindow};

use super::*;

fn jwt(subject: &str, suffix: &str) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::json!({"sub": subject}).to_string());
    format!("x.{payload}.{suffix}")
}

fn setup() -> (tempfile::TempDir, CursorProvider, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("state.vscdb");
    let conn = Connection::open(&db).unwrap();
    conn.execute("CREATE TABLE ItemTable (key TEXT UNIQUE, value TEXT)", [])
        .unwrap();
    drop(conn);
    let store = Arc::new(FileStore::new(temp.path().join("credentials.json")));
    let registry = Arc::new(AccountRegistry::new(temp.path().join("registry.toml")));
    let provider = CursorProvider::with_config(
        store,
        registry,
        CursorProviderConfig {
            state_db: db.clone(),
            usage_url: "http://127.0.0.1:9/usage".into(),
            token_url: "http://127.0.0.1:9/token".into(),
            process_control: Arc::new(NoopProcessControl),
            refresh_lock_dir: temp.path().join("refresh-locks"),
            snapshots_dir: temp.path().join("snapshots"),
        },
    );
    (temp, provider, db)
}

fn configured(
    base: &CursorProvider,
    state_db: PathBuf,
    usage_url: String,
    token_url: String,
    process_control: Arc<dyn CursorProcessControl>,
) -> CursorProvider {
    CursorProvider::with_config(
        base.store.clone(),
        base.registry.clone(),
        CursorProviderConfig {
            state_db,
            usage_url,
            token_url,
            process_control,
            refresh_lock_dir: base.refresh_lock_dir.clone(),
            snapshots_dir: base.snapshots_dir.clone(),
        },
    )
}

fn write_live(db: &Path, email: &str, auth_id: &str, access: &str, refresh: &str) {
    let mut conn = Connection::open(db).unwrap();
    let tx = conn.transaction().unwrap();
    for (key, value) in [
        (ACCESS_KEY, access),
        (REFRESH_KEY, refresh),
        (EMAIL_KEY, email),
        (AUTH_ID_KEY, auth_id),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
            (key, value),
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn write_plan(db: &Path, membership: &str, status: &str, sign_up_type: &str) {
    let conn = Connection::open(db).unwrap();
    for (key, value) in [
        (MEMBERSHIP_KEY, membership),
        (SUBSCRIPTION_STATUS_KEY, status),
        (SIGN_UP_TYPE_KEY, sign_up_type),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
            (key, value),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn imports_and_transactionally_switches_while_capturing_live_owner() {
    let (_temp, provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(
        &db,
        "a@example.com",
        "auth0|user_a",
        &access_a,
        "refresh-a1",
    );
    write_plan(&db, "pro", "active", "oauth");
    let a = provider.import_active(None).await.unwrap();

    let access_b = jwt("auth0|user_b", "b");
    write_live(
        &db,
        "b@example.com",
        "auth0|user_b",
        &access_b,
        "refresh-b1",
    );
    write_plan(&db, "business", "trialing", "sso");
    let b = provider.import_active(None).await.unwrap();

    write_live(
        &db,
        "b@example.com",
        "auth0|user_b",
        &access_b,
        "refresh-b2",
    );
    write_plan(&db, "business", "trialing", "sso");
    provider.activate(&a.id).await.unwrap();

    let conn = Connection::open(db).unwrap();
    assert_eq!(
        read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
        Some("a@example.com")
    );
    assert_eq!(
        read_item(&conn, COMPAT_EMAIL_KEY).unwrap().as_deref(),
        Some("a@example.com")
    );
    assert_eq!(
        read_item(&conn, MEMBERSHIP_KEY).unwrap().as_deref(),
        Some("pro")
    );
    assert_eq!(
        read_item(&conn, SUBSCRIPTION_STATUS_KEY)
            .unwrap()
            .as_deref(),
        Some("active")
    );
    assert_eq!(
        read_item(&conn, SIGN_UP_TYPE_KEY).unwrap().as_deref(),
        Some("oauth")
    );
    let captured = provider.stored_blob(&b).unwrap();
    assert_eq!(captured.refresh_token.as_deref(), Some("refresh-b2"));
    assert!(provider.require_account(&a.id).unwrap().active);
    assert!(!provider.require_account(&b.id).unwrap().active);

    let snapshot_dirs: Vec<_> = std::fs::read_dir(&provider.snapshots_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(snapshot_dirs.len(), 1);
    let cursor_snapshot =
        std::fs::read_to_string(snapshot_dirs[0].join("cursor-state.json")).unwrap();
    assert!(cursor_snapshot.contains("b@example.com"));
    assert!(cursor_snapshot.contains("business"));
    assert!(snapshot_dirs[0].join("registry.toml").is_file());
}

#[test]
fn parses_first_party_and_api_as_used_percentages() {
    let id = AccountId("account".into());
    let value = serde_json::json!({
        "billingCycleEnd": "2026-08-01T00:00:00Z",
        "individualUsage": {"plan": {
            "autoPercentUsed": 59.2,
            "apiPercentUsed": "57"
        }}
    });
    let quotas = parse_usage(&id, &value).unwrap();
    assert_eq!(quotas.len(), 2);
    assert_eq!(quotas[0].window, QuotaWindow::FirstPartyModels);
    assert_eq!(quotas[0].used, 59);
    assert_eq!(quotas[1].window, QuotaWindow::Api);
    assert_eq!(quotas[1].used, 57);
    assert!(quotas.iter().all(|quota| quota.reset_at.is_some()));
}

#[test]
fn cursor_state_db_override_requires_an_absolute_path() {
    assert!(validate_state_db_override(PathBuf::from("relative/state.vscdb")).is_err());
    let absolute = std::env::temp_dir().join("cursor-state.vscdb");
    assert_eq!(
        validate_state_db_override(absolute.clone()).unwrap(),
        absolute
    );
}

struct MockServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
        Self::start_with_first_request_hook(responses, None)
    }

    fn start_with_first_request_hook(
        responses: Vec<(&'static str, &'static str)>,
        first_request_hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = requests.clone();
        let handle = std::thread::spawn(move || {
            let mut first_request_hook = first_request_hook;
            for (index, (status, body)) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0; 16384];
                let count = stream.read(&mut buffer).unwrap();
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buffer[..count]).to_string());
                if index == 0 {
                    if let Some(hook) = first_request_hook.take() {
                        hook();
                    }
                }
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            base,
            requests,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.handle.take().unwrap().join().unwrap();
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

#[tokio::test]
async fn active_401_rereads_rotated_live_token_without_refreshing() {
    let (_temp, base_provider, db) = setup();
    let old = jwt("auth0|user_a", "old");
    let new = jwt("auth0|user_a", "new");
    write_live(&db, "a@example.com", "auth0|user_a", &old, "refresh-a");
    let account = base_provider.import_active(None).await.unwrap();
    let rotate_db = db.clone();
    let server = MockServer::start_with_first_request_hook(
        vec![
            ("401 Unauthorized", "{}"),
            (
                "200 OK",
                r#"{"individualUsage":{"plan":{"autoPercentUsed":10,"apiPercentUsed":20}}}"#,
            ),
        ],
        Some(Box::new(move || {
            write_live(
                &rotate_db,
                "a@example.com",
                "auth0|user_a",
                &new,
                "refresh-a",
            );
        })),
    );
    let provider = configured(
        &base_provider,
        db.clone(),
        format!("{}/usage", server.base),
        format!("{}/token", server.base),
        Arc::new(NoopProcessControl),
    );
    let quotas = provider.query_quota(&account.id).await.unwrap();
    assert_eq!(quotas.len(), 2);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.starts_with("GET /usage")));
}

#[tokio::test]
async fn parked_401_rotates_full_token_pair_then_retries() {
    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    let parked = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    base_provider.import_active(None).await.unwrap();

    let rotated = jwt("auth0|user_a", "rotated");
    let refresh_body = serde_json::json!({
        "access_token": rotated,
        "refresh_token": "refresh-a2"
    })
    .to_string();
    let usage_body =
        r#"{"individual_usage":{"plan":{"auto_percent_used":59,"api_percent_used":57}}}"#;
    let server = MockServer::start(vec![
        ("401 Unauthorized", "{}"),
        ("200 OK", Box::leak(refresh_body.into_boxed_str())),
        ("200 OK", usage_body),
    ]);
    let provider = configured(
        &base_provider,
        db,
        format!("{}/usage", server.base),
        format!("{}/token", server.base),
        Arc::new(NoopProcessControl),
    );
    let quotas = provider.query_quota(&parked.id).await.unwrap();
    assert_eq!(quotas[0].used, 59);
    let stored = provider.stored_blob(&parked).unwrap();
    assert_eq!(stored.refresh_token.as_deref(), Some("refresh-a2"));
    let requests = server.finish();
    assert!(requests[0].starts_with("GET /usage"));
    assert!(requests[1].starts_with("POST /token"));
    assert!(requests[2].starts_with("GET /usage"));
}

#[tokio::test]
async fn concurrent_parked_refresh_uses_one_rotating_token_request() {
    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "old");
    write_live(
        &db,
        "a@example.com",
        "auth0|user_a",
        &access_a,
        "refresh-a1",
    );
    let parked = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    base_provider.import_active(None).await.unwrap();

    let rotated = jwt("auth0|user_a", "rotated");
    let refresh_body = Box::leak(
        serde_json::json!({
            "access_token": rotated,
            "refresh_token": "refresh-a2"
        })
        .to_string()
        .into_boxed_str(),
    );
    let server = MockServer::start(vec![("200 OK", refresh_body)]);
    let provider = configured(
        &base_provider,
        db,
        format!("{}/usage", server.base),
        format!("{}/token", server.base),
        Arc::new(NoopProcessControl),
    );
    let original = provider.stored_blob(&parked).unwrap();
    let (first, second) = tokio::join!(
        provider.refresh_parked(&parked, original.clone()),
        provider.refresh_parked(&parked, original)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.access_token, second.access_token);
    assert_eq!(first.refresh_token.as_deref(), Some("refresh-a2"));
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /token"));
}

#[tokio::test]
async fn dead_refresh_token_tombstone_prevents_request_storm() {
    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "old");
    write_live(
        &db,
        "a@example.com",
        "auth0|user_a",
        &access_a,
        "dead-refresh",
    );
    let parked = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    base_provider.import_active(None).await.unwrap();
    let server = MockServer::start(vec![("401 Unauthorized", "{}")]);
    let provider = configured(
        &base_provider,
        db,
        format!("{}/usage", server.base),
        format!("{}/token", server.base),
        Arc::new(NoopProcessControl),
    );
    let original = provider.stored_blob(&parked).unwrap();
    assert!(provider
        .refresh_parked(&parked, original.clone())
        .await
        .is_err());
    assert!(provider.refresh_parked(&parked, original).await.is_err());
    assert_eq!(server.finish().len(), 1);
}

struct NoopProcessControl;

impl CursorProcessControl for NoopProcessControl {
    fn is_running(&self) -> Result<bool> {
        Ok(false)
    }
    fn stop(&self) -> Result<()> {
        Ok(())
    }
    fn start(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn start_failure_rolls_back_database_and_registry_in_order() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingProcessControl {
        db: PathBuf,
        events: Arc<Mutex<Vec<&'static str>>>,
        starts: AtomicUsize,
    }
    impl CursorProcessControl for FailingProcessControl {
        fn is_running(&self) -> Result<bool> {
            Ok(true)
        }
        fn stop(&self) -> Result<()> {
            let conn = Connection::open(&self.db).unwrap();
            assert_eq!(
                read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
                Some("b@example.com")
            );
            self.events.lock().unwrap().push("stop-old");
            Ok(())
        }
        fn start(&self) -> Result<()> {
            let call = self.starts.fetch_add(1, Ordering::SeqCst);
            let conn = Connection::open(&self.db).unwrap();
            if call == 0 {
                assert_eq!(
                    read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
                    Some("a@example.com")
                );
                assert_eq!(
                    read_item(&conn, MEMBERSHIP_KEY).unwrap().as_deref(),
                    Some("pro")
                );
                self.events.lock().unwrap().push("start-new");
                Err(Error::Provider("simulated start failure".into()))
            } else {
                assert_eq!(
                    read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
                    Some("b@example.com")
                );
                assert_eq!(
                    read_item(&conn, MEMBERSHIP_KEY).unwrap().as_deref(),
                    Some("business")
                );
                self.events.lock().unwrap().push("restart-old");
                Ok(())
            }
        }
    }

    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    write_plan(&db, "pro", "active", "oauth");
    let a = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    write_plan(&db, "business", "trialing", "sso");
    let b = base_provider.import_active(None).await.unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = configured(
        &base_provider,
        db.clone(),
        "http://127.0.0.1:9/usage".into(),
        "http://127.0.0.1:9/token".into(),
        Arc::new(FailingProcessControl {
            db: db.clone(),
            events: events.clone(),
            starts: AtomicUsize::new(0),
        }),
    );

    let error = provider.activate(&a.id).await.unwrap_err().to_string();
    assert!(error.contains("rolled back"));
    let conn = Connection::open(db).unwrap();
    assert_eq!(
        read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
        Some("b@example.com")
    );
    assert!(provider.require_account(&b.id).unwrap().active);
    assert!(!provider.require_account(&a.id).unwrap().active);
    assert_eq!(
        *events.lock().unwrap(),
        vec!["stop-old", "start-new", "restart-old"]
    );
}

#[tokio::test]
async fn stop_failure_does_not_touch_database_or_registry() {
    struct StopFails;
    impl CursorProcessControl for StopFails {
        fn is_running(&self) -> Result<bool> {
            Ok(true)
        }
        fn stop(&self) -> Result<()> {
            Err(Error::Provider("simulated stop failure".into()))
        }
        fn start(&self) -> Result<()> {
            panic!("start must not run when stop itself failed")
        }
    }

    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    let a = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    let b = base_provider.import_active(None).await.unwrap();
    let provider = configured(
        &base_provider,
        db.clone(),
        "http://127.0.0.1:9/usage".into(),
        "http://127.0.0.1:9/token".into(),
        Arc::new(StopFails),
    );

    assert!(provider.activate(&a.id).await.is_err());
    let conn = Connection::open(db).unwrap();
    assert_eq!(
        read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
        Some("b@example.com")
    );
    assert!(provider.require_account(&b.id).unwrap().active);
    assert!(!provider.require_account(&a.id).unwrap().active);
}

#[tokio::test]
async fn registry_active_failure_restores_database_before_reopening_cursor() {
    struct RemoveTargetOnStop {
        registry: Arc<AccountRegistry>,
        target: AccountId,
        db: PathBuf,
    }
    impl CursorProcessControl for RemoveTargetOnStop {
        fn is_running(&self) -> Result<bool> {
            Ok(true)
        }
        fn stop(&self) -> Result<()> {
            self.registry.remove(PROVIDER_ID, &self.target)
        }
        fn start(&self) -> Result<()> {
            let conn = Connection::open(&self.db).unwrap();
            assert_eq!(
                read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
                Some("b@example.com")
            );
            Ok(())
        }
    }

    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    let a = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    let b = base_provider.import_active(None).await.unwrap();
    let provider = configured(
        &base_provider,
        db.clone(),
        "http://127.0.0.1:9/usage".into(),
        "http://127.0.0.1:9/token".into(),
        Arc::new(RemoveTargetOnStop {
            registry: base_provider.registry.clone(),
            target: a.id.clone(),
            db: db.clone(),
        }),
    );

    assert!(provider.activate(&a.id).await.is_err());
    let conn = Connection::open(db).unwrap();
    assert_eq!(
        read_item(&conn, EMAIL_KEY).unwrap().as_deref(),
        Some("b@example.com")
    );
    assert!(provider.require_account(&b.id).unwrap().active);
    assert!(provider
        .registry
        .find(PROVIDER_ID, &a.id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn reconcile_marks_the_known_live_owner_active_after_native_account_change() {
    let (_temp, provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    let a = provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    let b = provider.import_active(None).await.unwrap();

    // 模拟 registry 仍指向 A，但用户已在原生 Cursor 内切到已导入的 B。
    provider.registry.set_active(PROVIDER_ID, &a.id).unwrap();
    provider.reconcile_active_from_live().await.unwrap();

    assert!(!provider.require_account(&a.id).unwrap().active);
    assert!(provider.require_account(&b.id).unwrap().active);
}

#[tokio::test]
async fn active_account_never_refreshes_when_live_database_is_unreadable() {
    let (_temp, base_provider, db) = setup();
    let access = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access, "refresh-a");
    let account = base_provider.import_active(None).await.unwrap();
    std::fs::remove_file(&db).unwrap();
    std::fs::write(&db, "not a sqlite database").unwrap();

    let server = MockServer::start(vec![("401 Unauthorized", "{}")]);
    let provider = configured(
        &base_provider,
        db,
        format!("{}/usage", server.base),
        format!("{}/token", server.base),
        Arc::new(NoopProcessControl),
    );
    assert!(provider.query_quota(&account.id).await.is_err());
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /usage"));
}

#[tokio::test]
async fn inactive_registry_account_that_owns_live_db_only_rereads_live_on_401() {
    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    let a = base_provider.import_active(None).await.unwrap();
    let old_b = jwt("auth0|user_b", "old");
    let new_b = jwt("auth0|user_b", "new");
    write_live(&db, "b@example.com", "auth0|user_b", &old_b, "refresh-b");
    let b = base_provider.import_active(None).await.unwrap();
    base_provider
        .registry
        .set_active(PROVIDER_ID, &a.id)
        .unwrap();

    let rotate_db = db.clone();
    let server = MockServer::start_with_first_request_hook(
        vec![
            ("401 Unauthorized", "{}"),
            (
                "200 OK",
                r#"{"individualUsage":{"plan":{"autoPercentUsed":11,"apiPercentUsed":22}}}"#,
            ),
        ],
        Some(Box::new(move || {
            write_live(
                &rotate_db,
                "b@example.com",
                "auth0|user_b",
                &new_b,
                "refresh-b",
            );
        })),
    );
    let provider = configured(
        &base_provider,
        db,
        format!("{}/usage", server.base),
        format!("{}/token", server.base),
        Arc::new(NoopProcessControl),
    );
    let quotas = provider.query_quota(&b.id).await.unwrap();
    assert_eq!(quotas[0].used, 11);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.starts_with("GET /usage")));
}

#[tokio::test]
async fn concurrent_switches_are_serialized_by_provider_lock() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CriticalProbe {
        current: AtomicUsize,
        max: AtomicUsize,
    }
    impl CursorProcessControl for CriticalProbe {
        fn is_running(&self) -> Result<bool> {
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.max.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(100));
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(false)
        }
        fn stop(&self) -> Result<()> {
            Ok(())
        }
        fn start(&self) -> Result<()> {
            Ok(())
        }
    }

    let (_temp, base_provider, db) = setup();
    let access_a = jwt("auth0|user_a", "a");
    write_live(&db, "a@example.com", "auth0|user_a", &access_a, "refresh-a");
    let a = base_provider.import_active(None).await.unwrap();
    let access_b = jwt("auth0|user_b", "b");
    write_live(&db, "b@example.com", "auth0|user_b", &access_b, "refresh-b");
    let b = base_provider.import_active(None).await.unwrap();
    let probe = Arc::new(CriticalProbe {
        current: AtomicUsize::new(0),
        max: AtomicUsize::new(0),
    });
    let provider = configured(
        &base_provider,
        db,
        "http://127.0.0.1:9/usage".into(),
        "http://127.0.0.1:9/token".into(),
        probe.clone(),
    );
    let (first, second) = tokio::join!(provider.activate(&a.id), provider.activate(&b.id));
    first.unwrap();
    second.unwrap();
    assert_eq!(probe.max.load(Ordering::SeqCst), 1);
}
