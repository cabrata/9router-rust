//! The `9router` CLI. Port of cli/ (owner: cli agent, see AGENTS.md).
//!
//! Manages the lifecycle of the 9router server: tracking it via PID file in `~/.9router/launcher.pid`,
//! presenting interactive menus, and handling graceful shutdown. The server process itself is embedded
//! in the binary (nr_server::run) and runs in-process, avoiding subprocess overhead.
//!
//! ## Architecture
//!
//! The launcher flow:
//! 1. Parse CLI arguments (port, hostname, browser preference, etc.)
//! 2. Read and clean up any previous launcher PID record
//! 3. Write current launcher PID record for next launch's cleanup
//! 4. Show startup message (warn if network-exposed)
//! 5. Open browser if requested (unless --no-browser)
//! 6. Run embedded server (blocks until shutdown via Ctrl-C / SIGTERM)
//! 7. Clean up PID record on exit
//!
//! ## Implemented modules
//!
//! - [`connect_tools`]: per-tool config writers for `connect` / `show` / `--reset`
//!   (port of cli/src/cli/commands/connectTools.js)
//! - [`launcher_process`]: PID file management for fast instance detection
//! - [`port_management`]: TCP port availability polling
//! - [`process_management`]: Platform-specific process control (kill, signal, check alive)
//! - [`ui`]: Terminal UI helpers (spinners, colors, clear screen)
//!
//! `connect`, `show`, `update` and `xai video` live in the binary's `cli_commands`
//! module; menus use `inquire` (Select / Password / Confirm).
//!
//! ## Not yet ported (future work)
//!
//! - Terminal UI mode menus (from cli/src/cli/terminalUI.js: providers/api-keys/combos/cli-tools/settings)
//! - API client commands (from cli/src/cli/api/)
//! - `xai video` generation (argument parsing is ported; the request/poll/download is stubbed)
//! - Autostart/launch-on-boot configuration
//!
//! ## TODOs and shortcuts
//!
//! - `shortcut: tray icon menu callbacks need a graphical session; `tray-icon` + `gtk`
//!   are optional deps behind `[features] tray`, so headless Linux CI needs no GTK`
//! - `shortcut: update installs via `cargo install --path crates/nr-cli``

pub mod connect_tools;
pub mod launcher_process;
pub mod port_management;
pub mod process_management;
pub mod ui;

pub use launcher_process::{read_pid_record, write_pid_record, PidRecord, PID_FILE_NAME};
pub use port_management::{is_port_in_use, wait_for_port_free};
pub use process_management::{kill_by_pid, stop_recorded_instance};
