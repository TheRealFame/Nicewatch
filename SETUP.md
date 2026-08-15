# Nicewatch setup guide

Nicewatch is a Rust priority-management daemon with a Tauri v2 / Svelte 5
desktop GUI.  This page covers manual install on a systemd Linux desktop.

## 1. Build

Prerequisites: Rust (>= 1.85), Node (>= 20), WebKitGTK 4.1 dev packages,
GTK 3 dev packages (Debian/Ubuntu: `libwebkit2gtk-4.1-dev libgtk-3-dev`).

```
cargo build --release
npm install --prefix gui
```

Binaries land in `target/release/`:

- `target/release/nicewatch` — the daemon (system service, runs as root)
- `target/release/nicewatch-gui` — the desktop GUI (run as the desktop user)

## 2. Install the daemon

```
sudo install -m755 target/release/nicewatch /usr/local/bin/nicewatch
sudo mkdir -p /etc/proc-priority-daemon
sudo install -m644 rules.toml.example /etc/proc-priority-daemon/rules.toml
```

Make sure the directory is owned by root but world-readable; the daemon
promotes GUI-set rules here, so it also needs write access (it runs as root).
Local (non-root) rules go to `~/.config/proc-priority-daemon/rules.toml`.

## 3. systemd service

```
sudo cp setup/nicewatch.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nicewatch
journalctl -u nicewatch -f      # watch it scanning /proc
```

To test without the service (as a normal user, non-root):

```
./target/release/nicewatch --socket /tmp/nicewatch-test.sock
```

Without the /etc config it falls back to the local config; if the system
config cannot be written (EPERM) it logs `continuing with local ...` and keeps
running.

## 4. Run the GUI

The GUI is intentionally a per-user application, not a service.  Run it from
your desktop session:

```
./target/release/nicewatch-gui
```

It connects to the daemon over the Unix socket
`$XDG_RUNTIME_DIR/nicewatch.sock` (the systemd unit uses `%t` for the same
value), with a 1-second reconnect loop if the daemon is not running yet.

## 5. Config

See `rules.toml.example` and README.  Summary:

| path | role |
|---|---|
| `/etc/proc-priority-daemon/rules.toml` | authoritative system config |
| `~/.config/proc-priority-daemon/rules.toml` | local changes; promoted on settle (30 s) |

## 6. Uninstall

```
sudo systemctl disable --now nicewatch
sudo rm /etc/systemd/system/nicewatch.service /usr/local/bin/nicewatch
rm -rf ~/.config/proc-priority-daemon
```