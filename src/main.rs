use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{MultiPart, SinglePart, header::ContentType},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use maud::{DOCTYPE, html};
use tracing::{error, info, warn};

// ── Config ──────────────────────────────────────────────────────────────────

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Debug, Clone)]
struct Config {
    smtp_host: String,
    smtp_port: u16,
    server_host: String,
    server_port: u16,
    queue_poll_seconds: u64,
    archive_max_rows: u64,
    archive_cull_interval_seconds: u64,
    db_path: String,
    seed_domains: Vec<String>,
}

impl Config {
    fn load() -> Self {
        let domains_str = env_or("MAYL_DOMAINS", "");
        let seed_domains = if domains_str.is_empty() {
            vec![]
        } else {
            domains_str.split(',').map(|s| s.trim().to_string()).collect()
        };

        Self {
            smtp_host: env_or("MAYL_SMTP_HOST", "localhost"),
            smtp_port: env_parse("MAYL_SMTP_PORT", 1025),
            server_host: env_or("MAYL_SERVER_HOST", "0.0.0.0"),
            server_port: env_parse("MAYL_SERVER_PORT", 8080),
            queue_poll_seconds: env_parse("MAYL_QUEUE_POLL_SECONDS", 5),
            archive_max_rows: env_parse("MAYL_ARCHIVE_MAX_ROWS", 100_000),
            archive_cull_interval_seconds: env_parse("MAYL_ARCHIVE_CULL_INTERVAL_SECONDS", 600),
            db_path: env_or("MAYL_DB_PATH", "mayl.db"),
            seed_domains,
        }
    }
}

// ── Models ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EmailRequest {
    from: String,
    to: Vec<String>,
    subject: String,
    body: String,
    html: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendQuery {
    sync: Option<bool>,
    save: Option<bool>,
}

#[derive(Debug, Serialize)]
struct QueueResponse {
    id: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
    queue_size: i64,
    archive_size: i64,
}

#[derive(Debug, Deserialize)]
struct DomainRequest {
    domain: String,
}

#[derive(Debug, Serialize)]
struct DomainResponse {
    domain: String,
    token: String,
}

#[derive(Debug, Serialize)]
struct DomainListEntry {
    domain: String,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct SmtpRequest {
    user: String,
    pass: String,
}

#[derive(Debug, Serialize)]
struct SmtpStatusResponse {
    configured: bool,
    user: String,
}

// ── App State ───────────────────────────────────────────────────────────────

struct SmtpCredentials {
    user: String,
    pass: String,
}

struct AppState {
    db: Mutex<Connection>,
    config: Config,
    smtp_creds: RwLock<SmtpCredentials>,
}

// ── Database ────────────────────────────────────────────────────────────────

fn init_db(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS email_queue (
            id TEXT PRIMARY KEY,
            status TEXT NOT NULL DEFAULT 'pending',
            from_addr TEXT NOT NULL,
            to_addrs TEXT NOT NULL,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            html TEXT,
            created_at INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT
        );
        CREATE TABLE IF NOT EXISTS email_archive (
            id INTEGER PRIMARY KEY,
            queue_id TEXT NOT NULL,
            from_addr TEXT NOT NULL,
            to_addrs TEXT NOT NULL,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            html TEXT,
            sent_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS domains (
            domain TEXT PRIMARY KEY,
            token TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_queue_status ON email_queue(status);
        CREATE INDEX IF NOT EXISTS idx_archive_sent ON email_archive(id);
        CREATE INDEX IF NOT EXISTS idx_domains_token ON domains(token);",
    )
    .expect("failed to initialize database");
}

fn seed_domains(conn: &Connection, domains: &[String]) {
    for domain in domains {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM domains WHERE domain = ?1",
                [domain],
                |r| r.get(0),
            )
            .unwrap_or(false);

        if !exists {
            let token = uuid::Uuid::new_v4().to_string();
            let now = now_millis();
            match conn.execute(
                "INSERT INTO domains (domain, token, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![domain, token, now],
            ) {
                Ok(_) => info!(domain, token, "seeded domain"),
                Err(e) => warn!(domain, "failed to seed domain: {e}"),
            }
        }
    }
}

// ── Auth ────────────────────────────────────────────────────────────────────

fn extract_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
}

