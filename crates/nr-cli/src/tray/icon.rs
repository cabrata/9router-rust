//! System tray icon. Port of `cli/src/cli/tray/tray.js` (systray2 path) and
//! `trayWin.js`.
//!
//! Everything here is behind the `tray` cargo feature, so the default build
//! pulls no GTK: `cargo build -p nr-cli --features tray`. On Linux the icon
//! must be created on the thread that runs the GTK main loop, so [`start`]
//! owns that thread and hands back a [`TrayControl`] that only relays events.
//!
//! Menu: `9Router (Port N)` (status, disabled) · `Open Dashboard` ·
//! `Enable Auto-start` / `✓ Auto-start Enabled` · `Quit` — the same four items
//! and the same indexes as the JS `buildMenuItems`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use super::autostart::{
    autostart_label, is_auto_start_enabled, menu_items, tray_action, TrayAction,
    MENU_INDEX_AUTOSTART, MENU_INDEX_DASHBOARD, MENU_INDEX_QUIT, MENU_INDEX_STATUS,
};

/// Handshake timeout: the JS bounds tray readiness at 3s (`TRAY_READY_TIMEOUT_MS`).
pub const TRAY_READY_TIMEOUT_MS: u64 = 3_000;

/// Bundled 32x32 logo. `cli/src/cli/tray/icon.png` from the JS tree.
const ICON_PNG: &[u8] = include_bytes!("../../assets/tray-icon.png");

/// What the tray reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// Icon built and visible.
    Ready,
    /// The user picked "Quit".
    Quit,
}

/// Caller-side handle. Dropping it does not tear the icon down; the tray thread
/// lives for the process lifetime, like the JS `trayInstance`.
pub struct TrayControl {
    events: mpsc::Receiver<TrayEvent>,
    quit: Arc<AtomicBool>,
}

impl TrayControl {
    /// Next event, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<TrayEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(TrayEvent::Quit),
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }
}

/// Write the bundled PNG to a temp file so `Icon::from_path` can decode it.
///
/// `tray_icon::Icon` exposes `from_rgba` and `from_path`; path decoding hands
/// the work to the crate's own PNG reader instead of a hand-rolled inflate.
fn icon_file() -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join("9router-tray");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("icon-{}.png", std::process::id()));
    std::fs::write(&path, ICON_PNG).map_err(|e| format!("cannot write icon: {e}"))?;
    Ok(path)
}

fn load_icon() -> Result<Icon, String> {
    let path = icon_file()?;
    Icon::from_path(&path, Some((32, 32))).map_err(|e| format!("bad icon: {e}"))
}

