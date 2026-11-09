//! Menu tree + navigation for the terminal UI. Port of
//! `cli/src/cli/terminalUI.js`, `cli/src/cli/menus/**` and
//! `cli/src/cli/utils/menuHelper.js`.
//!
//! Everything here is pure data and pure functions: the TTY layer in
//! [`super::tui`] renders these and feeds selections back. That keeps the
//! navigation rules (`← Back`, breadcrumbs, "exit after delete") testable
//! without a terminal.

use serde_json::Value;

use super::format;

/// Breadcrumb root, same string the JS uses (`basePath` in terminalUI.js).
pub const ROOT_LABEL: &str = "9Router";

/// One node of the interactive menu tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuId {
    Main,
    Providers,
    ProviderDetail(ProviderSlot),
    CustomProviders,
    CustomNodeDetail(ProviderSlot),
    ApiKeys,
    Combos,
    ComboDetail(ProviderSlot),
    CliTools,
    ClaudeCode,
    Codex,
    Droid,
    OpenClaw,
    OpenCode,
    Hermes,
    Settings,
    Usage,
    Logs,
}

/// A dynamically-sized id slot for detail menus (the JS passes the object
/// straight into the closure; a Rust enum needs an index instead).
///
/// `slot` indexes the list the parent menu rendered, so the TUI can re-fetch
/// the list and pick the same row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderSlot(pub usize);

impl MenuId {
    /// The menu one level up, or `None` at the root — this is what drives the
    /// `← Back` item and Esc handling.
    pub fn parent(self) -> Option<MenuId> {
        match self {
            MenuId::Main => None,
            MenuId::Providers
            | MenuId::ApiKeys
            | MenuId::Combos
            | MenuId::CliTools
            | MenuId::Settings
            | MenuId::Usage
            | MenuId::Logs => Some(MenuId::Main),
            MenuId::ProviderDetail(_) | MenuId::CustomProviders => Some(MenuId::Providers),
            MenuId::CustomNodeDetail(_) => Some(MenuId::CustomProviders),
            MenuId::ComboDetail(_) => Some(MenuId::Combos),
            MenuId::ClaudeCode
            | MenuId::Codex
            | MenuId::Droid
            | MenuId::OpenClaw
            | MenuId::OpenCode
            | MenuId::Hermes => Some(MenuId::CliTools),
        }
    }

    /// Menu title, matching the JS `title:` strings.
    pub fn title(self) -> &'static str {
        match self {
            MenuId::Main => "📡 9Router Terminal UI",
            MenuId::Providers => "🔌 Providers Management",
            MenuId::ProviderDetail(_) => "🔌 Provider",
            MenuId::CustomProviders => "🔧 Custom Providers",
            MenuId::CustomNodeDetail(_) => "🔧 Custom Provider",
            MenuId::ApiKeys => "🔑 API Keys Management",
            MenuId::Combos => "🔀 Combos Management",
            MenuId::ComboDetail(_) => "🔀 Combo",
            MenuId::CliTools => "🔧 CLI Tools",
            MenuId::ClaudeCode => "🔧 Claude Code Settings",
            MenuId::Codex => "🤖 Codex CLI Settings",
            MenuId::Droid => "🤖 Factory Droid Settings",
            MenuId::OpenClaw => "🦞 Open Claw Settings",
            MenuId::OpenCode => "💻 OpenCode CLI Settings",
            MenuId::Hermes => "⚡ Hermes Agent Settings",
            MenuId::Settings => "⚙️  Settings",
            MenuId::Usage => "📊 Usage",
            MenuId::Logs => "📜 Request Logs",
        }
    }

    /// Label used in a breadcrumb (`basePath` entries in the JS).
    pub fn crumb(self) -> &'static str {
        match self {
            MenuId::Main => ROOT_LABEL,
            MenuId::Providers => "Providers",
            MenuId::ProviderDetail(_) => "Connection",
            MenuId::CustomProviders => "Custom Providers",
            MenuId::CustomNodeDetail(_) => "Custom Provider",
            MenuId::ApiKeys => "API Keys",
            MenuId::Combos => "Combos",
            MenuId::ComboDetail(_) => "Combo",
            MenuId::CliTools => "CLI Tools",
            MenuId::ClaudeCode => "Claude Code",
            MenuId::Codex => "Codex CLI",
            MenuId::Droid => "Factory Droid",
            MenuId::OpenClaw => "Open Claw",
            MenuId::OpenCode => "OpenCode",
            MenuId::Hermes => "Hermes",
            MenuId::Settings => "Settings",
            MenuId::Usage => "Usage",
            MenuId::Logs => "Logs",
        }
    }
}

/// What selecting a row does. Kept data-only so the tree needs no closures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Descend into another menu (the row carries any slot it needs).
    Open(MenuId),
    /// Leave the current menu (`← Back`), or exit at the root.
    Back,
    /// Leave the whole terminal UI.
    Exit,
    /// A leaf action the TUI performs (`Action` carries the payload).
    Act(Action),
}

/// One rendered row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub step: Step,
    /// Non-selectable separator line (the JS `isSeparator` provider entry).
    pub separator: bool,
}

impl Row {
    pub fn new(label: impl Into<String>, step: Step) -> Self {
        Self { label: label.into(), step, separator: false }
    }
    pub fn separator(label: impl Into<String>) -> Self {
        Self { label: label.into(), step: Step::Act(Action::Nothing), separator: true }
    }
}

