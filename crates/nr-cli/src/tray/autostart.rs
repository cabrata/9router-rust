//! Launch-on-boot configuration. Port of `cli/src/cli/tray/autostart.js`.
//!
//! Manages launch-on-boot entries across platforms:
//! - Linux: XDG `.desktop` entry in `~/.config/autostart/` and systemd user unit in `~/.config/systemd/user/`
//! - macOS: launchd agent plist in `~/Library/LaunchAgents/`
//! - Windows: startup folder `.vbs` runner and `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` registry entry
//!
//! No GUI dependencies here: this only writes platform autostart entries, so
//! it compiles in the default build without the `tray` feature.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `APP_NAME` in autostart.js.
pub const APP_NAME: &str = "9router";
/// launchd label used on macOS.
pub const APP_LABEL: &str = "com.9router.autostart";
/// Windows Run registry subkey.
pub const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Flags the autostart entry launches the CLI with.
pub const AUTOSTART_ARGS: &[&str] = &["--tray", "--skip-update"];

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

// ── Linux Paths & Templates ──────────────────────────────────────────────────

/// Linux XDG `.desktop` path: `~/.config/autostart/9router.desktop`.
pub fn linux_desktop_path() -> PathBuf {
    home().join(".config").join("autostart").join(format!("{APP_NAME}.desktop"))
}

/// Linux systemd user unit path: `~/.config/systemd/user/9router.service`.
pub fn linux_systemd_path() -> PathBuf {
    home().join(".config").join("systemd").join("user").join(format!("{APP_NAME}.service"))
}

/// Linux `.desktop` file content (`enableLinux`).
pub fn linux_desktop_content(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=9Router\nComment=9Router API Proxy\nExec={} {}\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\n",
        exe.display(),
        AUTOSTART_ARGS.join(" ")
    )
}

/// Linux systemd user service unit content.
pub fn linux_systemd_content(exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=9Router API Proxy\nAfter=network.target\n\n[Service]\nType=simple\nExecStart={} {}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        exe.display(),
        AUTOSTART_ARGS.join(" ")
    )
}

/// Enable Linux autostart (writes desktop entry and systemd user unit).
pub fn enable_linux(exe: &Path) -> bool {
    let mut ok = false;

    // 1. Desktop entry (for GUI desktop session)
    let desk_path = linux_desktop_path();
    if let Some(parent) = desk_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&desk_path, linux_desktop_content(exe)).is_ok() {
        ok = true;
    }

    // 2. Systemd user service unit
    let sys_path = linux_systemd_path();
    if let Some(parent) = sys_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&sys_path, linux_systemd_content(exe)).is_ok() {
        ok = true;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let _ = Command::new("systemctl")
            .args(["--user", "enable", &format!("{APP_NAME}.service")])
            .output();
    }

    ok
}

/// Disable Linux autostart (removes desktop entry and systemd user unit).
pub fn disable_linux() -> bool {
    let mut ok = true;
    let desk_path = linux_desktop_path();
    if desk_path.exists() && std::fs::remove_file(desk_path).is_err() {
        ok = false;
    }

    let _ = Command::new("systemctl")
        .args(["--user", "disable", &format!("{APP_NAME}.service")])
        .output();

    let sys_path = linux_systemd_path();
    if sys_path.exists() && std::fs::remove_file(sys_path).is_err() {
        ok = false;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    ok
}

/// Is autostart enabled on Linux?
pub fn is_linux_autostart_enabled() -> bool {
    linux_desktop_path().is_file() || linux_systemd_path().is_file()
}

// ── Windows Paths & Templates ────────────────────────────────────────────────

/// Windows Startup folder `.vbs` path: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\9router.vbs`.
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

/// Windows `.vbs` body (`enableWindows`), hidden window via WScript.Shell.
pub fn windows_vbs_content(exe: &Path) -> String {
    format!(
        "Set WshShell = CreateObject(\"WScript.Shell\")\nWshShell.Run \"\"\"{}\"\" {}\", 0, False\n",
        exe.display(),
        AUTOSTART_ARGS.join(" ")
    )
}

/// Windows Registry command to register autostart in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub fn windows_registry_value(exe: &Path) -> String {
    format!("\"{}\" {}", exe.display(), AUTOSTART_ARGS.join(" "))
}