fn extract_domain_from_addr(from: &str) -> Option<String> {
    // Handle "Name <user@domain>" or plain "user@domain"
    let addr = if let Some(start) = from.find('<') {
        let end = from.find('>')?;
        &from[start + 1..end]
    } else {
        from
    };
    addr.split('@').nth(1).map(|d| d.to_lowercase())
}

// ── SMTP ────────────────────────────────────────────────────────────────────

fn build_mailer(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> AsyncSmtpTransport<Tokio1Executor> {
    let tls_params = TlsParameters::builder(host.to_string())
        .dangerous_accept_invalid_certs(true)
        .build()
        .expect("failed to build TLS parameters");

    let mut builder =
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
            .port(port)
            .tls(Tls::Required(tls_params));

    if !user.is_empty() {
        builder = builder.credentials(Credentials::new(
            user.to_string(),
            pass.to_string(),
        ));
    }

    builder.build()
}

async fn send_email(
    state: &AppState,
    from: &str,
    to: &[String],
    subject: &str,
    body: &str,
    html: Option<&str>,
) -> Result<(), String> {
    let creds = state.smtp_creds.read().await;
    let mailer = build_mailer(
        &state.config.smtp_host,
        state.config.smtp_port,
        &creds.user,
        &creds.pass,
    );
    drop(creds);

    let from_mbox: lettre::message::Mailbox = from.parse().map_err(|e| format!("bad from: {e}"))?;

    let mut email_builder = lettre::Message::builder()
        .from(from_mbox)
        .subject(subject);

    for addr in to {
        let mbox: lettre::message::Mailbox =
            addr.parse().map_err(|e| format!("bad to addr '{addr}': {e}"))?;
        email_builder = email_builder.to(mbox);
    }

    let message = if let Some(html_body) = html {
        email_builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(body.to_string()),
                    )
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_HTML)
                            .body(html_body.to_string()),
                    ),
            )
            .map_err(|e| format!("build email: {e}"))?
    } else {
        email_builder
            .body(body.to_string())
            .map_err(|e| format!("build email: {e}"))?
    };

    mailer
        .send(message)
        .await
        .map_err(|e| format!("smtp send: {e}"))?;

    Ok(())
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn index_handler(State(state): State<Arc<AppState>>) -> maud::Markup {
    let (queue_size, archive_size, failed_count, domains) = {
        let db = state.db.lock().await;
        let qs: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM email_queue WHERE status IN ('pending', 'sending')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let ar: i64 = db
            .query_row("SELECT COUNT(*) FROM email_archive", [], |r| r.get(0))
            .unwrap_or(0);
        let fc: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM email_queue WHERE attempts > 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let mut stmt = db.prepare("SELECT domain FROM domains ORDER BY domain").unwrap();
        let ds: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        (qs, ar, fc, ds)
    };

    let smtp_host = &state.config.smtp_host;
    let smtp_port = state.config.smtp_port;
    let (smtp_configured, smtp_user) = {
        let creds = state.smtp_creds.read().await;
        (!creds.user.is_empty(), creds.user.clone())
    };

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "mayl" }
                style {
                    (maud::PreEscaped("
                        * { margin: 0; padding: 0; box-sizing: border-box; }
                        body { font-family: system-ui, -apple-system, sans-serif; background: #0a0a0a; color: #e0e0e0; padding: 2rem; }
                        .container { max-width: 640px; margin: 0 auto; }
                        h1 { font-size: 2rem; margin-bottom: 0.5rem; color: #fff; }
                        .subtitle { color: #888; margin-bottom: 2rem; }
                        .card { background: #161616; border: 1px solid #2a2a2a; border-radius: 8px; padding: 1.25rem; margin-bottom: 1rem; }
                        .card h2 { font-size: 0.875rem; text-transform: uppercase; letter-spacing: 0.05em; color: #888; margin-bottom: 0.75rem; }
                        .stat-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 1rem; }
                        .stat .value { font-size: 1.5rem; font-weight: 600; color: #fff; }
                        .stat .label { font-size: 0.75rem; color: #888; }
                        .domain-list { list-style: none; }
                        .domain-list li { padding: 0.375rem 0; border-bottom: 1px solid #2a2a2a; font-family: monospace; font-size: 0.875rem; }
                        .domain-list li:last-child { border-bottom: none; }
                        .empty { color: #555; font-style: italic; font-size: 0.875rem; }
                        .smtp-info { font-family: monospace; font-size: 0.875rem; color: #aaa; }
                        .routes { font-family: monospace; font-size: 0.875rem; }
                        .routes dt { color: #6cb6ff; }
                        .routes dd { color: #888; margin-bottom: 0.5rem; margin-left: 1rem; }
                        .add-domain { display: flex; gap: 0.5rem; margin-top: 0.75rem; }
                        .add-domain input { flex: 1; padding: 0.375rem 0.5rem; background: #0a0a0a; border: 1px solid #333; border-radius: 4px; color: #e0e0e0; font-family: monospace; font-size: 0.875rem; }
                        .add-domain button { padding: 0.375rem 0.75rem; background: #2a2a2a; border: 1px solid #444; border-radius: 4px; color: #e0e0e0; cursor: pointer; font-size: 0.875rem; }
                        .add-domain button:hover { background: #333; }
                        #domain-result { margin-top: 0.5rem; font-family: monospace; font-size: 0.8rem; word-break: break-all; }
                        #domain-result.ok { color: #4ec970; }
                        #domain-result.err { color: #f85149; }
                    "))
                }
            }
            body {
                .container {
                    h1 { "mayl" }
                    p.subtitle { "email sending API" }

                    .card {
                        h2 { "Status" }
                        .stat-grid {
                            .stat {
                                .value { (queue_size) }
                                .label { "queued" }
                            }
                            .stat {
                                .value { (archive_size) }
                                .label { "sent" }
                            }
                            .stat {
                                .value { (failed_count) }
                                .label { "retrying" }
                            }
                        }
                    }

                    .card {
                        h2 { "Domains" }
                        @if domains.is_empty() {
                            p.empty { "No domains configured" }
                        } @else {
                            ul.domain-list {
                                @for domain in &domains {
                                    li { (domain) }
                                }
                            }
                        }
                        form.add-domain onsubmit="return addDomain(event)" {
                            input type="text" id="domain-input" placeholder="example.com" required;
                            button type="submit" { "Add" }
                        }
                        div id="domain-result" {}
                        script {
                            (maud::PreEscaped("
                                async function addDomain(e){e.preventDefault();const r=document.getElementById('domain-result'),i=document.getElementById('domain-input');r.className='';r.textContent='';try{const res=await fetch('/domains',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({domain:i.value})});const d=await res.json();if(res.ok){r.className='ok';r.textContent='Token: '+d.token;i.value='';location.reload()}else{r.className='err';r.textContent=d.error}}catch(ex){r.className='err';r.textContent=ex.message}}
                            "))
                        }
                    }

                    .card {
                        h2 { "SMTP" }
                        p.smtp-info { (smtp_host) ":" (smtp_port) }
                        @if smtp_configured {
                            p.smtp-info { "credentials: " (smtp_user) }
                        } @else {
                            p.empty { "no credentials configured" }
                        }
                    }

                    .card {
                        h2 { "API" }
                        dl.routes {
                            dt { "POST /domains" }
                            dd { "Register a domain, get a token" }
                            dt { "GET /domains" }
                            dd { "List registered domains" }
                            dt { "DELETE /domains/:domain" }
                            dd { "Remove a domain" }
                            dt { "GET /smtp" }
                            dd { "SMTP credential status" }
                            dt { "POST /smtp" }
                            dd { "Set SMTP credentials" }
                            dt { "POST /email" }
                            dd { "Queue an email (Authorization: Bearer <token>)" }
                            dt { "POST /email?sync=true" }
                            dd { "Send immediately" }
                            dt { "GET /health" }
                            dd { "Queue and archive stats (JSON)" }
                        }
                    }
                }
            }
        }
    }
}

async fn health_handler(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let db = state.db.lock().await;

    let queue_size: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM email_queue WHERE status IN ('pending', 'sending')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let archive_size: i64 = db
        .query_row("SELECT COUNT(*) FROM email_archive", [], |r| r.get(0))
        .unwrap_or(0);

    Json(HealthResponse {
        status: "ok".into(),
        queue_size,
        archive_size,
    })
}

// ── Domain Handlers ─────────────────────────────────────────────────────────

async fn create_domain_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<DomainRequest>,
) -> Result<(StatusCode, Json<DomainResponse>), (StatusCode, Json<ErrorResponse>)> {
    let domain = payload.domain.trim().to_lowercase();

    if domain.is_empty() || !domain.contains('.') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid domain".into(),
            }),
        ));
    }

    let token = uuid::Uuid::new_v4().to_string();
    let now = now_millis();

    let db = state.db.lock().await;
    db.execute(
        "INSERT INTO domains (domain, token, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![domain, token, now],
    )
    .map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "domain already exists".into(),
            }),
        )
    })?;

    info!(domain, "domain registered");
    Ok((
        StatusCode::CREATED,
        Json(DomainResponse { domain, token }),
    ))
}

async fn list_domains_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<DomainListEntry>> {
    let db = state.db.lock().await;
    let mut stmt = db
        .prepare("SELECT domain, created_at FROM domains ORDER BY domain")
        .unwrap();
    let domains: Vec<DomainListEntry> = stmt
        .query_map([], |row| {
            Ok(DomainListEntry {
                domain: row.get(0)?,
                created_at: row.get(1)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Json(domains)
}

async fn delete_domain_handler(
    State(state): State<Arc<AppState>>,
    Path(domain): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let domain = domain.to_lowercase();
    let db = state.db.lock().await;
    let deleted = db
        .execute("DELETE FROM domains WHERE domain = ?1", [&domain])
        .unwrap_or(0);

    if deleted == 0 {
        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "domain not found".into(),
            }),
        ))
    } else {
        info!(domain, "domain deleted");
        Ok(StatusCode::NO_CONTENT)
    }
}

// ── SMTP Config Handlers ────────────────────────────────────────────────────

async fn get_smtp_handler(
    State(state): State<Arc<AppState>>,
) -> Json<SmtpStatusResponse> {
    let creds = state.smtp_creds.read().await;
    Json(SmtpStatusResponse {
        configured: !creds.user.is_empty(),
        user: creds.user.clone(),
    })
}

async fn set_smtp_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SmtpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    if payload.user.is_empty() || payload.pass.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "user and pass are required".into(),
            }),
        ));
    }

    // Persist to DB
    {
        let db = state.db.lock().await;
        db.execute(
            "INSERT INTO config (key, value) VALUES ('smtp_user', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [&payload.user],
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("db error: {e}"),
                }),
            )
        })?;
        db.execute(
            "INSERT INTO config (key, value) VALUES ('smtp_pass', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [&payload.pass],
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("db error: {e}"),
                }),
            )
        })?;
    }

    // Update in-memory credentials
    {
        let mut creds = state.smtp_creds.write().await;
        creds.user = payload.user;
        creds.pass = payload.pass;
    }

    info!("SMTP credentials updated");
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({"status": "ok"})),
    ))
}