/// Leaf actions. The payload is whatever the TUI needs to call the API again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    // Providers
    AddConnection(usize),
    /// Open the per-connection action menu for the connection at this index.
    ConnectionActions(ProviderSlot),
    RenameConnection(String),
    TestConnection(String),
    DeleteConnection(String),
    // Custom provider nodes
    AddNode,
    AddNodeConnection(usize),
    EditNode(String),
    DeleteNode(String),
    // API keys
    CreateApiKey,
    ViewApiKey(String),
    CopyApiKey(String),
    DeleteApiKey(String),
    // Combos
    CreateCombo,
    EditCombo(String),
    DeleteCombo(String),
    // CLI tools
    CliToolQuickSetup(&'static str),
    CliToolReset(&'static str),
    CliToolSelectModel(&'static str, &'static str),
    // Settings
    TunnelOn,
    TunnelOff,
    ToggleRtk,
    ToggleHeadroom,
    ResetPassword,
    ResetAuthMode,
    // Usage
    SetUsagePeriod(&'static str),
}

/// A provider row in the Providers menu: the JS keeps two ordered tables
/// (`OAUTH_PROVIDERS`, `APIKEY_PROVIDERS`) keyed by provider id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEntry {
    pub id: &'static str,
    pub alias: &'static str,
    pub name: &'static str,
    pub oauth: bool,
}

/// OAuth providers, in JS declaration order.
pub const OAUTH_PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry { id: "claude", alias: "cc", name: "Claude Code", oauth: true },
    ProviderEntry { id: "codex", alias: "cx", name: "OpenAI Codex", oauth: true },
    ProviderEntry { id: "gemini-cli", alias: "gc", name: "Gemini CLI", oauth: true },
    ProviderEntry { id: "github", alias: "gh", name: "GitHub Copilot", oauth: true },
    ProviderEntry { id: "antigravity", alias: "ag", name: "Antigravity", oauth: true },
    ProviderEntry { id: "iflow", alias: "if", name: "iFlow AI", oauth: true },
    ProviderEntry { id: "qwen", alias: "qw", name: "Qwen Code", oauth: true },
    ProviderEntry { id: "kiro", alias: "kr", name: "Kiro AI", oauth: true },
    ProviderEntry { id: "glm", alias: "glm", name: "Zai GLM Coding", oauth: true },
];

/// API-key providers, in JS declaration order. `glm` is listed twice in the
/// JS too (OAuth and API key) — keep that so the menu matches.
pub const APIKEY_PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry { id: "openrouter", alias: "openrouter", name: "OpenRouter", oauth: false },
    ProviderEntry { id: "glm", alias: "glm", name: "Zai GLM Coding", oauth: false },
    ProviderEntry { id: "minimax", alias: "minimax", name: "Minimax Coding", oauth: false },
    ProviderEntry { id: "kimi", alias: "kimi", name: "Kimi", oauth: false },
    ProviderEntry { id: "openai", alias: "openai", name: "OpenAI", oauth: false },
    ProviderEntry { id: "anthropic", alias: "anthropic", name: "Anthropic", oauth: false },
    ProviderEntry { id: "gemini", alias: "gemini", name: "Gemini", oauth: false },
];

/// Providers using the device-code flow in the JS (`DEVICE_CODE_PROVIDERS`).
pub const DEVICE_CODE_PROVIDERS: &[&str] = &["github", "qwen", "kiro", "glm"];

/// `ALL_PROVIDERS` lookup by id or alias (OAuth table first, then API key table).
pub fn provider_by_id(id: &str) -> Option<&'static ProviderEntry> {
    OAUTH_PROVIDERS.iter().chain(APIKEY_PROVIDERS.iter()).find(|p| p.id == id || p.alias == id)
}

