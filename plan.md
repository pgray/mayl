# Plan: Remove VNC/GUI, Run Bridge Headless

## Approach
Strip all VNC/GUI services and run `protonmail-bridge --noninteractive` with the existing DEB package. Keep minimal X11/Qt libs since the DEB binary links against them (it just won't open a window with `--noninteractive`).

## Changes

### 1. Dockerfile (`Dockerfile`)
- **Remove packages**: `xvfb`, `x11vnc`, `fluxbox`, `stalonetray`, `novnc`, `websockify`, `python3-gi`
- **Keep packages**: `dbus`, `dbus-x11`, `gnome-keyring`, `gir1.2-secret-1`, `gnupg`, `pass`, `ca-certificates`, `curl`, `runit`, and all the `lib*` X11/Qt/OpenGL packages (the DEB binary dynamically links them — removing them would crash the binary even in noninteractive mode)
- **Remove**: `COPY novnc.html /novnc.html`
- **Remove service symlinks**: `xvfb`, `fluxbox`, `stalonetray`, `x11vnc`, `websockify`
- **Remove**: `ENV DISPLAY=:99`
- **Change EXPOSE**: `6080 8080` → `8080`

### 2. Bridge service (`sv/bridge/run`)
- Remove the Xvfb wait loop (`while [ ! -e /tmp/.X99-lock ]`)
- Add `--noninteractive` flag to `protonmail-bridge`
- Keep dbus sourcing and lock cleanup

### 3. Entrypoint (`entrypoint.sh`)
- No changes needed — GPG, pass, and dbus setup are all still required

### 4. Docker Compose (`docker-compose.yml`)
- Remove port mapping `6080:6080`

### 5. Delete files
- `novnc.html`
- `sv/xvfb/run`
- `sv/fluxbox/run`
- `sv/stalonetray/run`
- `sv/x11vnc/run`
- `sv/websockify/run`

### 6. Update CLAUDE.md and README.md
- Remove references to noVNC, VNC, port 6080, xvfb, fluxbox, stalonetray
- Document new login flow: `docker exec -it mayl protonmail-bridge --cli` for initial account setup

## Initial Account Login (post-change)
Users will run:
```bash
docker exec -it mayl protonmail-bridge --cli
```
Then use the `login` command to authenticate. This is a one-time step; credentials persist in the `bridge-pass` and `bridge-config` volumes.

## Risk
The DEB binary links against Qt/X11 libs. Running with `--noninteractive` *should* skip GUI init, but if it doesn't, we'd need to either:
- Add back a minimal Xvfb (fallback)
- Switch to building from source with `make build-nogui`