// ── Email Handler ───────────────────────────────────────────────────────────

async fn email_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SendQuery>,
    Json(payload): Json<EmailRequest>,
) -> Result<(StatusCode, Json<QueueResponse>), (StatusCode, Json<ErrorResponse>)> {
    let is_sync = query.sync.unwrap_or(false);
    let save = query.save.unwrap_or(true);

    // Validate token
    let token = extract_token(&headers).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "missing Authorization header".into(),
            }),
        )
    })?;

    // Look up the domain this token authorizes
    let authorized_domain: String = {
        let db = state.db.lock().await;
        db.query_row(
            "SELECT domain FROM domains WHERE token = ?1",
            [&token],
            |r| r.get(0),
        )
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "invalid token".into(),
                }),
            )
        })?
    };

    // Validate from address domain matches token
    let from_domain = extract_domain_from_addr(&payload.from).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid from address".into(),
            }),
        )
    })?;

    if from_domain != authorized_domain {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: format!(
                    "token authorizes domain '{}', but from address uses '{}'",
                    authorized_domain, from_domain
                ),
            }),
        ));
    }

    if payload.to.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "to list is empty".into(),
            }),
        ));
    }

    if is_sync {
        if let Err(e) = send_email(
            &state,
            &payload.from,
            &payload.to,
            &payload.subject,
            &payload.body,
            payload.html.as_deref(),
        )
        .await
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("smtp error: {e}"),
                }),
            ));
        }

        let id = uuid::Uuid::new_v4().to_string();

        if save {
            let now = now_millis();
            let to_json = serde_json::to_string(&payload.to).unwrap();
            let db = state.db.lock().await;
            let _ = db.execute(
                "INSERT INTO email_archive (id, queue_id, from_addr, to_addrs, subject, body, html, sent_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![now, &id, &payload.from, &to_json, &payload.subject, &payload.body, &payload.html, now],
            );
        }

        Ok((
            StatusCode::OK,
            Json(QueueResponse {
                id,
                status: "sent".into(),
            }),
        ))
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_millis();
        let to_json = serde_json::to_string(&payload.to).unwrap();

        let db = state.db.lock().await;
        db.execute(
            "INSERT INTO email_queue (id, status, from_addr, to_addrs, subject, body, html, created_at)
             VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![&id, &payload.from, &to_json, &payload.subject, &payload.body, &payload.html, now],
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("db error: {e}"),
                }),
            )
        })?;

        Ok((
            StatusCode::ACCEPTED,
            Json(QueueResponse {
                id,
                status: "queued".into(),
            }),
        ))
    }
}