/// Static per-provider model ids, ported from the JS `PROVIDER_MODELS` table.
/// Only the menu header needs the first five, so the lists are capped there
/// plus the total count for the "(+N more)" suffix.
pub fn provider_model_preview(alias: &str) -> (Vec<String>, usize) {
    let models: &[&str] = match alias {
        "cc" => &["claude-opus-4-5-20251101", "claude-sonnet-4-5-20250929", "claude-haiku-4-5-20251001"],
        "cx" => &[
            "gpt-5.2-codex", "gpt-5.2", "gpt-5.1-codex-max", "gpt-5.1-codex", "gpt-5.1-codex-mini",
            "gpt-5.1", "gpt-5-codex", "gpt-5-codex-mini",
        ],
        "gc" => &[
            "gemini-3-flash-preview", "gemini-3-pro-preview", "gemini-2.5-pro", "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
        ],
        "qw" => &["qwen3-coder-plus", "qwen3-coder-flash", "vision-model"],
        "if" => &[
            "qwen3-coder-plus", "kimi-k2", "kimi-k2-thinking", "deepseek-r1", "deepseek-v3.2-chat",
            "deepseek-v3.2-reasoner", "minimax-m2", "glm-4.7",
        ],
        "ag" => &[
            "gemini-3.8-flash-high", "gemini-3.8-flash-medium", "gemini-3.8-flash-low",
            "gemini-3.7-flash-high", "gemini-3.7-flash-medium", "gemini-3.7-flash-low",
            "gemini-3.6-flash-high", "gemini-3.6-flash-medium", "gemini-3.6-flash-low",
            "gemini-3-flash-agent", "gemini-3.5-flash-low", "gemini-3.5-flash-extra-low",
            "gemini-pro-agent", "gemini-3.1-pro-low", "claude-sonnet-5-5", "claude-sonnet-5-5-low",
            "claude-sonnet-5-5-medium", "claude-sonnet-5-5-high", "claude-opus-5-5",
            "claude-opus-5-5-low", "claude-opus-5-5-medium", "claude-opus-5-5-high",
            "claude-sonnet-4-6", "claude-opus-4-6-thinking", "gpt-oss-120b-medium", "gemini-3-flash",
        ],
        "gh" => &[
            "gpt-5", "gpt-5-mini", "gpt-5.1-codex", "gpt-5.1-codex-max", "gpt-4.1",
            "claude-4.5-sonnet", "claude-4.5-opus", "claude-4.5-haiku", "gemini-3-pro",
            "gemini-3-flash", "gemini-2.5-pro", "grok-code-fast-1",
        ],
        "kr" => &["claude-sonnet-5", "claude-sonnet-4.5", "claude-haiku-4.5"],
        "openai" => &["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "o1", "o1-mini"],
        "anthropic" => &["claude-sonnet-4-20250514", "claude-opus-4-20250514", "claude-3-5-sonnet-20241022"],
        "gemini" => &[
            "gemini-3.8-flash", "gemini-3.7-flash", "gemini-3.6-flash", "gemini-3.5-flash-lite",
            "gemini-3-pro-preview", "gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.5-flash-lite",
        ],
        "openrouter" => &["auto"],
        "glm" => &["glm-4.7", "glm-4.6v"],
        "kimi" => &["kimi-latest"],
        "minimax" => &["MiniMax-M2.1"],
        _ => &[],
    };
    let preview = models.iter().take(5).map(|m| format!("{alias}/{m}")).collect();
    (preview, models.len())
}

/// `countConnectionsByProvider` — provider id → connection count.
pub fn count_connections_by_provider(connections: &[Value]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for conn in connections {
        let Some(provider) = conn
            .get("provider")
            .and_then(Value::as_str)
            .or_else(|| conn.get("providerId").and_then(Value::as_str))
        else {
            continue;
        };
        match counts.iter_mut().find(|(id, _)| id == provider) {
            Some((_, n)) => *n += 1,
            None => counts.push((provider.to_string(), 1)),
        }
    }
    counts
}

fn count_for(connections: &[Value], provider_id: &str) -> usize {
    connections
        .iter()
        .filter(|c| {
            c.get("provider").and_then(Value::as_str) == Some(provider_id)
                || c.get("providerId").and_then(Value::as_str) == Some(provider_id)
        })
        .count()
}

// ── pure row builders ───────────────────────────────────────────────────────

/// Main menu rows (terminalUI.js `items`), with the extra Usage and Logs
/// entries this build exposes.
pub fn main_rows() -> Vec<Row> {
    vec![
        Row::new("Providers", Step::Open(MenuId::Providers)),
        Row::new("API Keys", Step::Open(MenuId::ApiKeys)),
        Row::new("Combos", Step::Open(MenuId::Combos)),
        Row::new("CLI Tools", Step::Open(MenuId::CliTools)),
        Row::new("Settings", Step::Open(MenuId::Settings)),
        Row::new("Usage", Step::Open(MenuId::Usage)),
        Row::new("Logs", Step::Open(MenuId::Logs)),
    ]
}

/// Providers menu rows: OAuth providers, API-key providers, then the custom
/// provider node entry. `node_count` comes from `/api/provider-nodes`.
pub fn provider_rows(connections: &[Value], node_count: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for p in OAUTH_PROVIDERS {
        rows.push(Row::new(
            format!("{} (OAuth) - {} Connected", p.name, count_for(connections, p.id)),
            Step::Open(MenuId::ProviderDetail(ProviderSlot(
                rows.len(),
            ))),
        ));
    }
    for p in APIKEY_PROVIDERS {
        rows.push(Row::new(
            format!("{} (API) - {} Connected", p.name, count_for(connections, p.id)),
            Step::Open(MenuId::ProviderDetail(ProviderSlot(rows.len()))),
        ));
    }
    rows.push(Row::separator("── Custom Providers ──"));
    rows.push(Row::new(
        format!("Custom Providers - {node_count} Configured"),
        Step::Open(MenuId::CustomProviders),
    ));
    rows
}

/// The provider entry a `ProviderSlot` points at (OAuth rows first).
pub fn provider_for_slot(slot: ProviderSlot) -> Option<&'static ProviderEntry> {
    OAUTH_PROVIDERS.iter().chain(APIKEY_PROVIDERS.iter()).nth(slot.0)
}

/// Header block for a provider detail menu (`buildProviderHeader`).
pub fn provider_header(entry: &ProviderEntry) -> String {
    let (preview, total) = provider_model_preview(entry.alias);
    let mut lines = vec![format!("Alias: {}", entry.alias)];
    if preview.is_empty() {
        lines.push("Models: No models configured".to_string());
    } else {
        let more = if total > preview.len() {
            format!(" (+{} more)", total - preview.len())
        } else {
            String::new()
        };
        lines.push(format!("Models: {}{more}", preview.join(", ")));
    }
    lines.join("\n")
}

/// Provider detail rows: connections of that provider plus the create action.
pub fn connection_rows(connections: &[Value], provider_id: &str) -> Vec<Row> {
    let mut rows = vec![Row::new(
        "Add New Connection",
        Step::Act(Action::AddConnection(0)),
    )];
    for (i, conn) in connections.iter().enumerate() {
        let is_mine = conn.get("provider").and_then(Value::as_str) == Some(provider_id)
            || conn.get("providerId").and_then(Value::as_str) == Some(provider_id);
        if !is_mine {
            continue;
        }
        rows.push(Row::new(
            format::connection_label(conn),
            Step::Act(Action::ConnectionActions(ProviderSlot(i))),
        ));
    }
    rows
}