/// Start the tray on a dedicated thread. `Err` means "no icon, keep serving" —
/// exactly what the JS `onError` path reports.
pub fn start(port: u16) -> Result<TrayControl, String> {
    if !super::autostart::is_tray_supported() {
        return Err("no graphical session (DISPLAY/WAYLAND_DISPLAY unset)".into());
    }

    let (event_tx, event_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let quit = Arc::new(AtomicBool::new(false));
    let quit_for_thread = quit.clone();

    std::thread::Builder::new()
        .name("9router-tray".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = gtk::init() {
                    let _ = ready_tx.send(Err(format!("gtk init failed: {e}")));
                    return;
                }
            }

            let (icon, autostart_item) = match build_tray(port, event_tx.clone(), quit_for_thread.clone()) {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            let _ = ready_tx.send(Ok(()));
            let _ = event_tx.send(TrayEvent::Ready);

            // Keep the icon (and its menu) alive for as long as the tray runs.
            #[cfg(target_os = "linux")]
            {
                let _keep = (&icon, &autostart_item);
                gtk::main();
            }
            #[cfg(not(target_os = "linux"))]
            {
                // Windows / macOS pump their own message loop inside tray-icon;
                // park this thread until the user quits.
                let _keep = (&icon, &autostart_item);
                while !quit_for_thread.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        })
        .map_err(|e| format!("cannot spawn tray thread: {e}"))?;

    match ready_rx.recv_timeout(Duration::from_millis(TRAY_READY_TIMEOUT_MS)) {
        Ok(Ok(())) => Ok(TrayControl { events: event_rx, quit }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("tray did not become ready within {TRAY_READY_TIMEOUT_MS}ms")),
    }
}

/// Build the menu, the icon and the click handler. Must run on the GTK thread.
fn build_tray(
    port: u16,
    event_tx: mpsc::Sender<TrayEvent>,
    quit: Arc<AtomicBool>,
) -> Result<(tray_icon::TrayIcon, MenuItem), String> {
    let icon = match load_icon() {
        Ok(icon) => icon,
        // A missing icon must not disable the tray; fall back to a 1x1
        // transparent pixel (the JS has a base64 fallback too).
        Err(_) => Icon::from_rgba(vec![0u8; 4], 1, 1).map_err(|e| e.to_string())?,
    };

    let labels = menu_items(port, is_auto_start_enabled());
    let menu = Menu::new();
    let status_item = MenuItem::new(&labels[MENU_INDEX_STATUS].0, false, None);
    let dashboard_item = MenuItem::new(&labels[MENU_INDEX_DASHBOARD].0, true, None);
    let autostart_item = MenuItem::new(&labels[MENU_INDEX_AUTOSTART].0, true, None);
    let quit_item = MenuItem::new(&labels[MENU_INDEX_QUIT].0, true, None);
    menu.append_items(&[&status_item, &dashboard_item, &autostart_item, &quit_item])
        .map_err(|e| format!("menu: {e}"))?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip(format!("9Router - Port {port}"))
        .build()
        .map_err(|e| format!("tray icon: {e}"))?;

    // Menu clicks are delivered on the platform's UI thread; forward the two
    // that matter and update the auto-start label in place.
    let dashboard_id = dashboard_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    let quit_id = quit_item.id().clone();
    let autostart_for_click = autostart_item.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let index = if *event.id() == dashboard_id {
            MENU_INDEX_DASHBOARD
        } else if *event.id() == autostart_id {
            MENU_INDEX_AUTOSTART
        } else if *event.id() == quit_id {
            MENU_INDEX_QUIT
        } else {
            return;
        };
        match tray_action(index) {
            TrayAction::OpenDashboard => super::open_dashboard(port),
            TrayAction::ToggleAutostart => {
                let enabled = is_auto_start_enabled();
                let ok = if enabled {
                    super::autostart::disable_auto_start()
                } else {
                    super::autostart::enable_auto_start(None)
                };
                if ok {
                    autostart_for_click.set_text(autostart_label(!enabled));
                }
            }
            TrayAction::Quit => {
                quit.store(true, Ordering::SeqCst);
                let _ = event_tx.send(TrayEvent::Quit);
                #[cfg(target_os = "linux")]
                gtk::main_quit();
            }
            TrayAction::None => {}
        }
    }));

    Ok((tray, autostart_item))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_icon_is_a_png_of_the_expected_size() {
        assert_eq!(&ICON_PNG[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width/height live at bytes 16..24.
        let w = u32::from_be_bytes([ICON_PNG[16], ICON_PNG[17], ICON_PNG[18], ICON_PNG[19]]);
        let h = u32::from_be_bytes([ICON_PNG[20], ICON_PNG[21], ICON_PNG[22], ICON_PNG[23]]);
        assert_eq!((w, h), (32, 32));
    }

    #[test]
    fn icon_file_writes_a_decodable_icon() {
        let path = icon_file().expect("temp icon written");
        assert_eq!(std::fs::read(&path).unwrap(), ICON_PNG);
        assert!(Icon::from_path(&path, Some((32, 32))).is_ok(), "tray-icon can decode it");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tray_start_refuses_without_a_graphical_session() {
        // Headless CI: DISPLAY/WAYLAND_DISPLAY unset must be a clean Err, never a panic.
        let had_display = std::env::var_os("DISPLAY");
        let had_wayland = std::env::var_os("WAYLAND_DISPLAY");
        if had_display.is_none() && had_wayland.is_none() {
            match start(20128) {
                Err(e) => assert!(e.contains("graphical session"), "{e}"),
                Ok(_) => panic!("tray must not start headless"),
            }
        }
    }
}