// ── Background Workers ──────────────────────────────────────────────────────

async fn queue_worker(state: Arc<AppState>) {
    let poll_interval = Duration::from_secs(state.config.queue_poll_seconds);

    loop {
        tokio::time::sleep(poll_interval).await;

        let emails: Vec<(String, String, String, String, String, Option<String>)> = {
            let db = state.db.lock().await;
            let mut stmt = match db.prepare(
                "SELECT id, from_addr, to_addrs, subject, body, html
                 FROM email_queue WHERE status = 'pending' ORDER BY created_at LIMIT 10",
            ) {
                Ok(s) => s,
                Err(e) => {
                    error!("queue worker prepare: {e}");
                    continue;
                }
            };

            let rows: Vec<(String, String, String, String, String, Option<String>)> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .ok()
                .map(|r| r.filter_map(|x| x.ok()).collect())
                .unwrap_or_default();

            for row in &rows {
                let _ = db.execute(
                    "UPDATE email_queue SET status = 'sending' WHERE id = ?1",
                    [&row.0],
                );
            }

            rows
        };

        for (id, from, to_json, subject, body, html) in &emails {
            let to_addrs: Vec<String> = serde_json::from_str(to_json).unwrap_or_default();

            match send_email(
                &state,
                from,
                &to_addrs,
                subject,
                body,
                html.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    info!("sent queued email {id}");
                    let db = state.db.lock().await;
                    let now = now_millis();
                    let _ = db.execute(
                        "INSERT INTO email_archive (id, queue_id, from_addr, to_addrs, subject, body, html, sent_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        rusqlite::params![now, id, from, to_json, subject, body, html, now],
                    );
                    let _ = db.execute("DELETE FROM email_queue WHERE id = ?1", [id]);
                }
                Err(e) => {
                    warn!("failed to send {id}: {e}");
                    let db = state.db.lock().await;
                    let _ = db.execute(
                        "UPDATE email_queue SET status = 'pending', attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
                        rusqlite::params![id, e],
                    );
                }
            }
        }
    }
}