/// Connection action rows (`showConnectionActions`).
pub fn connection_action_rows(conn: &Value) -> Vec<Row> {
    let id = conn.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = format::connection_name(conn);
    let status = match conn.get("testStatus").and_then(Value::as_str) {
        Some("active") => "✓ Active",
        Some("error") => "✗ Error",
        _ => "? Unknown",
    };
    vec![
        Row::new(format!("Connection: {name}"), Step::Act(Action::Nothing)),
        Row::new(format!("Status: {status}"), Step::Act(Action::Nothing)),
        Row::new("Rename Connection", Step::Act(Action::RenameConnection(id.clone()))),
        Row::new("Test Connection", Step::Act(Action::TestConnection(id.clone()))),
        Row::new("Delete Connection", Step::Act(Action::DeleteConnection(id))),
    ]
}

/// API key rows: create action then one row per key (`showListMenu`).
pub fn api_key_rows(keys: &[Value]) -> Vec<Row> {
    let mut rows = vec![Row::new("Create New API Key", Step::Act(Action::CreateApiKey))];
    for key in keys {
        let id = key.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        rows.push(Row::new(format::api_key_label(key), Step::Act(Action::ViewApiKey(id))));
    }
    rows
}

/// API key detail rows (`showKeyActions`).
pub fn api_key_action_rows(key: &Value) -> Vec<Row> {
    let id = key.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    vec![
        Row::new("View Full Key", Step::Act(Action::ViewApiKey(id.clone()))),
        Row::new("Copy to Clipboard", Step::Act(Action::CopyApiKey(id.clone()))),
        Row::new("Delete Key", Step::Act(Action::DeleteApiKey(id))),
    ]
}

/// Combo rows: create action then one row per combo.
pub fn combo_rows(combos: &[Value]) -> Vec<Row> {
    let mut rows = vec![Row::new("Create New Combo", Step::Act(Action::CreateCombo))];
    for combo in combos {
        let id = combo.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        rows.push(Row::new(format::combo_label(combo, 40), Step::Act(Action::EditCombo(id))));
    }
    rows
}

/// Combo detail rows (`showComboActions`).
pub fn combo_action_rows(combo: &Value) -> Vec<Row> {
    let id = combo.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    vec![
        Row::new("Edit Combo", Step::Act(Action::EditCombo(id.clone()))),
        Row::new("Delete Combo", Step::Act(Action::DeleteCombo(id))),
    ]
}

/// Custom provider node rows.
pub fn custom_node_rows(nodes: &[Value]) -> Vec<Row> {
    let mut rows = vec![Row::new("➕ Add Custom Provider", Step::Act(Action::AddNode))];
    for (i, node) in nodes.iter().enumerate() {
        let prefix = node.get("prefix").and_then(Value::as_str).unwrap_or("");
        let name = node.get("name").and_then(Value::as_str).unwrap_or("Unnamed");
        let kind = node.get("type").and_then(Value::as_str).unwrap_or("unknown");
        rows.push(Row::new(
            format!("[{prefix}] {name} ({kind})"),
            Step::Open(MenuId::CustomNodeDetail(ProviderSlot(i))),
        ));
    }
    rows
}

/// Custom provider node detail rows (`showCustomNodeDetail`).
pub fn custom_node_action_rows(node: &Value) -> Vec<Row> {
    let id = node.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    vec![
        Row::new(
            "Connections",
            Step::Act(Action::AddNodeConnection(0)),
        ),
        Row::new("Edit Node", Step::Act(Action::EditNode(id.clone()))),
        Row::new("Delete Node", Step::Act(Action::DeleteNode(id))),
    ]
}

/// CLI tools menu rows.
pub fn cli_tool_rows() -> Vec<Row> {
    vec![
        Row::new("Claude Code", Step::Open(MenuId::ClaudeCode)),
        Row::new("Codex CLI", Step::Open(MenuId::Codex)),
        Row::new("Factory Droid", Step::Open(MenuId::Droid)),
        Row::new("Open Claw", Step::Open(MenuId::OpenClaw)),
        Row::new("OpenCode", Step::Open(MenuId::OpenCode)),
        Row::new("Hermes", Step::Open(MenuId::Hermes)),
    ]
}

/// Claude Code rows: quick setup then the three model slots and reset.
/// `current` holds the three model values read from the tool's settings.
pub fn claude_rows(current: &[String; 3]) -> Vec<Row> {
    vec![
        Row::new("⚡ Quick Setup (recommended)", Step::Act(Action::CliToolQuickSetup("claude"))),
        Row::new(
            format!("Sonnet → {}", current[0]),
            Step::Act(Action::CliToolSelectModel("claude", "ANTHROPIC_DEFAULT_SONNET_MODEL")),
        ),
        Row::new(
            format!("Opus → {}", current[1]),
            Step::Act(Action::CliToolSelectModel("claude", "ANTHROPIC_DEFAULT_OPUS_MODEL")),
        ),
        Row::new(
            format!("Haiku → {}", current[2]),
            Step::Act(Action::CliToolSelectModel("claude", "ANTHROPIC_DEFAULT_HAIKU_MODEL")),
        ),
        Row::new("Reset to Default", Step::Act(Action::CliToolReset("claude"))),
    ]
}

