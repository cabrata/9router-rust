//! System tray. Port of `cli/src/cli/tray/**`.
//!
//! - [`autostart`] — launch-on-boot entries for Linux/Windows/macOS. Always
//!   compiled: it only writes config files, no GUI.
//! - [`icon`] — the actual tray icon, gated behind the `tray` cargo feature so a
//!   default build pulls in no GTK.
//!
//! The tray menu is exactly `tray.js`'s: a disabled status row, "Open
//! Dashboard", an auto-start toggle, and "Quit".

use std::time::Duration;

pub mod autostart;

#[cfg(feature = "tray")]
pub mod icon;

#[cfg(feature = "tray")]
pub use icon::{start, TrayControl, TrayEvent};

pub use autostart::{
    disable_auto_start, enable_auto_start, is_auto_start_enabled, is_tray_supported, menu_items,
    tray_action, TrayAction,
};

/// Open the dashboard in the user's browser (`openBrowser` in tray.js).
pub fn open_dashboard(port: u16) {
    let _ = open::that(autostart::dashboard_url(port));
}

/// Run the tray until the user quits it (`initTray` + `handleClick`'s Quit).
///
/// `on_ready` fires only once the icon really is registered: the JS is explicit
/// that callers must not announce a tray before that handshake.
///
/// Without the `tray` feature this reports the build hint and returns at once,
/// so `9router --tray` still keeps the server running (it just has no icon).
pub fn run_tray<F: FnOnce()>(port: u16, on_ready: F, on_error: impl FnOnce(&str)) {
    #[cfg(feature = "tray")]
    {
        match icon::start(port) {
            Ok(control) => {
                on_ready();
                loop {
                    match control.recv_timeout(Duration::from_millis(500)) {
                        Some(icon::TrayEvent::Quit) => {
                            println!("\n👋 Shutting down...");
                            return;
                        }
                        Some(icon::TrayEvent::Ready) => {}
                        None => {
                            if control.quit_requested() {
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => on_error(&e),
        }
    }
    #[cfg(not(feature = "tray"))]
    {
        let _ = on_ready;
        on_error("tray support is not compiled in (build with --features tray)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_labels_and_actions_are_shared_with_autostart() {
        // Kept here so the module surface is exercised even without the feature.
        let items = menu_items(20128, false);
        assert_eq!(items.len(), 4);
        assert_eq!(tray_action(1), TrayAction::OpenDashboard);
    }

    #[test]
    fn run_tray_without_the_feature_reports_the_build_hint() {
        #[cfg(not(feature = "tray"))]
        {
            let mut err = None;
            run_tray(20128, || panic!("must not report ready"), |e| err = Some(e.to_string()));
            let msg = err.expect("error reported");
            assert!(msg.contains("--features tray"), "{msg}");
        }
    }
}
