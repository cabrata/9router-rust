//! Interactive terminal menus. Port of `cli/src/cli/terminalUI.js`,
//! `cli/src/cli/menus/**`, `cli/src/cli/utils/**` and `cli/src/cli/api/client.js`.
//!
//! Layout mirrors the JS:
//! - [`api`] — HTTP client for the local `/api` surface (`api/client.js`)
//! - [`format`] — display formatting (`utils/{format,display,comboModelId}.js`)
//! - [`tree`] — pure menu tree, row builders and navigation (`menus/**`,
//!   `utils/menuHelper.js`, `terminalUI.js`)
//! - [`tui`] — the TTY driver that renders [`tree`] and runs each action
//!
//! Splitting navigation out of the terminal layer is what lets the menu
//! structure be tested without a TTY; see the `tree` unit tests.

pub mod api;
pub mod format;
pub mod tree;
pub mod tui;

pub use tree::{MenuId, Nav, Row, Step};
pub use tui::Terminal;

/// Start the terminal UI on `port`, returning when the user backs out of the
/// root menu (`startTerminalUI` in terminalUI.js).
pub fn run(port: u16) -> anyhow::Result<()> {
    Terminal::new(port).run()
}
