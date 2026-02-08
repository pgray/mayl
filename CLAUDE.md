# CLAUDE.md -- Context for Claude Code

## Project Overview

mayl is an email-sending HTTP API backed by protonmail-bridge, running in a single Docker container. The container runs both protonmail-bridge (SMTP on localhost:1025) and the mayl Rust API server (HTTP on port 8080). noVNC on port 6080 provides browser access to the bridge GUI for Proton account login.

Emails can be sent synchronously or queued for background delivery. All sent emails are optionally archived in SQLite with automatic old-row culling. Domain-based token authentication controls who can send.

## Tech Stack

- **Language:** Rust (edition 2024)
- **Web framework:** axum 0.8
- **HTML templating:** maud 0.27
- **SMTP client:** lettre 0.11 (tokio1-native-tls, smtp-transport)
- **Database:** rusqlite 0.32 (bundled SQLite)
- **Async runtime:** tokio 1 (full)
- **Serialization:** serde + serde_json
- **Logging:** tracing + tracing-subscriber (env-filter)
- **HTTP middleware:** tower-http (cors, trace)
- **IDs:** uuid v4
- **Container:** Docker with multi-stage build (rust builder + debian bookworm-slim runtime)

## Build and Run

```bash
cargo build           # compile
cargo test            # run tests (8 tests, all in-memory)
cargo run             # run locally (needs SMTP server)

docker compose build  # build container image
docker compose up -d  # start container
```

Rust edition 2024 requires a recent stable toolchain.

## Project Structure

```
.
├── Cargo.toml           # Dependencies (no toml crate -- env vars only)
├── Dockerfile           # Multi-stage: rust builder + debian with bridge + mayl
├── docker-compose.yml   # Single service, 5 volumes
├── entrypoint.sh        # Starts bridge + VNC + mayl in one container
├── src/
│   └── main.rs          # Entire application (~970 lines)
├── .github/
│   └── workflows/
│       └── ci.yml       # Rust CI + Docker build + GHCR push
├── .dockerignore
├── .gitignore
└── README.md
```

All application logic lives in `src/main.rs`.

## Configuration

**All config is via environment variables.** No config files.

| Variable | Default | Description |
|----------|---------|-------------|
| `MAYL_SMTP_HOST` | `localhost` | SMTP host |
| `MAYL_SMTP_PORT` | `1025` | SMTP port |
| `MAYL_SMTP_USER` | (empty) | Bridge SMTP username |
| `MAYL_SMTP_PASS` | (empty) | Bridge SMTP password |
| `MAYL_SERVER_HOST` | `0.0.0.0` | HTTP bind address |
| `MAYL_SERVER_PORT` | `8080` | HTTP bind port |
| `MAYL_QUEUE_POLL_SECONDS` | `5` | Queue poll interval |
| `MAYL_ARCHIVE_MAX_ROWS` | `100000` | Max archive rows |
| `MAYL_ARCHIVE_CULL_INTERVAL_SECONDS` | `600` | Archive cull interval |
| `MAYL_DB_PATH` | `mayl.db` | SQLite path |
| `MAYL_DOMAINS` | (empty) | Comma-separated seed domains |

## Database

SQLite with WAL mode and 5000ms busy timeout. Three tables:

- `email_queue` -- pending/sending emails
- `email_archive` -- sent emails (PK = unix millis)
- `domains` -- registered domains with tokens

Access serialized via `tokio::sync::Mutex<Connection>`.

## API Routes

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/` | No | maud HTML dashboard |
| `GET` | `/health` | No | JSON stats |
| `POST` | `/domains` | No | Register domain, get token |
| `GET` | `/domains` | No | List domains |
| `DELETE` | `/domains/{domain}` | No | Remove domain |
| `POST` | `/email` | Bearer token | Send/queue email |

`POST /email` requires `Authorization: Bearer <token>` matching the `from` address domain.

## Key Design Decisions

- **Single-file app:** All logic in `src/main.rs`.
- **Env vars only:** No config files, no toml crate.
- **Single container:** Bridge and API in one container, SMTP via localhost.
- **`dangerous_accept_invalid_certs(true)`:** Bridge uses self-signed TLS. Must use `TlsParameters::builder().dangerous_accept_invalid_certs(true)` then `AsyncSmtpTransport::builder_dangerous()` with `Tls::Required(tls_params)`.
- **Domain token auth:** `POST /domains` creates a domain + UUID token. `POST /email` validates the Bearer token matches the `from` domain.
- **Background workers:** `queue_worker` and `archive_culler` run as `tokio::spawn` tasks.

## Testing

```bash
cargo test
```

8 tests (all in `src/main.rs` `#[cfg(test)] mod tests`):
- `test_env_helpers` -- env_or/env_parse defaults
- `test_init_db` -- verifies 3 tables created
- `test_seed_domains` -- idempotent domain seeding
- `test_domain_token_lookup` -- token-to-domain query
- `test_extract_domain_from_addr` -- parses `user@domain` and `Name <user@domain>`
- `test_extract_token` -- Bearer header parsing
- `test_queue_insert_and_read` -- queue insert/read
- `test_now_millis` -- timestamp sanity

All tests use in-memory SQLite. No SMTP or Docker required.

## Important Notes

- Rust 2024 edition: `std::env::remove_var` is unsafe. Tests avoid it.
- maud 0.27 required for axum 0.8 compatibility (0.26 uses axum-core 0.4, needs 0.5).
- Bridge Dockerfile: use `apt-get install -y /tmp/bridge.deb` (NOT `dpkg -i || apt-get -yf` which removes the package).
