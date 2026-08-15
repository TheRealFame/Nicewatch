//! Tauri backend for the Nicewatch GUI.
//!
//! All communication with the daemon happens here on the Rust side over a
//! Unix-domain socket; the webview frontend only talks to us via Tauri
//! commands/events, never to the daemon directly.

mod apps;
mod settings;
mod theme;

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

use nicewatch_common::{
    APP_DISPLAY_NAME, APP_NAME, ClientMsg, GameAnswer, ServerMsg, Snapshot, Tier,
    EVT_DIFF, EVT_HELLO, EVT_PROMPT, EVT_SNAPSHOT, EVT_WARN,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
struct UiState {
    /// Most recent full snapshot from the daemon (for late subscribers).
    latest: Mutex<Option<Snapshot>>,
    /// Writer half of the daemon connection (None while disconnected).
    send: Mutex<Option<Sender<ClientMsg>>>,
}

/// "connected" frame shared by the heartbeat paths (kept in one place so the
/// webview store sees a consistent shape).
fn heartbeat_connected() -> serde_json::Value {
    serde_json::json!({ "connected": true, "app": APP_NAME, "display": APP_DISPLAY_NAME })
}

#[derive(Serialize)]
struct AppInfo {
    /// Filename spelling (from the single definition in the common crate).
    app: &'static str,
    /// Display name for window titles.
    display: &'static str,
    version: String,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        app: APP_NAME,
        display: APP_DISPLAY_NAME,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The OS's native colors + accent (port of the @nearcade packages).
#[tauri::command]
fn get_native_theme() -> theme::NativeTheme {
    theme::get_native_theme()
}

#[derive(Serialize)]
struct InstallOutcome {
    ok: bool,
    /// Before/after connection is not the command's business; the UI polls
    /// the connection pill itself.
    detail: String,
}

/// Locate the daemon binary, in priority order: the stable per-user install
/// (`~/.config/proc-priority-daemon/nicewatch.bin`, what the unit's
/// `ExecStart` points at), the external bin bundled inside this app's
/// AppImage (`<app>/bin/nicewatch`), then `nicewatch` on PATH (dev mode).
fn locate_daemon() -> Result<std::path::PathBuf, String> {
    let config_file = nicewatch_common::local_config_path();
    if let Some(config_dir) = config_file.parent() {
        let installed = config_dir.join(format!("{}.bin", nicewatch_common::APP_NAME));
        if installed.exists() {
            return Ok(installed);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let bundled = exe.parent().unwrap_or(&exe).join(nicewatch_common::APP_NAME);
        if bundled.exists() {
            return Ok(bundled);
        }
    }
    Ok(nicewatch_common::APP_NAME.into())
}

/// Install/start the background daemon as a per-user systemd service — the
/// same `nicewatch install` operation the CLI offers, surfaced in the app.
#[tauri::command]
fn install_service() -> Result<InstallOutcome, String> {
    let which = locate_daemon()?;
    let out = std::process::Command::new(&which)
        .arg("install")
        .output()
        .map_err(|e| format!("cannot run daemon install from {}: {e}", which.display()))?;
    let detail = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let detail = if detail.is_empty() { err } else { detail };
    Ok(InstallOutcome {
        ok: out.status.success(),
        detail,
    })
}

#[tauri::command]
fn get_state(state: State<'_, UiState>) -> Option<Snapshot> {
    state.latest.lock().ok()?.clone()
}

#[tauri::command]
fn set_tier(state: State<'_, UiState>, pid: u32, tier: Tier) -> Result<(), String> {
    let tx = state
        .send
        .lock()
        .map_err(|_| "state poisoned".to_string())?
        .clone()
        .ok_or_else(|| "daemon not connected".to_string())?;
    tx.send(ClientMsg::SetTier { pid, tier })
        .map_err(|e| format!("cannot reach daemon: {e}"))
}

#[tauri::command]
fn confirm_game(state: State<'_, UiState>, name: String, answer: GameAnswer) -> Result<(), String> {
    let tx = state
        .send
        .lock()
        .map_err(|_| "state poisoned".to_string())?
        .clone()
        .ok_or_else(|| "daemon not connected".to_string())?;
    tx.send(ClientMsg::ConfirmGame { name, answer })
        .map_err(|e| format!("cannot reach daemon: {e}"))
}

#[tauri::command]
fn set_cap(state: State<'_, UiState>, name: String, pct: Option<u32>) -> Result<(), String> {
    let tx = state
        .send
        .lock()
        .map_err(|_| "state poisoned".to_string())?
        .clone()
        .ok_or_else(|| "daemon not connected".to_string())?;
    tx.send(ClientMsg::SetCap { name, pct })
        .map_err(|e| format!("cannot reach daemon: {e}"))
}

#[tauri::command]
fn set_poll_interval(state: State<'_, UiState>, poll_interval_ms: u64) -> Result<(), String> {
    let tx = state
        .send
        .lock()
        .map_err(|_| "state poisoned".to_string())?
        .clone()
        .ok_or_else(|| "daemon not connected".to_string())?;
    tx.send(ClientMsg::SetPollInterval { poll_interval_ms })
        .map_err(|e| format!("cannot reach daemon: {e}"))
}

/// Resolve a process `exe` path to a display name + icon (cached).
#[tauri::command]
fn app_meta(exe: String) -> apps::AppMeta {
    apps::app_meta(&exe)
}

#[derive(Serialize)]
struct Outcome {
    ok: bool,
    detail: String,
}

fn run_systemctl(args: &[&str]) -> Outcome {
    let out = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output();
    match out {
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stdout)
                .trim()
                .to_string();
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Outcome {
                ok: o.status.success(),
                detail: if o.status.success() { detail } else { err },
            }
        }
        Err(e) => Outcome {
            ok: false,
            detail: format!("cannot run systemctl: {e}"),
        },
    }
}

/// One-click daemon on/off: `systemctl --user start|stop nicewatch`.
#[tauri::command]
fn set_daemon_running(running: bool) -> Outcome {
    let name = "nicewatch";
    if running {
        run_systemctl(&["start", name])
    } else {
        run_systemctl(&["stop", name])
    }
}

/// Start/stop the daemon from the tray without opening the window.
#[tauri::command]
fn daemon_start() -> Outcome {
    run_systemctl(&["start", "nicewatch"])
}

#[tauri::command]
fn daemon_stop() -> Outcome {
    run_systemctl(&["stop", "nicewatch"])
}

/// One-click fix for the "CANNOT WRITE ROOT CONFIG" warning: make
/// `/etc/proc-priority-daemon` owned by the current user so the daemon's
/// promote-to-root path works.  Uses `pkexec` (polkit) — the DE's auth
/// dialog pops up once.
#[tauri::command]
fn fix_root_config() -> Outcome {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok());
    let gid = std::process::Command::new("id")
        .arg("-g")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (uid, gid) else {
        return Outcome {
            ok: false,
            detail: "cannot determine uid/gid (is `id` available?)".into(),
        };
    };
    let local = nicewatch_common::local_config_path();
    let script = format!(
        "install -d -o {uid} -g {gid} /etc/proc-priority-daemon && cp '{}' /etc/proc-priority-daemon/rules.toml && chown {uid}:{gid} /etc/proc-priority-daemon/rules.toml",
        local.display()
    );
    let out = std::process::Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .output();
    match out {
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stdout)
                .trim()
                .to_string();
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let ok = o.status.success();
            Outcome {
                ok,
                detail: if ok {
                    if detail.is_empty() {
                        "/etc/proc-priority-daemon is now writable".into()
                    } else {
                        detail
                    }
                } else if err.is_empty() {
                    "pkexec was cancelled or unavailable".into()
                } else {
                    err
                },
            }
        }
        Err(e) => Outcome {
            ok: false,
            detail: format!("cannot run pkexec (is polkit installed?): {e}"),
        },
    }
}