async fn archive_culler(state: Arc<AppState>) {
    let interval = Duration::from_secs(state.config.archive_cull_interval_seconds);
    let max_rows = state.config.archive_max_rows;

    loop {
        tokio::time::sleep(interval).await;

        let db = state.db.lock().await;
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM email_archive", [], |r| r.get(0))
            .unwrap_or(0);

        if count > max_rows as i64 {
            let to_delete = count - max_rows as i64;
            match db.execute(
                "DELETE FROM email_archive WHERE id IN (SELECT id FROM email_archive ORDER BY id ASC LIMIT ?1)",
                [to_delete],
            ) {
                Ok(n) => info!("archive culler: deleted {n} rows"),
                Err(e) => error!("archive culler: {e}"),
            }
        }
    }
}

// ── Util ────────────────────────────────────────────────────────────────────

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mayl=info".parse().unwrap()),
        )
        .init();

    let config = Config::load();
    info!(
        smtp = %format!("{}:{}", config.smtp_host, config.smtp_port),
        server = %format!("{}:{}", config.server_host, config.server_port),
        "starting mayl"
    );

    let conn = Connection::open(&config.db_path).expect("failed to open database");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .expect("failed to set pragmas");
    init_db(&conn);
    seed_domains(&conn, &config.seed_domains);

    // Load SMTP credentials: env vars first, then DB overrides
    let mut smtp_user = env_or("MAYL_SMTP_USER", "");
    let mut smtp_pass = env_or("MAYL_SMTP_PASS", "");

    if let Ok(db_user) = conn.query_row(
        "SELECT value FROM config WHERE key = 'smtp_user'",
        [],
        |r| r.get::<_, String>(0),
    ) {
        smtp_user = db_user;
    }
    if let Ok(db_pass) = conn.query_row(
        "SELECT value FROM config WHERE key = 'smtp_pass'",
        [],
        |r| r.get::<_, String>(0),
    ) {
        smtp_pass = db_pass;
    }

    if !smtp_user.is_empty() {
        info!(user = %smtp_user, "SMTP credentials loaded");
    } else {
        info!("no SMTP credentials configured (use POST /smtp to set)");
    }

    let bind_addr = format!("{}:{}", config.server_host, config.server_port);

    let state = Arc::new(AppState {
        db: Mutex::new(conn),
        config,
        smtp_creds: RwLock::new(SmtpCredentials {
            user: smtp_user,
            pass: smtp_pass,
        }),
    });

    tokio::spawn(queue_worker(Arc::clone(&state)));
    tokio::spawn(archive_culler(Arc::clone(&state)));

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/health", get(health_handler))
        .route("/domains", post(create_domain_handler))
        .route("/domains", get(list_domains_handler))
        .route("/domains/{domain}", delete(delete_domain_handler))
        .route("/smtp", get(get_smtp_handler))
        .route("/smtp", post(set_smtp_handler))
        .route("/email", post(email_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .expect("failed to bind");

    info!("listening on {bind_addr}");
    axum::serve(listener, app).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_helpers() {
        assert_eq!(env_or("MAYL_TEST_NONEXISTENT_KEY", "fallback"), "fallback");
        assert_eq!(env_parse::<u16>("MAYL_TEST_NONEXISTENT_KEY", 42), 42);
    }

    #[test]
    fn test_init_db() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('email_queue', 'email_archive', 'domains', 'config')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn test_seed_domains() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);

        seed_domains(&conn, &["example.com".into(), "test.org".into()]);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM domains", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Seeding again should not duplicate
        seed_domains(&conn, &["example.com".into()]);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM domains", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_domain_token_lookup() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);

        let token = "test-token-abc";
        conn.execute(
            "INSERT INTO domains (domain, token, created_at) VALUES ('example.com', ?1, 0)",
            [token],
        )
        .unwrap();

        let domain: String = conn
            .query_row(
                "SELECT domain FROM domains WHERE token = ?1",
                [token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn test_extract_domain_from_addr() {
        assert_eq!(
            extract_domain_from_addr("user@example.com"),
            Some("example.com".into())
        );
        assert_eq!(
            extract_domain_from_addr("Name <user@example.com>"),
            Some("example.com".into())
        );
        assert_eq!(
            extract_domain_from_addr("User <USER@Example.COM>"),
            Some("example.com".into())
        );
        assert_eq!(extract_domain_from_addr("nope"), None);
    }

    #[test]
    fn test_extract_token() {
        let mut headers = HeaderMap::new();
        assert!(extract_token(&headers).is_none());

        headers.insert("authorization", "Bearer my-token".parse().unwrap());
        assert_eq!(extract_token(&headers).unwrap(), "my-token");

        headers.insert("authorization", "raw-token".parse().unwrap());
        assert_eq!(extract_token(&headers).unwrap(), "raw-token");
    }

    #[test]
    fn test_queue_insert_and_read() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);

        let id = "test-id-123";
        let now = now_millis();
        conn.execute(
            "INSERT INTO email_queue (id, status, from_addr, to_addrs, subject, body, created_at)
             VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, "a@b.com", "[\"c@d.com\"]", "hi", "hello", now],
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM email_queue WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn test_now_millis() {
        let ms = now_millis();
        assert!(ms > 1_700_000_000_000);
    }
}