/// Simple quick-setup / reset rows shared by Codex, Droid, OpenClaw, OpenCode.
pub fn simple_cli_tool_rows(tool: &'static str) -> Vec<Row> {
    vec![
        Row::new("⚡ Quick Setup", Step::Act(Action::CliToolQuickSetup(tool))),
        Row::new("Reset to Default", Step::Act(Action::CliToolReset(tool))),
    ]
}

/// Hermes rows (`showHermesMenu`), including the active-profile label.
pub fn hermes_rows(active_profile: &str) -> Vec<Row> {
    vec![
        Row::new(
            format!("🎯 Profile: {active_profile}"),
            Step::Act(Action::CliToolSelectModel("hermes", "profile")),
        ),
        Row::new("⚡ Quick Setup", Step::Act(Action::CliToolQuickSetup("hermes"))),
        Row::new("🔁 Apply to All Profiles", Step::Act(Action::CliToolQuickSetup("hermes-all"))),
        Row::new(
            format!("↺ Reset Profile: {active_profile}"),
            Step::Act(Action::CliToolReset("hermes")),
        ),
    ]
}

/// Settings rows, with the RTK / headroom / auth-mode labels reflecting state.
pub fn settings_rows(rtk_enabled: bool, headroom_enabled: bool, auth_mode: &str) -> Vec<Row> {
    vec![
        Row::new("Tunnel ON", Step::Act(Action::TunnelOn)),
        Row::new("Tunnel OFF", Step::Act(Action::TunnelOff)),
        Row::new(
            format!("Token Saver (RTK): {} → toggle", if rtk_enabled { "ON" } else { "OFF" }),
            Step::Act(Action::ToggleRtk),
        ),
        Row::new(
            format!(
                "Token Saver (Headroom): {} → toggle",
                if headroom_enabled { "ON" } else { "OFF" }
            ),
            Step::Act(Action::ToggleHeadroom),
        ),
        Row::new("🔑 Reset Password to Default", Step::Act(Action::ResetPassword)),
        Row::new(
            if auth_mode == "password" {
                "🔓 Reset Auth Mode (already password)".to_string()
            } else {
                format!("🔓 Reset Auth Mode to Password (current: {auth_mode})")
            },
            Step::Act(Action::ResetAuthMode),
        ),
    ]
}

/// Usage period choices.
pub const USAGE_PERIODS: &[&str] = &["today", "24h", "7d", "30d"];

/// Usage menu rows for the selected period.
pub fn usage_rows(period: &str) -> Vec<Row> {
    let mut rows = vec![Row::new(
        format!("Period: {period}"),
        Step::Act(Action::SetUsagePeriod("next")),
    )];
    for p in USAGE_PERIODS {
        if *p != period {
            rows.push(Row::new(format!("Show {p}"), Step::Act(Action::SetUsagePeriod(p))));
        }
    }
    rows
}

/// Settings header (tunnel / RTK / headroom / auth mode).
pub fn settings_header(tunnel: &Value, settings: &Value) -> String {
    let mut lines = Vec::new();
    let enabled = tunnel.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let public_url = tunnel.get("publicUrl").and_then(Value::as_str).unwrap_or("");
    if enabled && !public_url.is_empty() {
        lines.push(format!("  Endpoint: {public_url}/v1"));
        lines.push(format!(
            "  Tunnel:   ON ({})",
            tunnel.get("shortId").and_then(Value::as_str).unwrap_or("")
        ));
    } else {
        lines.push("  Endpoint: http://localhost:20128/v1".to_string());
        lines.push("  Tunnel:   OFF (local only)".to_string());
    }
    let rtk = settings.get("rtkEnabled").and_then(Value::as_bool).unwrap_or(true);
    lines.push(format!("  RTK:      {}", if rtk { "ON" } else { "OFF" }));
    let headroom = settings.get("headroomEnabled").and_then(Value::as_bool).unwrap_or(false);
    let headroom_url = settings
        .get("headroomUrl")
        .and_then(Value::as_str)
        .unwrap_or("http://localhost:8787");
    lines.push(format!(
        "  Headroom: {} ({headroom_url})",
        if headroom { "ON" } else { "OFF" }
    ));
    let mode = settings.get("authMode").and_then(Value::as_str).unwrap_or("password");
    lines.push(format!("  Auth:     {} (login mode)", mode.to_ascii_uppercase()));
    lines.join("\n")
}

/// Usage summary block from `/api/usage/stats`.
pub fn usage_summary(stats: &Value, period: &str) -> String {
    let n = |key: &str| stats.get(key).and_then(Value::as_i64).unwrap_or(0);
    let cost = stats.get("totalCost").and_then(Value::as_f64).unwrap_or(0.0);
    let mut lines = vec![
        format!("Period:    {period}"),
        format!("Requests:  {}", n("totalRequests")),
        format!("Tokens:    {} in / {} out", n("totalPromptTokens"), n("totalCompletionTokens")),
        format!("Cached:    {}", n("totalCachedTokens")),
        format!("Cost:      ${cost:.4}"),
    ];
    if let Some(by) = stats.get("byProvider").and_then(Value::as_object) {
        if !by.is_empty() {
            lines.push("By provider:".to_string());
            let mut entries: Vec<(&String, &Value)> = by.iter().collect();
            entries.sort_by(|a, b| {
                let ra = a.1.get("requests").and_then(Value::as_i64).unwrap_or(0);
                let rb = b.1.get("requests").and_then(Value::as_i64).unwrap_or(0);
                rb.cmp(&ra).then_with(|| a.0.cmp(b.0))
            });
            for (provider, stats) in entries {
                let requests = stats.get("requests").and_then(Value::as_i64).unwrap_or(0);
                let cost = stats.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                lines.push(format!("  {provider:<16} {requests:>6} req  ${cost:.4}"));
            }
        }
    }
    lines.join("\n")
}

