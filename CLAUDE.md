# CLAUDE.md -- Context for Claude Code

## Project Overview

mayl is an email-sending HTTP API backed by protonmail-bridge, running in a single Docker container. The container runs protonmail-bridge in gRPC mode and the mayl Rust API server (HTTP on port 8080). mayl connects to the bridge via gRPC over Unix socket to handle login and auto-harvest SMTP credentials. All processes are supervised by runit (PID 1).

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
- **gRPC client:** tonic 0.12 (TLS, transport) + prost 0.13
- **HTTP middleware:** tower-http (cors, trace)
- **IDs:** uuid v4
- **Process supervision:** runit
- **Container:** Docker multi-stage build (rust builder on trixie + debian trixie-slim runtime)

## Build and Run

```bash
cargo build           # compile
cargo test            # run tests (10 tests, all in-memory)
cargo run             # run locally (needs SMTP server)

docker compose build  # build container image
docker compose up -d  # start container
```

Rust edition 2024 requires a recent stable toolchain.

## Project Structure

```
.
├── Cargo.toml           # Dependencies (no toml crate -- env vars only)
├── build.rs             # tonic-build proto compilation
├── proto/
│   └── bridge.proto     # Proton Bridge gRPC service definition
├── Dockerfile           # Multi-stage: trixie runtime + rust builder + final
├── docker-compose.yml   # Single service, 5 volumes
├── entrypoint.sh        # One-time init (GPG, pass, dbus) then exec runsvdir
├── sv/                  # runit service directories
│   ├── bridge/run       # protonmail-bridge --grpc (with lock cleanup)
│   └── mayl/run         # mayl API server
├── src/
│   └── main.rs          # Entire application
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
| `MAYL_BRIDGE_CONFIG_DIR` | (empty) | Bridge gRPC config dir (enables bridge integration) |

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
| `POST` | `/bridge/unlock` | Headers + JSON | Login to Proton bridge |
| `GET` | `/bridge/status` | No | Bridge connection + user status |

`POST /email` requires `Authorization: Bearer <token>` matching the `from` address domain.

## Process Supervision

runit (`runsvdir`) runs as PID 1. The entrypoint does one-time setup (GPG key generation, pass init, D-Bus, gnome-keyring) then `exec runsvdir /etc/service`. Each service in `sv/` has a `run` script; runit auto-restarts any that exit. The bridge service cleans stale lock files before each start. Proton account login is done via `POST /bridge/unlock`.

## Key Design Decisions

- **Single-file app:** All logic in `src/main.rs`.
- **Env vars only:** No config files, no toml crate.
- **Single container:** Bridge and API in one container, SMTP via localhost.
- **runit for PID 1:** Proper signal handling, auto-restart, clean stop/start cycles.
- **`dangerous_accept_invalid_certs(true)`:** Bridge uses self-signed TLS. Must use `TlsParameters::builder().dangerous_accept_invalid_certs(true)` then `AsyncSmtpTransport::builder_dangerous()` with `Tls::Required(tls_params)`.
- **Domain token auth:** `POST /domains` creates a domain + UUID token. `POST /email` validates the Bearer token matches the `from` domain.
- **Background workers:** `queue_worker`, `archive_culler`, and `bridge_event_worker` run as `tokio::spawn` tasks.
- **Bridge gRPC integration:** Bridge runs with `--grpc`. mayl connects via Unix socket + TLS, reads `grpcServerConfig.json` for socket path/cert/token. Login via `POST /bridge/unlock`, SMTP credentials auto-harvested from bridge after login.
- **Bridge event stream:** Background task listens to `RunEventStream()` for login events, user changes. Must stay alive — bridge quits if client disconnects.

## Testing

```bash
cargo test
```

10 tests (all in `src/main.rs` `#[cfg(test)] mod tests`):
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
- Bridge runs with `--grpc`. Login via `POST /bridge/unlock`. SMTP creds auto-harvested.
- Bridge lock files must be cleaned on service start to survive container stop/start cycles.
- Base image is `debian:trixie-slim` (not bookworm) because OpenGL/Qt libs needed by bridge are only in trixie.
