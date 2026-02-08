#!/bin/bash
set -e

# ── GPG + pass (needed by protonmail-bridge for credential storage) ──────────

if [ ! -f /root/.gnupg/trustdb.gpg ]; then
    echo "Generating GPG key for pass..."
    gpg --batch --gen-key <<EOF2
%no-protection
Key-Type: RSA
Key-Length: 2048
Subkey-Type: RSA
Subkey-Length: 2048
Name-Real: Bridge
Name-Email: bridge@local
Expire-Date: 0
%commit
EOF2
fi

GPG_KEY_ID=$(gpg --list-keys --with-colons 2>/dev/null | grep '^pub' | head -1 | cut -d: -f5)

if [ ! -d /root/.password-store ]; then
    pass init "$GPG_KEY_ID"
fi

# ── D-Bus (needed by gnome-keyring / secret service) ─────────────────────────

mkdir -p /run/dbus
if [ ! -f /run/dbus/pid ] || ! kill -0 "$(cat /run/dbus/pid)" 2>/dev/null; then
    rm -f /run/dbus/pid
    dbus-daemon --system --fork
fi

eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS

# Persist dbus session address for runit services
echo "export DBUS_SESSION_BUS_ADDRESS='$DBUS_SESSION_BUS_ADDRESS'" > /run/dbus-session-env

# ── Hand off to runit ────────────────────────────────────────────────────────

echo "Starting services via runit..."
exec runsvdir /etc/service
