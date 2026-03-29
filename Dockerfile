# ── Stage 1: Runtime base (bridge + deps) ────────────────────────────────────

FROM debian:trixie-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    gnupg \
    pass \
    dbus \
    dbus-x11 \
    gnome-keyring \
    gir1.2-secret-1 \
    libegl1 \
    libgl1 \
    libgl1-mesa-dri \
    libopengl0 \
    libglx0 \
    libxcb-xinerama0 \
    libxcb-icccm4 \
    libxcb-image0 \
    libxcb-keysyms1 \
    libxcb-randr0 \
    libxcb-render-util0 \
    libxcb-shape0 \
    libxcb-xkb1 \
    libxcb-cursor0 \
    libxkbcommon-x11-0 \
    libdbus-1-3 \
    fontconfig \
    libfontconfig1 \
    runit \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sL https://proton.me/download/bridge/protonmail-bridge_3.16.0-1_amd64.deb -o /tmp/bridge.deb \
    && apt-get update \
    && apt-get install -y /tmp/bridge.deb \
    && rm /tmp/bridge.deb \
    && rm -rf /var/lib/apt/lists/*

# ── Stage 2: Build mayl ──────────────────────────────────────────────────────

FROM rust:1-slim-trixie AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
COPY static ./static

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && cp target/release/mayl /usr/local/bin/mayl

# ── Stage 3: Final image ─────────────────────────────────────────────────────

FROM runtime

COPY --from=builder /usr/local/bin/mayl /usr/local/bin/mayl
COPY entrypoint.sh /entrypoint.sh
COPY sv/ /etc/sv/
RUN chmod +x /entrypoint.sh \
    && chmod +x /etc/sv/*/run \
    && ln -s /etc/sv/bridge /etc/service/bridge \
    && ln -s /etc/sv/mayl /etc/service/mayl

EXPOSE 8080

VOLUME ["/root/.config/protonmail", "/root/.local/share/protonmail", "/root/.gnupg", "/root/.password-store", "/data"]

ENTRYPOINT ["/entrypoint.sh"]