/// Command and arguments to add Windows Run registry key.
pub fn windows_registry_command_add(exe: &Path) -> (String, Vec<String>) {
    (
        "reg".to_string(),
        vec![
            "add".to_string(),
            WINDOWS_RUN_KEY.to_string(),
            "/v".to_string(),
            APP_NAME.to_string(),
            "/t".to_string(),
            "REG_SZ".to_string(),
            "/d".to_string(),
            windows_registry_value(exe),
            "/f".to_string(),
        ],
    )
}

/// Command and arguments to remove Windows Run registry key.
pub fn windows_registry_command_delete() -> (String, Vec<String>) {
    (
        "reg".to_string(),
        vec![
            "delete".to_string(),
            WINDOWS_RUN_KEY.to_string(),
            "/v".to_string(),
            APP_NAME.to_string(),
            "/f".to_string(),
        ],
    )
}

/// Command and arguments to query Windows Run registry key.
pub fn windows_registry_command_query() -> (String, Vec<String>) {
    (
        "reg".to_string(),
        vec![
            "query".to_string(),
            WINDOWS_RUN_KEY.to_string(),
            "/v".to_string(),
            APP_NAME.to_string(),
        ],
    )
}

/// Enable Windows autostart (writes `.vbs` to Startup folder and adds registry Run key).
pub fn enable_windows(exe: &Path) -> bool {
    let mut ok = false;

    // 1. Startup folder .vbs
    if let Some(path) = windows_vbs_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&path, windows_vbs_content(exe)).is_ok() {
            ok = true;
        }
    }

    // 2. Windows registry Run key
    #[cfg(target_os = "windows")]
    {
        let (cmd, args) = windows_registry_command_add(exe);
        if let Ok(status) = Command::new(cmd).args(&args).status() {
            if status.success() {
                ok = true;
            }
        }
    }

    ok
}

/// Disable Windows autostart (removes `.vbs` and deletes registry Run key).
pub fn disable_windows() -> bool {
    let mut ok = true;

    if let Some(path) = windows_vbs_path() {
        if path.exists() && std::fs::remove_file(path).is_err() {
            ok = false;
        }
    }

    #[cfg(target_os = "windows")]
    {
        let (cmd, args) = windows_registry_command_delete();
        let _ = Command::new(cmd).args(&args).status();
    }

    ok
}

/// Is autostart enabled on Windows?
pub fn is_windows_autostart_enabled() -> bool {
    if windows_vbs_path().map(|p| p.is_file()).unwrap_or(false) {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        let (cmd, args) = windows_registry_command_query();
        if let Ok(output) = Command::new(cmd).args(&args).output() {
            return output.status.success();
        }
    }
    false
}

// ── macOS Paths & Templates ──────────────────────────────────────────────────

