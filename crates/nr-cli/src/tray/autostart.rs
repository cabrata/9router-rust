//! Launch-on-boot configuration. Port of `cli/src/cli/tray/autostart.js`.
//!
//! No GUI dependencies here: this only writes the platform's autostart entry
//! (`.desktop` / `.vbs` / launchd plist), so it compiles in the default build.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `APP_NAME` in autostart.js.
pub const APP_NAME: &str = "9router";
/// launchd label used on macOS.
pub const APP_LABEL: &str = "com.9router.autostart";

/// Flags the autostart entry launches the CLI with.
const AUTOSTART_ARGS: &[&str] = &["--tray", "--skip-update"];

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Absolute path of the running `9router` binary (`getCliJsPath`). Callers must
/// not write an entry pointing at a file that does not exist.
pub fn binary_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.is_file() {
            return Some(exe);
        }
    }
    // Fall back to `$PATH` so a `cargo run`-style launch still resolves.
    let name = if cfg!(windows) { "9router.exe" } else { "9router" };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// Linux `.desktop` path.
pub fn linux_desktop_path() -> PathBuf {
    home().join(".config").join("autostart").join(format!("{APP_NAME}.desktop"))
}

/// Windows Startup `.vbs` path.
pub fn windows_vbs_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join(format!("{APP_NAME}.vbs"))
    })
}

/// macOS launchd plist path.
pub fn macos_plist_path() -> PathBuf {
    home().join("Library").join("LaunchAgents").join(format!("{APP_LABEL}.plist"))
}

/// `.desktop` body (`enableLinux`).
pub fn linux_desktop_content(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=9Router\nComment=9Router API Proxy\nExec={} {}\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\n",
        exe.display(),
        AUTOSTART_ARGS.join(" ")
    )
}

/// Windows `.vbs` body (`enableWindows`), hidden window via WScript.Shell.
pub fn windows_vbs_content(exe: &Path) -> String {
    format!(
        "Set WshShell = CreateObject(\"WScript.Shell\")\nWshShell.Run \"\"\"{}\"\" {}\", 0, False\n",
        exe.display(),
        AUTOSTART_ARGS.join(" ")
    )
}

