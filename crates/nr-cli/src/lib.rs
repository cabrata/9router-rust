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
//! - [`launcher_process`]: PID file management for fast instance detection
//! - [`port_management`]: TCP port availability polling
//! - [`process_management`]: Platform-specific process control (kill, signal, check alive)
//! - [`ui`]: Terminal UI helpers (spinners, colors, clear screen)
//!
//! ## Not yet ported (future work)
//!
//! - Interactive menus (from cli/src/cli/terminalUI.js) — use `inquire` crate for dialogs
//! - System tray icon (from cli/src/cli/tray/) — use `tray-icon` crate with optional GTK feature
//! - Update checking and installation
//! - Terminal UI mode vs Web UI mode selector
//! - API client commands (from cli/src/cli/api/)
//! - CLI tool configuration commands (connect, show, xai video)
//! - Autostart/launch-on-boot configuration
//!
//! ## TODOs and shortcuts
//!
//! - `shortcut: tray icon requires GTK dev libs on Linux; feature-gated with fallback message`
//! - `TODO(dedupe): interactive menus once inquire integration is complete`

pub mod launcher_process;
pub mod port_management;
pub mod process_management;
pub mod ui;

pub use launcher_process::{read_pid_record, write_pid_record, PidRecord, PID_FILE_NAME};
pub use port_management::{is_port_in_use, wait_for_port_free};
pub use process_management::{kill_by_pid, stop_recorded_instance};