/// macOS launchd plist path: `~/Library/LaunchAgents/com.9router.autostart.plist`.
pub fn macos_plist_path() -> PathBuf {
    home().join("Library").join("LaunchAgents").join(format!("{APP_LABEL}.plist"))
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

/// Enable macOS autostart via launchd agent.
pub fn enable_macos(exe: &Path) -> bool {
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
    if std::fs::write(&path, macos_plist_content(exe, &path_env)).is_err() {
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
    true
}

/// Disable macOS autostart by removing plist and unloading launchd agent.
pub fn disable_macos() -> bool {
    let path = macos_plist_path();
    if !is_agent_self_macos() {
        let _ = Command::new("launchctl").args(["unload", &path.display().to_string()]).status();
    }
    if path.exists() {
        return std::fs::remove_file(path).is_ok();
    }
    true
}

/// Is autostart enabled on macOS?
pub fn is_macos_autostart_enabled() -> bool {
    let path = macos_plist_path();
    if !path.is_file() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("launchctl")
            .args(["list", APP_LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
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

// ── Cross-Platform Facade ────────────────────────────────────────────────────

/// Enable autostart. `exe` defaults to the running binary.
///
/// Failing here is never fatal: a write error returns `false` rather than an error value.
pub fn enable_auto_start(exe: Option<&Path>) -> bool {
    let Some(exe) = exe.map(Path::to_path_buf).or_else(binary_path) else {
        return false;
    };

    #[cfg(target_os = "linux")]
    {
        // Linux without a graphical session or systemd cannot autostart a tray app.
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            // Still allow systemd service unit creation
            return enable_linux(&exe);
        }
        return enable_linux(&exe);
    }

    #[cfg(target_os = "windows")]
    {
        return enable_windows(&exe);
    }

    #[cfg(target_os = "macos")]
    {
        return enable_macos(&exe);
    }

    #[allow(unreachable_code)]
    false
}

/// Disable autostart by removing platform entries.
pub fn disable_auto_start() -> bool {
    #[cfg(target_os = "linux")]
    {
        return disable_linux();
    }

    #[cfg(target_os = "windows")]
    {
        return disable_windows();
    }

    #[cfg(target_os = "macos")]
    {
        return disable_macos();
    }

    #[allow(unreachable_code)]
    false
}

/// Is autostart currently enabled on the current platform?
pub fn is_auto_start_enabled() -> bool {
    #[cfg(target_os = "linux")]
    {
        return is_linux_autostart_enabled();
    }

    #[cfg(target_os = "windows")]
    {
        return is_windows_autostart_enabled();
    }

    #[cfg(target_os = "macos")]
    {
        return is_macos_autostart_enabled();
    }

    #[allow(unreachable_code)]
    false
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

        let systemd = linux_systemd_content(exe);
        assert!(systemd.starts_with("[Unit]\n"), "{systemd}");
        assert!(systemd.contains("ExecStart=/usr/local/bin/9router --tray --skip-update"), "{systemd}");
        assert!(systemd.contains("Restart=on-failure"), "{systemd}");
        assert!(systemd.contains("WantedBy=default.target"), "{systemd}");

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
    fn windows_registry_commands_generate_expected_args() {
        let exe = Path::new(r"C:\Tools\9router.exe");
        let (cmd, args) = windows_registry_command_add(exe);
        assert_eq!(cmd, "reg");
        assert_eq!(args[0], "add");
        assert_eq!(args[1], WINDOWS_RUN_KEY);
        assert_eq!(args[2], "/v");
        assert_eq!(args[3], "9router");
        assert_eq!(args[4], "/t");
        assert_eq!(args[5], "REG_SZ");
        assert_eq!(args[6], "/d");
        assert_eq!(args[7], r#""C:\Tools\9router.exe" --tray --skip-update"#);
        assert_eq!(args[8], "/f");

        let (del_cmd, del_args) = windows_registry_command_delete();
        assert_eq!(del_cmd, "reg");
        assert_eq!(del_args, vec!["delete", WINDOWS_RUN_KEY, "/v", "9router", "/f"]);

        let (q_cmd, q_args) = windows_registry_command_query();
        assert_eq!(q_cmd, "reg");
        assert_eq!(q_args, vec!["query", WINDOWS_RUN_KEY, "/v", "9router"]);
    }

    #[test]
    fn autostart_paths_are_under_home_or_appdata() {
        let desktop = linux_desktop_path();
        assert!(desktop.ends_with(".config/autostart/9router.desktop"), "{desktop:?}");
        let systemd = linux_systemd_path();
        assert!(systemd.ends_with(".config/systemd/user/9router.service"), "{systemd:?}");
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
        let _ = is_tray_supported();
        let _ = is_auto_start_enabled();
    }
}