/// One log row, `time model status duration`.
pub fn log_row(log: &Value) -> String {
    let ts = log.get("timestamp").cloned().unwrap_or(Value::Null);
    let when = format::format_date(&ts);
    let model = log.get("model").and_then(Value::as_str).unwrap_or("-");
    let status = log
        .get("status")
        .map(|s| match s {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "-".to_string());
    let duration = log.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
    let error = log.get("error").and_then(Value::as_str).unwrap_or("");
    let suffix = if error.is_empty() {
        String::new()
    } else {
        format!("  {error}")
    };
    format!("{when}  {model}  {status}  {duration}ms{suffix}")
}

/// Log menu rows: one row per request, capped so a long list stays usable.
pub fn log_rows(logs: &[Value]) -> Vec<Row> {
    logs.iter()
        .take(50)
        .map(|log| Row::new(log_row(log), Step::Act(Action::Nothing)))
        .collect()
}

// ── navigation ──────────────────────────────────────────────────────────────

/// A navigation stack. `open` descends, `back` pops, and popping past the root
/// reports "leave the TUI" so the caller can exit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Nav {
    stack: Vec<MenuId>,
}

impl Nav {
    pub fn new() -> Self {
        Self { stack: vec![MenuId::Main] }
    }

    pub fn current(&self) -> MenuId {
        *self.stack.last().unwrap_or(&MenuId::Main)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Descend into `id` when it is a child of the current menu (or a detail of
    /// the current list). Returns false for an unrelated menu.
    pub fn open(&mut self, id: MenuId) -> bool {
        match id.parent() {
            Some(parent) if parent == self.current() => {
                self.stack.push(id);
                true
            }
            // Detail menus are children of the list they came from; they carry
            // no children of their own, so nothing else needs special casing.
            _ => false,
        }
    }

    /// Pop one level. Returns false when already at the root (the caller then
    /// exits the terminal UI, matching `← Back to Interface Menu`).
    pub fn back(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.stack.pop();
        true
    }

    pub fn breadcrumb(&self) -> String {
        self.stack.iter().map(|m| m.crumb()).collect::<Vec<_>>().join(" > ")
    }

    pub fn breadcrumb_parts(&self) -> Vec<&'static str> {
        self.stack.iter().map(|m| m.crumb()).collect()
    }
}

/// Rows plus the implicit `Back` row every menu carries at the top
/// (`showMenuWithBack` prepends `backLabel`).
pub fn with_back_row(rows: Vec<Row>, back_label: &str) -> Vec<Row> {
    let mut out = Vec::with_capacity(rows.len() + 1);
    out.push(Row::new(back_label, Step::Back));
    out.extend(rows);
    out
}

/// Default `← Back` label, or the root's exit label.
pub fn back_label(nav: &Nav) -> &'static str {
    if nav.depth() <= 1 {
        "← Back to Interface Menu"
    } else {
        "← Back"
    }
}