#[tauri::command]
fn get_gui_settings() -> settings::GuiSettings {
    settings::load()
}

#[tauri::command]
fn set_gui_settings(s: settings::GuiSettings) -> Result<(), String> {
    settings::save(&s)
}

/// Reconnect loop: attach to the daemon socket, stream snapshots/diffs into
/// Tauri events, and relay command invocations back out.  Starts whether or
/// not the daemon is up yet (1s retry backoff).
fn connect_loop(app: AppHandle) {
    loop {
        let path = nicewatch_common::ipc_socket_path();
        let Ok(stream) = UnixStream::connect(&path) else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        // Blocking read: the daemon broadcasts a snapshot/diff every poll
        // (default 2 s), so a read timeout here would false-disconnect us in
        // the silence between broadcasts.  A dead daemon closes the socket,
        // which wakes the read with EOF.
        log::info!("connected to daemon at {}", path.display());
        let _ = app.emit(
            EVT_HELLO,
            serde_json::json!({ "connected": true, "app": APP_NAME, "display": APP_DISPLAY_NAME }),
        );

        // Writer channel: frontend commands flow through this.
        let (tx, rx) = mpsc::channel::<ClientMsg>();
        if let Ok(mut st) = app.state::<UiState>().send.lock() {
            *st = Some(tx);
        }
        if let Ok(writer_stream) = stream.try_clone() {
            std::thread::spawn(move || {
                let mut w = BufWriter::new(writer_stream);
                while let Ok(msg) = rx.recv() {
                    let bytes = nicewatch_common::encode_msg(&msg);
                    if w.write_all(&bytes).and_then(|_| w.flush()).is_err() {
                        break;
                    }
                }
            });
        }
        // Introduce ourselves; the daemon replies with a full snapshot.
        let _ = stream.try_clone().map(|mut s| {
            let _ = s.write_all(&nicewatch_common::encode_msg(&ClientMsg::Hello {
                client_kind: "gui".into(),
            }));
            let _ = s.flush();
        });

        // Reader loop: daemon -> Tauri events.
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF or keep-alive timeout
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match nicewatch_common::decode_msg::<ServerMsg>(trimmed) {
                Ok(ServerMsg::Snapshot(s)) => {
                    if let Ok(mut latest) = app.state::<UiState>().latest.lock() {
                        *latest = Some(s.clone());
                    }
                    let _ = app.emit(EVT_SNAPSHOT, &s);
                    // Heartbeat for the UI's connected flag: the initial
                    // "connected: true" can race the webview's listener
                    // registration, and once lost nothing ever turned the pill
                    // green again.  Emitting on every frame self-heals within
                    // one poll cycle.
                    let _ = app.emit(EVT_HELLO, heartbeat_connected());
                }
                Ok(ServerMsg::Diff(d)) => {
                    let _ = app.emit(EVT_DIFF, &d);
                    let _ = app.emit(EVT_HELLO, heartbeat_connected());
                }
                Ok(ServerMsg::PromptGame(p)) => {
                    let _ = app.emit(EVT_PROMPT, &p);
                }
                Ok(ServerMsg::Warn { msg }) => {
                    let _ = app.emit(EVT_WARN, serde_json::json!({ "msg": msg }));
                }
                Ok(ServerMsg::Hello { poll_interval_ms, .. }) => {
                    let _ = app.emit(
                        EVT_HELLO,
                        serde_json::json!({
                            "connected": true,
                            "poll_interval_ms": poll_interval_ms,
                        }),
                    );
                }
                Err(e) => log::debug!("bad frame from daemon: {e}"),
            }
        }

        // Disconnected.
        app.emit(EVT_HELLO, serde_json::json!({ "connected": false })).ok();
        if let Ok(mut st) = app.state::<UiState>().send.lock() {
            *st = None;
        }
        if let Ok(mut latest) = app.state::<UiState>().latest.lock() {
            *latest = None;
        }
        log::warn!("daemon connection lost — retrying every second");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Single-instance guard: bind a per-user Unix socket; a second launch
/// fails to bind, sees the first instance is alive (connect succeeds), asks
/// it to focus its window, and exits.  A stale socket from a crashed first
/// instance is detected via the failed connect and cleared.
fn single_instance_guard(app: &tauri::App) -> bool {
    use std::os::unix::net::{UnixListener, UnixStream};

    let path = nicewatch_common::runtime_dir().join(format!("{}.gui.sock", APP_NAME));
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(_) => {
            if UnixStream::connect(&path).is_ok() {
                return false; // first instance is alive — exit, it takes the stage
            }
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) => {
                    log::error!(
                        "cannot bind single-instance socket {}: {e}",
                        path.display()
                    );
                    return true; // still run rather than lock the user out
                }
            }
        }
    };

    // Wake on each second-instance ping and pull its window to the front.
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            drop(conn);
            if let Some(win) = handle.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    });
    true
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    tauri::Builder::default()
        .manage(UiState::default())
        .invoke_handler(tauri::generate_handler![
            app_info,
            get_native_theme,
            get_state,
            set_tier,
            set_cap,
            set_poll_interval,
            confirm_game,
            install_service,
            app_meta,
            set_daemon_running,
            daemon_start,
            daemon_stop,
            fix_root_config,
            get_gui_settings,
            set_gui_settings
        ])
        .setup(|app| {
            // Second instance?  Hand the stage to the first and quit before
            // any window or tray icon exists (no flash, no tray fights).
            if !single_instance_guard(app) {
                app.handle().exit(0);
                return Ok(());
            }
            let handle = app.handle().clone();
            std::thread::spawn(move || connect_loop(handle));

            let tray_settings = settings::load();
            let start_hidden = tray_settings.start_in_tray;

            let _win = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
                .title(APP_DISPLAY_NAME)
                .inner_size(1180.0, 760.0)
                .min_inner_size(800.0, 480.0)
                // No native GTK header bar: the in-app header is the title
                // bar (thin, drag region) — avoids the double app name
                // ("nicewatch" GTK title + in-app header) and the thick
                // GNOME decoration.
                .decorations(false)
                .visible(!start_hidden)
                .build()?;

            setup_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Close-to-tray: hide instead of quitting when the setting is on.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if settings::load().minimize_to_tray {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// System tray: show/hide the window, start/stop the daemon, quit.
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItemBuilder::with_id("show", "Show Nicewatch").build(app)?;
    let start = MenuItemBuilder::with_id("daemon-start", "Start Daemon").build(app)?;
    let stop = MenuItemBuilder::with_id("daemon-stop", "Stop Daemon").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&show)
        .separator()
        .item(&start)
        .item(&stop)
        .separator()
        .item(&quit)
        .build()?;

    let icon_bytes = include_bytes!("../icons/icon-32.png");
    let icon = tauri::image::Image::from_bytes(icon_bytes)?;
    TrayIconBuilder::new()
        .icon(icon)
        .tooltip(APP_DISPLAY_NAME)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            "daemon-start" => {
                let _ = daemon_start();
            }
            "daemon-stop" => {
                let _ = daemon_stop();
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(win) = tray.app_handle().get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}