/// launchd plist body (`enableMacOS`).
pub fn macos_plist_content(exe: &Path, path_env: &str) -> String {
    let args: String = std::iter::once(exe.display().to_string())
        .chain(AUTOSTART_ARGS.iter().map(|s| s.to_string()))
        .map(|a| format!("        <string>{a}</string>\n"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{APP_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>/tmp/9router.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/9router.error.log</string>
</dict>
</plist>
"#
    )
}

/// Enable autostart. `exe` defaults to the running binary.
///
/// The JS calls this from "Hide to Tray"; failing here is never fatal, so a
/// write error returns `false` rather than an error value.
pub fn enable_auto_start(exe: Option<&Path>) -> bool {
    let Some(exe) = exe.map(Path::to_path_buf).or_else(binary_path) else {
        return false;
    };
    // Linux without a graphical session cannot autostart a tray app.
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        let path = linux_desktop_path();
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        return std::fs::write(&path, linux_desktop_content(&exe)).is_ok();
    }

    #[cfg(target_os = "windows")]
    {
        let Some(path) = windows_vbs_path() else { return false };
        if let Some(parent) = path.parent() {
            if !parent.is_dir() {
                return false;
            }
        }
        return std::fs::write(&path, windows_vbs_content(&exe)).is_ok();
    }

    #[cfg(target_os = "macos")]
    {
        let path = macos_plist_path();
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        let path_env = format!(
            "{}:/usr/local/bin:/usr/bin:/bin",
            exe.parent().map(|p| p.display().to_string()).unwrap_or_default()
        );
        if std::fs::write(&path, macos_plist_content(&exe, &path_env)).is_err() {
            return false;
        }
        // Skip launchctl when the running process IS the agent: unload would
        // send us SIGTERM (the reason isAgentSelfMacOS exists in the JS too).
        if !is_agent_self_macos() {
            let _ = Command::new("launchctl").args(["unload", &path.display().to_string()]).status();
            let _ = Command::new("launchctl")
                .args(["load", "-w", &path.display().to_string()])
                .status();
        }
        return true;
    }

    #[allow(unreachable_code)]
    false
}

/// Disable autostart by removing the platform entry.
pub fn disable_auto_start() -> bool {
    #[cfg(target_os = "linux")]
    {
        let path = linux_desktop_path();
        if path.exists() {
            return std::fs::remove_file(path).is_ok();
        }
        return true;
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(path) = windows_vbs_path() {
            if path.exists() {
                return std::fs::remove_file(path).is_ok();
            }
        }
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        let path = macos_plist_path();
        if !is_agent_self_macos() {
            let _ = Command::new("launchctl").args(["unload", &path.display().to_string()]).status();
        }
        if path.exists() {
            return std::fs::remove_file(path).is_ok();
        }
        return true;
    }

    #[allow(unreachable_code)]
    false
}

/// Is autostart currently enabled?
///
/// On macOS the plist must exist *and* launchd must have the agent loaded, so
/// the tray menu never claims "enabled" for a failed agent — same rule as the
/// JS `isAutoStartEnabled`.
pub fn is_auto_start_enabled() -> bool {
    #[cfg(target_os = "linux")]
    {
        return linux_desktop_path().is_file();
    }

    #[cfg(target_os = "windows")]
    {
        return windows_vbs_path().map(|p| p.is_file()).unwrap_or(false);
    }

    #[cfg(target_os = "macos")]
    {
        let path = macos_plist_path();
        if !path.is_file() {
            return false;
        }
        return Command::new("launchctl")
            .args(["list", APP_LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    }

    #[allow(unreachable_code)]
    false
}

/// True when this process is the launchd agent itself (see the JS
/// `isAgentSelfMacOS` comment: unloading would SIGTERM us).
pub fn is_agent_self_macos() -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(output) = Command::new("launchctl").args(["list", APP_LABEL]).output() else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let Some(pid) = text
            .split("\"PID\"")
            .nth(1)
            .and_then(|rest| rest.split('=').nth(1))
            .and_then(|n| n.trim().split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse::<u32>().ok())
        else {
            return false;
        };
        pid == std::process::id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Tray menu labels (`buildMenuItems`), shared with the icon menu.
pub fn menu_items(port: u16, autostart_enabled: bool) -> Vec<(String, bool)> {
    vec![
        (format!("9Router (Port {port})"), false),
        ("Open Dashboard".to_string(), true),
        (
            if autostart_enabled {
                "✓ Auto-start Enabled".to_string()
            } else {
                "Enable Auto-start".to_string()
            },
            true,
        ),
        ("Quit".to_string(), true),
    ]
}

/// Menu indexes shared between platforms (`MENU_INDEX`).
pub const MENU_INDEX_STATUS: usize = 0;
pub const MENU_INDEX_DASHBOARD: usize = 1;
pub const MENU_INDEX_AUTOSTART: usize = 2;
pub const MENU_INDEX_QUIT: usize = 3;

/// What a tray click should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Status line, not clickable.
    None,
    OpenDashboard,
    ToggleAutostart,
    Quit,
}

/// Map a menu index to an action (`handleClick`).
pub fn tray_action(index: usize) -> TrayAction {
    match index {
        MENU_INDEX_DASHBOARD => TrayAction::OpenDashboard,
        MENU_INDEX_AUTOSTART => TrayAction::ToggleAutostart,
        MENU_INDEX_QUIT => TrayAction::Quit,
        _ => TrayAction::None,
    }
}

/// Label to write after an autostart toggle.
pub fn autostart_label(enabled: bool) -> &'static str {
    if enabled {
        "✓ Auto-start Enabled"
    } else {
        "Enable Auto-start"
    }
}

/// Is a tray icon possible on this platform/session (`isTraySupported`)?
pub fn is_tray_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Dashboard URL for the tray's "Open Dashboard" item.
pub fn dashboard_url(port: u16) -> String {
    format!("http://localhost:{port}/dashboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_entries_launch_the_cli_in_tray_mode() {
        let exe = Path::new("/usr/local/bin/9router");
        let desktop = linux_desktop_content(exe);
        assert!(desktop.starts_with("[Desktop Entry]\n"), "{desktop}");
        assert!(desktop.contains("Exec=/usr/local/bin/9router --tray --skip-update"), "{desktop}");
        assert!(desktop.contains("X-GNOME-Autostart-enabled=true"), "{desktop}");

        let vbs = windows_vbs_content(Path::new(r"C:\Program Files\9router\9router.exe"));
        assert!(vbs.contains("--tray --skip-update"), "{vbs}");
        assert!(vbs.contains(", 0, False"), "hidden window: {vbs}");

        let plist = macos_plist_content(exe, "/usr/local/bin:/usr/bin");
        assert!(plist.contains("<string>/usr/local/bin/9router</string>"), "{plist}");
        assert!(plist.contains("<string>--tray</string>"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        assert!(plist.contains("com.9router.autostart"), "{plist}");
    }

    #[test]
    fn autostart_paths_are_under_home_or_appdata() {
        let desktop = linux_desktop_path();
        assert!(desktop.ends_with(".config/autostart/9router.desktop"), "{desktop:?}");
        let plist = macos_plist_path();
        assert!(plist.ends_with("Library/LaunchAgents/com.9router.autostart.plist"), "{plist:?}");
    }

    #[test]
    fn tray_menu_matches_the_js_items_and_indexes() {
        let items = menu_items(20128, false);
        assert_eq!(items.len(), 4);
        assert_eq!(items[MENU_INDEX_STATUS].0, "9Router (Port 20128)");
        assert!(!items[MENU_INDEX_STATUS].1, "status row is disabled");
        assert_eq!(items[MENU_INDEX_DASHBOARD].0, "Open Dashboard");
        assert_eq!(items[MENU_INDEX_AUTOSTART].0, "Enable Auto-start");
        assert_eq!(items[MENU_INDEX_QUIT].0, "Quit");

        let enabled = menu_items(3000, true);
        assert_eq!(enabled[MENU_INDEX_AUTOSTART].0, "✓ Auto-start Enabled");
        assert_eq!(autostart_label(true), "✓ Auto-start Enabled");
        assert_eq!(autostart_label(false), "Enable Auto-start");
    }

    #[test]
    fn tray_clicks_map_to_actions() {
        assert_eq!(tray_action(0), TrayAction::None);
        assert_eq!(tray_action(1), TrayAction::OpenDashboard);
        assert_eq!(tray_action(2), TrayAction::ToggleAutostart);
        assert_eq!(tray_action(3), TrayAction::Quit);
        assert_eq!(tray_action(9), TrayAction::None);
    }

    #[test]
    fn dashboard_url_points_at_the_local_dashboard() {
        assert_eq!(dashboard_url(20128), "http://localhost:20128/dashboard");
    }

    #[test]
    fn tray_support_follows_the_graphical_session() {
        // Purely a platform check; it must never panic headless.
        let _ = is_tray_supported();
        let _ = is_auto_start_enabled();
    }
}