/// Apply a chosen row's step. Returns `Some(true)` to keep the TUI running,
/// `Some(false)` to leave it, or `None` when the row needs no navigation
/// change (a leaf action the caller already performed).
pub fn apply_step(nav: &mut Nav, step: &Step) -> Option<bool> {
    match step {
        Step::Open(id) => {
            nav.open(*id);
            Some(true)
        }
        Step::Back => Some(nav.back()),
        Step::Exit => Some(false),
        Step::Act(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn main_menu_has_the_js_entries_plus_usage_and_logs() {
        let labels: Vec<String> = main_rows().into_iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            vec![
                "Providers", "API Keys", "Combos", "CLI Tools", "Settings", "Usage", "Logs"
            ]
        );
    }

    #[test]
    fn provider_rows_are_oauth_then_apikey_then_custom() {
        let conns = vec![
            json!({"provider": "claude"}),
            json!({"provider": "claude"}),
            json!({"provider": "openrouter"}),
        ];
        let rows = provider_rows(&conns, 3);
        // 9 OAuth + 7 API key + separator + custom entry
        assert_eq!(rows.len(), 9 + 7 + 2);
        assert_eq!(rows[0].label, "Claude Code (OAuth) - 2 Connected");
        assert!(rows[9].label.starts_with("OpenRouter (API) - 1 Connected"), "{}", rows[9].label);
        assert!(rows[16].separator);
        assert_eq!(rows[17].label, "Custom Providers - 3 Configured");
        assert_eq!(rows[17].step, Step::Open(MenuId::CustomProviders));
        // Every provider row opens a detail menu for the slot it was rendered at.
        assert_eq!(rows[1].step, Step::Open(MenuId::ProviderDetail(ProviderSlot(1))));
    }

    #[test]
    fn provider_slots_resolve_back_to_the_same_entry() {
        for (slot, entry) in OAUTH_PROVIDERS.iter().chain(APIKEY_PROVIDERS.iter()).enumerate() {
            assert_eq!(provider_for_slot(ProviderSlot(slot)), Some(entry));
        }
        assert_eq!(provider_for_slot(ProviderSlot(999)), None);
        assert!(provider_by_id("glm").unwrap().oauth, "OAuth table wins for the shared id");
        assert!(provider_by_id("qwen").is_some());
        assert!(provider_by_id("nope").is_none());
    }

    #[test]
    fn provider_header_shows_alias_and_truncated_models() {
        let cc = provider_by_id("cc").unwrap();
        let header = provider_header(cc);
        assert!(header.contains("Alias: cc"), "{header}");
        assert!(header.contains("cc/claude-opus-4-5-20251101"), "{header}");
        assert!(!header.contains("more"), "three models, no overflow: {header}");

        let ag = provider_by_id("ag").unwrap();
        let header = provider_header(ag);
        assert!(header.contains("(+21 more)"), "{header}");

        let unknown = ProviderEntry { id: "x", alias: "zz", name: "X", oauth: false };
        assert!(provider_header(&unknown).contains("No models configured"));
    }

    #[test]
    fn connection_rows_start_with_add_and_skip_other_providers() {
        let conns = vec![
            json!({"id": "c1", "provider": "claude", "name": "Work", "testStatus": "active"}),
            json!({"id": "c2", "provider": "codex", "name": "Other"}),
        ];
        let rows = connection_rows(&conns, "claude");
        assert_eq!(rows.len(), 2, "add + one matching connection");
        assert_eq!(rows[0].label, "Add New Connection");
        assert_eq!(rows[0].step, Step::Act(Action::AddConnection(0)));
        assert_eq!(rows[1].label, "Work (✓)");
        assert_eq!(rows[1].step, Step::Act(Action::ConnectionActions(ProviderSlot(0))));
    }

    #[test]
    fn connection_action_rows_carry_the_id() {
        let conn = json!({"id": "c1", "name": "W", "testStatus": "active"});
        let rows = connection_action_rows(&conn);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Connection: W",
                "Status: ✓ Active",
                "Rename Connection",
                "Test Connection",
                "Delete Connection",
            ]
        );
        assert_eq!(rows[3].step, Step::Act(Action::TestConnection("c1".into())));
        assert_eq!(rows[4].step, Step::Act(Action::DeleteConnection("c1".into())));
        assert_eq!(
            connection_action_rows(&json!({"id": "c2"}))[1].label,
            "Status: ? Unknown"
        );
    }

    #[test]
    fn api_key_rows_use_masked_labels() {
        let keys = vec![json!({"id": "k1", "name": "laptop", "key": "sk-1234567890abcd"})];
        let rows = api_key_rows(&keys);
        assert_eq!(rows[0].label, "Create New API Key");
        assert_eq!(rows[1].label, "laptop (sk-1*********abcd)");
        assert_eq!(rows[1].step, Step::Act(Action::ViewApiKey("k1".into())));
        // The label must never leak the full key.
        assert!(!rows[1].label.contains("1234567890"));
    }

    #[test]
    fn combo_rows_and_actions_carry_ids() {
        let combos = vec![json!({"id": "cb1", "name": "fast", "models": ["a/x", "b/y"]})];
        let rows = combo_rows(&combos);
        assert_eq!(rows[0].label, "Create New Combo");
        assert_eq!(rows[1].label, "fast: a/x → b/y");

        let actions = combo_action_rows(&combos[0]);
        assert_eq!(actions[0].step, Step::Act(Action::EditCombo("cb1".into())));
        assert_eq!(actions[1].step, Step::Act(Action::DeleteCombo("cb1".into())));
    }

    #[test]
    fn cli_tool_and_settings_rows_reflect_state() {
        let rows = cli_tool_rows();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].step, Step::Open(MenuId::ClaudeCode));

        let claude = claude_rows(&["Not set".into(), "cx/x".into(), "cc/h".into()]);
        assert!(claude[1].label.ends_with("Not set"), "{}", claude[1].label);
        assert_eq!(
            claude[2].step,
            Step::Act(Action::CliToolSelectModel("claude", "ANTHROPIC_DEFAULT_OPUS_MODEL"))
        );

        let settings = settings_rows(true, false, "oidc");
        assert_eq!(settings[2].label, "Token Saver (RTK): ON → toggle");
        assert_eq!(settings[3].label, "Token Saver (Headroom): OFF → toggle");
        assert!(settings[5].label.contains("current: oidc"), "{}", settings[5].label);
        let plain = settings_rows(true, false, "password");
        assert_eq!(plain[5].label, "🔓 Reset Auth Mode (already password)");
    }

    #[test]
    fn usage_and_logs_render_from_api_payloads() {
        let stats = json!({
            "totalRequests": 42,
            "totalPromptTokens": 1000,
            "totalCompletionTokens": 200,
            "totalCachedTokens": 50,
            "totalCost": 0.1234,
            "byProvider": {"claude": {"requests": 30, "cost": 0.1}, "codex": {"requests": 12, "cost": 0.02}}
        });
        let text = usage_summary(&stats, "7d");
        assert!(text.contains("Period:    7d"), "{text}");
        assert!(text.contains("Requests:  42"), "{text}");
        assert!(text.contains("1000 in / 200 out"), "{text}");
        assert!(text.contains("$0.1234"), "{text}");
        // Sorted by request count, claude first.
        let claude_at = text.find("claude").unwrap();
        let codex_at = text.find("codex").unwrap();
        assert!(claude_at < codex_at, "{text}");

        let row = log_row(&json!({
            "timestamp": "2024-05-01T10:20:30Z",
            "model": "cc/opus",
            "status": 200,
            "durationMs": 1234
        }));
        assert!(row.contains("cc/opus"), "{row}");
        assert!(row.contains("200"), "{row}");
        assert!(row.contains("1234ms"), "{row}");

        let rows = log_rows(&[json!({"timestamp": "2024-05-01T10:20:30Z", "model": "m", "status": 500, "error": "boom"})]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].label.contains("boom"));

        let periods = usage_rows("7d");
        assert_eq!(periods[0].label, "Period: 7d");
        assert_eq!(periods[1].label, "Show today");
    }

    // ── navigation ──────────────────────────────────────────────────────────

    #[test]
    fn nav_starts_at_main_and_exits_from_the_root() {
        let mut nav = Nav::new();
        assert_eq!(nav.current(), MenuId::Main);
        assert_eq!(nav.depth(), 1);
        assert!(!nav.back(), "back at the root means exit the TUI");
        assert_eq!(nav.current(), MenuId::Main);
        assert_eq!(back_label(&nav), "← Back to Interface Menu");
    }

    #[test]
    fn nav_walks_main_providers_detail_and_back() {
        let mut nav = Nav::new();
        assert!(nav.open(MenuId::Providers));
        assert_eq!(nav.current(), MenuId::Providers);
        assert_eq!(nav.breadcrumb(), "9Router > Providers");
        assert_eq!(nav.breadcrumb_parts(), vec!["9Router", "Providers"]);
        assert_eq!(back_label(&nav), "← Back");

        let slot = ProviderSlot(0);
        assert!(nav.open(MenuId::ProviderDetail(slot)));
        assert_eq!(nav.current(), MenuId::ProviderDetail(slot));
        assert_eq!(nav.breadcrumb(), "9Router > Providers > Connection");

        assert!(nav.back());
        assert_eq!(nav.current(), MenuId::Providers);
        assert!(nav.back());
        assert_eq!(nav.current(), MenuId::Main);
        assert!(!nav.back(), "second back at the root exits");
    }

    #[test]
    fn nav_rejects_skipping_levels_and_unrelated_menus() {
        let mut nav = Nav::new();
        // Main -> detail is not a valid edge; must go through Providers.
        assert!(!nav.open(MenuId::ProviderDetail(ProviderSlot(0))));
        assert_eq!(nav.current(), MenuId::Main);
        assert!(!nav.open(MenuId::ClaudeCode), "CLI tool submenu needs CliTools");
        assert_eq!(nav.current(), MenuId::Main);

        assert!(nav.open(MenuId::CliTools));
        assert!(!nav.open(MenuId::Providers), "siblings cannot be opened directly");
    }

    #[test]
    fn every_menu_is_reachable_from_the_root_by_following_parents() {
        let all = [
            MenuId::Main,
            MenuId::Providers,
            MenuId::ProviderDetail(ProviderSlot(0)),
            MenuId::CustomProviders,
            MenuId::CustomNodeDetail(ProviderSlot(0)),
            MenuId::ApiKeys,
            MenuId::Combos,
            MenuId::ComboDetail(ProviderSlot(0)),
            MenuId::CliTools,
            MenuId::ClaudeCode,
            MenuId::Codex,
            MenuId::Droid,
            MenuId::OpenClaw,
            MenuId::OpenCode,
            MenuId::Hermes,
            MenuId::Settings,
            MenuId::Usage,
            MenuId::Logs,
        ];
        for target in all {
            let mut chain = vec![target];
            let mut cursor = target;
            while let Some(parent) = cursor.parent() {
                chain.push(parent);
                cursor = parent;
            }
            chain.reverse();
            let mut nav = Nav::new();
            assert_eq!(chain[0], MenuId::Main, "{target:?} must root at Main");
            for step in &chain[1..] {
                assert!(nav.open(*step), "{target:?}: cannot open {step:?} from {:?}", nav.current());
            }
            assert_eq!(nav.current(), target);
            // And every one of them unwinds back to the root.
            while nav.back() {}
            assert_eq!(nav.current(), MenuId::Main);
        }
    }

    #[test]
    fn apply_step_drives_navigation_and_reports_exit() {
        let mut nav = Nav::new();
        assert_eq!(apply_step(&mut nav, &Step::Open(MenuId::Usage)), Some(true));
        assert_eq!(nav.current(), MenuId::Usage);
        assert_eq!(apply_step(&mut nav, &Step::Act(Action::Nothing)), None);
        assert_eq!(apply_step(&mut nav, &Step::Back), Some(true));
        assert_eq!(nav.current(), MenuId::Main);
        assert_eq!(apply_step(&mut nav, &Step::Back), Some(false), "root back exits");
        assert_eq!(apply_step(&mut nav, &Step::Exit), Some(false));
    }

    #[test]
    fn back_row_is_prepended_to_every_menu() {
        let rows = with_back_row(main_rows(), "← Back to Interface Menu");
        assert_eq!(rows.len(), main_rows().len() + 1);
        assert_eq!(rows[0].label, "← Back to Interface Menu");
        assert_eq!(rows[0].step, Step::Back);
    }

    #[test]
    fn every_menu_has_a_title_and_crumb() {
        use MenuId::*;
        for id in [
            Main, Providers, ProviderDetail(ProviderSlot(0)), CustomProviders,
            CustomNodeDetail(ProviderSlot(0)), ApiKeys, Combos, ComboDetail(ProviderSlot(0)),
            CliTools, ClaudeCode, Codex, Droid, OpenClaw, OpenCode, Hermes, Settings, Usage, Logs,
        ] {
            assert!(!id.title().is_empty());
            assert!(!id.crumb().is_empty());
        }
    }
}
