//! Interactive menu driver: renders the [`tree`] menus with `inquire` and
//! performs the leaf actions against the local gateway.
//!
//! Port of `cli/src/cli/terminalUI.js` + `cli/src/cli/menus/**`. Terminal
//! handling lives here only; navigation rules stay in [`super::tree`] so they
//! can be tested without a TTY.

use std::io::{self, BufRead, Write};
use std::thread::sleep;
use std::time::Duration;

use inquire::{Confirm, Password, Select};
use serde_json::{json, Value};

use super::api::{self, ApiClient, ApiResult};
use super::format;
use super::tree::{self, Action, MenuId, Nav, ProviderSlot, Row, Step};
use crate::ui::clear_screen;

/// Default dashboard password, quoted in the reset confirmation.
pub const DEFAULT_PASSWORD: &str = "123456";

/// Everything the TUI needs to talk to the gateway.
pub struct Terminal {
    pub api: ApiClient,
    pub port: u16,
    nav: Nav,
    /// Active Hermes profile (`hermesActiveProfile` in cliTools.js).
    hermes_profile: String,
    /// Selected usage period.
    usage_period: String,
}

impl Terminal {
    pub fn new(port: u16) -> Self {
        Self {
            api: ApiClient::new(port),
            port,
            nav: Nav::new(),
            hermes_profile: "default".to_string(),
            usage_period: "7d".to_string(),
        }
    }

    /// Run until the user backs out of the root menu.
    pub fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let menu = self.nav.current();
            let rows = self.rows_for(menu);
            let rows = tree::with_back_row(rows, tree::back_label(&self.nav));
            let header = self.header_for(menu);
            let breadcrumb = self.nav.breadcrumb();

            clear_screen().ok();
            if !header.is_empty() {
                println!("{header}");
            }

            let labels: Vec<String> = rows.iter().map(|r| r.label.clone()).collect();
            let prompt = format!("{}  {}", menu.title(), format!("({breadcrumb})"));
            let choice = match Select::new(&prompt, labels.clone())
                .with_page_size(20)
                .with_help_message("↑↓ move · Enter select · Esc back")
                .prompt()
            {
                Ok(selected) => labels.iter().position(|l| *l == selected),
                // Esc / Ctrl-C behaves like `← Back`.
                Err(_) => Some(0),
            };

            let Some(index) = choice else { continue };
            let Some(row) = rows.get(index) else { continue };

            match &row.step {
                Step::Act(action) => {
                    // Leaving the terminal UI on its own action would trap the
                    // user in a menu they cannot exit; actions never navigate.
                    if let Some(exit) = self.perform(action.clone()) {
                        if !exit {
                            return Ok(());
                        }
                    }
                }
                step => {
                    if let Some(keep_going) = tree::apply_step(&mut self.nav, step) {
                        if !keep_going {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Execute a leaf action. Returns `Some(false)` when the TUI should exit
    /// (never used today), `Some(true)` to stay, `None` for plain actions.
    fn perform(&mut self, action: Action) -> Option<bool> {
        match action {
            Action::Nothing => None,
            // `showConnectionActions`: one-level submenu, then run the chosen action.
            Action::ConnectionActions(slot) => {
                let conn = self.connections().into_iter().nth(slot.0)?;
                let rows = tree::connection_action_rows(&conn);
                let labels: Vec<String> = rows.iter().map(|r| r.label.clone()).collect();
                let picked = Select::new("Connection", labels.clone()).prompt().ok()?;
                let row = rows.get(labels.iter().position(|l| *l == picked)?)?;
                if let Step::Act(a) = &row.step {
                    return self.perform(a.clone());
                }
                None
            }
            Action::AddConnection(_) => {
                let Some(entry) = self.current_provider() else { return None };
                if entry.oauth {
                    self.add_oauth_connection(entry.id, entry.name);
                } else {
                    self.add_api_key_connection(entry.id, entry.name);
                }
                None
            }
            Action::RenameConnection(id) => {
                let Some(name) = prompt_line("New name: ") else { return None };
                if name.trim().is_empty() {
                    return None;
                }
                let res = self.api.update_connection(&id, &json!({ "name": name.trim() }));
                self.report(res, "Connection renamed!");
                None
            }
            Action::TestConnection(id) => {
                let res = self.api.test_provider(&id);
                match &res {
                    r if r.ok => status_ok("Connection is working!"),
                    _ => status_err(&format!("Test failed: {}", res.error_text())),
                }
                pause();
                None
            }
            Action::DeleteConnection(id) => {
                if !confirm("Delete this connection?", false) {
                    return None;
                }
                let res = self.api.delete_provider(&id);
                self.report(res, "Connection deleted!");
                None
            }
            Action::AddNode => {
                self.add_custom_node();
                None
            }
            Action::AddNodeConnection(_) => {
                if let Some(node) = self.current_node() {
                    let id = node.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = node.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    if !id.is_empty() {
                        self.add_api_key_connection(&id, &name);
                    }
                }
                None
            }
            Action::EditNode(id) => {
                self.edit_custom_node(&id);
                None
            }
            Action::DeleteNode(id) => {
                if !confirm("Delete this provider and all its connections?", false) {
                    return None;
                }
                let res = self.api.delete_provider_node(&id);
                self.report(res, "Node deleted!");
                None
            }
            Action::CreateApiKey => {
                self.create_api_key();
                None
            }
            Action::ViewApiKey(id) => {
                let keys = self.api.get_api_keys();
                if let Some(key) = find_by_id(&keys.array("keys"), &id) {
                    self.show_api_key(&key);
                }
                None
            }
            Action::CopyApiKey(id) => {
                let keys = self.api.get_api_keys();
                if let Some(key) = find_by_id(&keys.array("keys"), &id) {
                    let value = key.get("key").and_then(Value::as_str).unwrap_or("");
                    if copy_to_clipboard(value) {
                        status_ok(&format!(
                            "Key \"{}\" copied to clipboard!",
                            key.get("name").and_then(Value::as_str).unwrap_or("")
                        ));
                    } else {
                        status_err("Failed to copy to clipboard");
                    }
                    pause();
                }
                None
            }
            Action::DeleteApiKey(id) => {
                if !confirm("Are you sure you want to delete this key?", false) {
                    return None;
                }
                let res = self.api.delete_api_key(&id);
                self.report(res, "API key deleted successfully");
                None
            }
            Action::CreateCombo => {
                self.create_combo();
                None
            }
            Action::EditCombo(id) => {
                self.edit_combo(&id);
                None
            }
            Action::DeleteCombo(id) => {
                let combos = self.api.get_combos();
                let name = find_by_id(&combos.array("combos"), &id)
                    .and_then(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| id.clone());
                print_chain(&combos.array("combos"), &id);
                if !confirm(&format!("Delete combo \"{name}\"?"), false) {
                    return None;
                }
                let res = self.api.delete_combo(&id);
                self.report(res, "Combo deleted!");
                None
            }
            Action::CliToolQuickSetup(tool) => {
                self.cli_tool_quick_setup(tool);
                None
            }
            Action::CliToolReset(tool) => {
                let profile = if tool == "hermes" { Some(self.hermes_profile.clone()) } else { None };
                let res = self.api.reset_cli_tool_settings(tool, profile.as_deref());
                self.report(res, "Settings reset successfully!");
                None
            }
            Action::CliToolSelectModel(tool, env_key) => {
                if env_key == "profile" {
                    self.select_hermes_profile();
                } else {
                    self.cli_tool_select_model(tool, env_key);
                }
                None
            }
            Action::TunnelOn => {
                status_info("Creating tunnel...");
                let res = self.api.enable_tunnel();
                if res.ok {
                    let public_url = res.get("publicUrl").as_str().unwrap_or("");
                    let short_id = res.get("shortId").as_str().unwrap_or("");
                    let already = res.get("alreadyRunning").as_bool().unwrap_or(false);
                    if already {
                        status_ok(&format!("Tunnel already running: {public_url}"));
                    } else {
                        status_ok(&format!("Tunnel enabled: {public_url} ({short_id})"));
                    }
                } else {
                    status_err(&format!("Failed: {}", res.error_text()));
                }
                pause();
                None
            }
            Action::TunnelOff => {
                let res = self.api.disable_tunnel();
                self.report(res, "Tunnel disabled");
                None
            }
            Action::ToggleRtk => {
                let settings = self.api.get_settings();
                let on = settings.get("rtkEnabled").as_bool().unwrap_or(true);
                let res = self.api.update_settings(&json!({ "rtkEnabled": !on }));
                self.report(res, if on { "Token Saver disabled" } else { "Token Saver enabled" });
                None
            }
            Action::ToggleHeadroom => {
                let settings = self.api.get_settings();
                let on = settings.get("headroomEnabled").as_bool().unwrap_or(false);
                let res = self.api.update_settings(&json!({ "headroomEnabled": !on }));
                self.report(res, if on { "Headroom disabled" } else { "Headroom enabled" });
                None
            }
            Action::ResetPassword => {
                if !confirm(&format!("Reset dashboard password to default \"{DEFAULT_PASSWORD}\"?"), false) {
                    return None;
                }
                let res = self.api.reset_password();
                self.report(res, &format!("Password reset. Default: {DEFAULT_PASSWORD}"));
                None
            }
            Action::ResetAuthMode => {
                if !confirm("Reset auth mode to PASSWORD (disable OIDC)?", false) {
                    return None;
                }
                let res = self.api.update_settings(&json!({ "authMode": "password" }));
                self.report(res, "Auth mode reset to password. OIDC disabled.");
                None
            }
            Action::SetUsagePeriod(period) => {
                if period != "next" {
                    self.usage_period = period.to_string();
                }
                None
            }
        }
    }

    // ── data lookups ────────────────────────────────────────────────────────

    fn current_provider(&self) -> Option<&'static tree::ProviderEntry> {
        match self.nav.current() {
            MenuId::ProviderDetail(slot) => tree::provider_for_slot(slot),
            _ => None,
        }
    }

    fn current_node(&self) -> Option<Value> {
        let MenuId::CustomNodeDetail(ProviderSlot(index)) = self.nav.current() else {
            return None;
        };
        self.api.get_provider_nodes().array("nodes").into_iter().nth(index)
    }

    fn connections(&self) -> Vec<Value> {
        self.api.get_providers().array("connections")
    }

    // ── rows per menu ───────────────────────────────────────────────────────

    fn rows_for(&mut self, menu: MenuId) -> Vec<Row> {
        match menu {
            MenuId::Main => tree::main_rows(),
            MenuId::Providers => {
                let connections = self.connections();
                let nodes = self.api.get_provider_nodes();
                let node_count = nodes.array("nodes").len();
                tree::provider_rows(&connections, node_count)
            }
            MenuId::ProviderDetail(slot) => {
                let Some(entry) = tree::provider_for_slot(slot) else { return vec![] };
                tree::connection_rows(&self.connections(), entry.id)
            }
            MenuId::CustomProviders => tree::custom_node_rows(&self.api.get_provider_nodes().array("nodes")),
            MenuId::CustomNodeDetail(slot) => {
                let nodes = self.api.get_provider_nodes().array("nodes");
                match nodes.get(slot.0) {
                    Some(node) => tree::custom_node_action_rows(node),
                    None => vec![],
                }
            }
            MenuId::ApiKeys => tree::api_key_rows(&self.api.get_api_keys().array("keys")),
            MenuId::Combos => tree::combo_rows(&self.api.get_combos().array("combos")),
            MenuId::ComboDetail(slot) => {
                let combos = self.api.get_combos().array("combos");
                match combos.get(slot.0) {
                    Some(combo) => tree::combo_action_rows(combo),
                    None => vec![],
                }
            }
            MenuId::CliTools => tree::cli_tool_rows(),
            MenuId::ClaudeCode => {
                let settings = self.api.get_cli_tool_settings("claude", None);
                let env = &settings.get("settings")["env"];
                let get = |key: &str| {
                    env[key]
                        .as_str()
                        .unwrap_or("Not set")
                        .to_string()
                };
                tree::claude_rows(&[
                    get("ANTHROPIC_DEFAULT_SONNET_MODEL"),
                    get("ANTHROPIC_DEFAULT_OPUS_MODEL"),
                    get("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
                ])
            }
            MenuId::Codex => tree::simple_cli_tool_rows("codex"),
            MenuId::Droid => tree::simple_cli_tool_rows("droid"),
            MenuId::OpenClaw => tree::simple_cli_tool_rows("openclaw"),
            MenuId::OpenCode => tree::simple_cli_tool_rows("opencode"),
            MenuId::Hermes => {
                self.refresh_hermes_profile();
                tree::hermes_rows(&self.hermes_profile)
            }
            MenuId::Settings => {
                let settings = self.api.get_settings();
                let tunnel = self.api.get_tunnel_status();
                let rtk = settings.get("rtkEnabled").as_bool().unwrap_or(true);
                let headroom = settings.get("headroomEnabled").as_bool().unwrap_or(false);
                let mode = settings.get("authMode").as_str().unwrap_or("password");
                tree::settings_rows(rtk, headroom, mode)
            }
            MenuId::Usage => tree::usage_rows(&self.usage_period),
            MenuId::Logs => tree::log_rows(&self.api.get_usage_logs().as_array()),
        }
    }

    fn header_for(&mut self, menu: MenuId) -> String {
        match menu {
            MenuId::Main => {
                let keys = self.api.get_api_keys();
                let keys = keys.array("keys");
                let (endpoint, tunnel_on) = api::endpoint(&self.api, self.port);
                let mut lines = vec![format!("Endpoint: {endpoint}")];
                lines.push(if tunnel_on {
                    "Tunnel:   ON".to_string()
                } else {
                    "Tunnel:   OFF (local only)".to_string()
                });
                if keys.is_empty() {
                    lines.push("Key:      No API keys yet".to_string());
                } else {
                    for (i, key) in keys.iter().enumerate() {
                        let label = if i == 0 { "Key:      " } else { "          " };
                        lines.push(format!(
                            "{label}{}",
                            key.get("key").and_then(Value::as_str).unwrap_or("")
                        ));
                    }
                }
                lines.join("\n")
            }
            MenuId::ProviderDetail(slot) => tree::provider_for_slot(slot)
                .map(tree::provider_header)
                .unwrap_or_default(),
            MenuId::CustomNodeDetail(slot) => {
                let nodes = self.api.get_provider_nodes().array("nodes");
                nodes
                    .get(slot.0)
                    .map(|n| {
                        format!(
                            "Type: {}\nPrefix: {}\nBase URL: {}",
                            n.get("type").and_then(Value::as_str).unwrap_or("-"),
                            n.get("prefix").and_then(Value::as_str).unwrap_or("-"),
                            n.get("baseUrl").and_then(Value::as_str).unwrap_or("-"),
                        )
                    })
                    .unwrap_or_default()
            }
            MenuId::CliTools => {
                let (endpoint, _) = api::endpoint(&self.api, self.port);
                format!("Configure CLI tools to use 9Router\nEndpoint: {endpoint}")
            }
            MenuId::Settings => {
                let settings = self.api.get_settings();
                let status = self.api.get_tunnel_status();
                let tunnel = status.get("tunnel").clone();
                tree::settings_header(&tunnel, &settings.data)
            }
            MenuId::Usage => {
                let stats = self.api.get_usage_stats(&self.usage_period);
                if stats.ok {
                    tree::usage_summary(&stats.data, &self.usage_period)
                } else {
                    format!("Failed to load usage: {}", stats.error_text())
                }
            }
            _ => String::new(),
        }
    }

    // ── leaf implementations ────────────────────────────────────────────────

    fn report(&mut self, res: ApiResult, success_message: &str) {
        if res.ok {
            status_ok(success_message);
        } else {
            status_err(&format!("Failed: {}", res.error_text()));
        }
        pause();
    }

    fn show_api_key(&self, key: &Value) {
        print!("{}", api_key_detail(key));
        pause();
    }

    fn create_api_key(&mut self) {
        let Some(name) = prompt_line("Enter key name: ") else { return };
        if name.is_empty() {
            status_err("Key name cannot be empty");
            pause();
            return;
        }
        let res = self.api.create_api_key(&name);
        if !res.ok {
            status_err(&format!("Failed to create key: {}", res.error_text()));
            pause();
            return;
        }
        println!("\n✅ API Key created successfully!");
        println!("\n⚠️  IMPORTANT: Save this key now. You won't be able to see it again!");
        println!("\nKey: {}", res.get("key").as_str().unwrap_or(""));
        println!("Name: {}", res.get("name").as_str().unwrap_or(""));
        println!("ID: {}", res.get("id").as_str().unwrap_or(""));
        if confirm("\nCopy key to clipboard?", true) {
            if copy_to_clipboard(res.get("key").as_str().unwrap_or("")) {
                status_ok("Key copied to clipboard!");
            } else {
                status_err("Failed to copy to clipboard");
            }
        }
        pause();
    }

    fn add_api_key_connection(&mut self, provider_id: &str, provider_name: &str) {
        clear_screen().ok();
        println!("\n➕ Add {provider_name} API Key Connection\n");
        let Some(name) = prompt_line("Connection Name: ") else {
            status_warn("Cancelled");
            pause();
            return;
        };
        if name.is_empty() {
            status_warn("Cancelled");
            pause();
            return;
        }
        let Some(api_key) = prompt_line("API Key: ") else {
            status_warn("Cancelled");
            pause();
            return;
        };
        if api_key.is_empty() {
            status_warn("Cancelled");
            pause();
            return;
        }
        status_info("Creating connection...");
        let res = self.api.create_api_key_provider(&json!({
            "provider": provider_id,
            "name": name,
            "apiKey": api_key,
        }));
        if res.ok {
            status_ok("✓ Connection created successfully!");
        } else {
            status_err(&format!("✗ Failed: {}", res.error_text()));
        }
        pause();
    }

    fn add_oauth_connection(&mut self, provider_id: &str, provider_name: &str) {
        clear_screen().ok();
        if tree::DEVICE_CODE_PROVIDERS.contains(&provider_id) {
            self.add_device_code_connection(provider_id, provider_name);
            return;
        }
        status_info("Requesting authorization URL...");
        let auth = self.api.get_oauth_auth_url(provider_id);
        if !auth.ok {
            status_err(&format!("Failed: {}", auth.error_text()));
            pause();
            return;
        }
        let auth_url = auth.get("authUrl").as_str().unwrap_or("");
        let code_verifier = auth.get("codeVerifier").as_str().unwrap_or("").to_string();
        let state = auth.get("state").as_str().unwrap_or("").to_string();
        let redirect_uri = auth.get("redirectUri").as_str().unwrap_or("").to_string();
        if auth_url.is_empty() {
            status_err("Failed: No auth URL received");
            pause();
            return;
        }

        clear_screen().ok();
        println!("\n🔐 OAuth Login — Providers > {provider_name} > Add Connection\n");
        println!("  1. Open this URL in your browser:\n     {auth_url}");
        if copy_to_clipboard(auth_url) {
            println!("     ✓ Link copied to clipboard!");
        }
        println!("\n  2. Complete authorization in browser");
        println!("\n  3. Copy the callback URL from the address bar");
        println!("     (looks like: http://localhost:20128/callback?code=...)\n");
        let Some(callback) = prompt_line("  Paste callback URL: ") else {
            status_warn("Cancelled");
            pause();
            return;
        };
        if callback.is_empty() {
            status_warn("Cancelled");
            pause();
            return;
        }
        let Some((code, url_state)) = parse_callback(&callback) else {
            status_err("No authorization code found in URL");
            pause();
            return;
        };
        println!();
        status_info("Exchanging code for tokens...");
        let res = self.api.exchange_oauth_code(
            provider_id,
            &json!({
                "code": code,
                "redirectUri": redirect_uri,
                "codeVerifier": code_verifier,
                "state": if url_state.is_empty() { state } else { url_state },
            }),
        );
        if res.ok {
            status_ok("Connection created successfully!");
        } else {
            status_err(&format!("Failed: {}", res.error_text()));
        }
        pause();
    }

    fn add_device_code_connection(&mut self, provider_id: &str, provider_name: &str) {
        status_info("Requesting device code...");
        let device = self.api.get_oauth_device_code(provider_id);
        if !device.ok {
            status_err(&format!("Failed: {}", device.error_text()));
            pause();
            return;
        }
        let device_code = device
            .get("deviceCode")
            .as_str()
            .or_else(|| device.get("device_code").as_str())
            .unwrap_or("")
            .to_string();
        let user_code = device
            .get("userCode")
            .as_str()
            .or_else(|| device.get("user_code").as_str())
            .unwrap_or("")
            .to_string();
        let verification_uri = device
            .get("verificationUri")
            .as_str()
            .or_else(|| device.get("verification_uri").as_str())
            .unwrap_or("")
            .to_string();
        let complete = device
            .get("verificationUriComplete")
            .as_str()
            .or_else(|| device.get("verification_uri_complete").as_str())
            .map(str::to_string);
        let code_verifier = device.get("codeVerifier").clone();
        if device_code.is_empty() {
            status_err("Failed: No device code received");
            pause();
            return;
        }

        clear_screen().ok();
        let url = complete.as_deref().unwrap_or(&verification_uri);
        println!("\n📱 Device Login — Providers > {provider_name} > Add Connection\n");
        println!("  1. Open: {url}");
        if copy_to_clipboard(url) {
            println!("     ✓ Link copied to clipboard!");
        }
        println!();
        if complete.is_none() && !user_code.is_empty() {
            println!("  2. Enter code: {user_code}");
            println!();
        }
        println!("  Waiting for authorization...");
        println!();

        // 60 attempts at 5s = 5 minutes, as in the JS.
        for _ in 0..60 {
            sleep(Duration::from_secs(5));
            let res = self.api.poll_oauth_token(
                provider_id,
                &json!({
                    "deviceCode": device_code,
                    "codeVerifier": code_verifier,
                    "extraData": device.data.clone(),
                }),
            );
            if res.ok {
                status_ok("\nConnection created successfully!");
                pause();
                return;
            }
            let pending = res.pending
                || res.error_text() == "authorization_pending"
                || res.error_text() == "slow_down";
            if !pending {
                status_err(&format!("\nFailed: {}", res.error_text()));
                pause();
                return;
            }
            print!(".");
            let _ = io::stdout().flush();
        }
        status_err("\nTimeout waiting for authorization");
        pause();
    }

    fn add_custom_node(&mut self) {
        clear_screen().ok();
        println!("\n➕ Add Custom Provider\n");
        let Some(kind) = Select::new(
            "Type",
            vec!["openai-compatible".to_string(), "anthropic-compatible".to_string()],
        )
        .prompt()
        .ok() else {
            status_warn("Cancelled");
            pause();
            return;
        };
        let Some(name) = prompt_line("Name: ") else { return };
        if name.is_empty() {
            status_warn("Cancelled");
            pause();
            return;
        }
        let Some(prefix) = prompt_line("Prefix (used in model IDs, e.g. myapi): ") else { return };
        if prefix.is_empty() {
            status_warn("Cancelled");
            pause();
            return;
        }
        let Some(base_url) = prompt_line("Base URL (e.g. https://api.example.com/v1): ") else { return };
        if base_url.is_empty() {
            status_warn("Cancelled");
            pause();
            return;
        }
        let mut body = json!({ "name": name, "prefix": prefix, "baseUrl": base_url, "type": kind });
        if kind == "openai-compatible" {
            let api_type = Select::new("API Type", vec!["chat".to_string(), "responses".to_string()])
                .prompt()
                .unwrap_or_else(|_| "chat".to_string());
            body["apiType"] = json!(api_type);
        }
        status_info("Creating provider node...");
        let res = self.api.create_provider_node(&body);
        if res.ok {
            status_ok("✓ Provider created!");
        } else {
            status_err(&format!("✗ Failed: {}", res.error_text()));
        }
        pause();
    }

    fn edit_custom_node(&mut self, id: &str) {
        clear_screen().ok();
        println!("\n✏️  Edit provider\n");
        println!("Leave blank to keep the current value\n");
        let mut updates = serde_json::Map::new();
        for (prompt, key) in [("Name: ", "name"), ("Base URL: ", "baseUrl"), ("Prefix: ", "prefix")] {
            if let Some(v) = prompt_line(prompt) {
                if !v.trim().is_empty() {
                    updates.insert(key.to_string(), json!(v.trim()));
                }
            }
        }
        if updates.is_empty() {
            status_warn("No changes");
            pause();
            return;
        }
        status_info("Updating...");
        let res = self.api.update_provider_node(id, &Value::Object(updates));
        if res.ok {
            status_ok("✓ Updated!");
        } else {
            status_err(&format!("✗ Failed: {}", res.error_text()));
        }
        pause();
    }

    fn create_combo(&mut self) {
        clear_screen().ok();
        status_info("Create New Combo");
        println!();
        let Some(name) = prompt_line("Combo name: ") else { return };
        if name.is_empty() {
            status_err("Combo name is required");
            pause();
            return;
        }
        let models = self.api.get_models();
        let available = models.array("models");
        if available.is_empty() {
            status_err("No models available. Please add providers first.");
            pause();
            return;
        }
        let mut selected: Vec<String> = Vec::new();
        loop {
            println!("\nCreating combo: {name}");
            println!("Selected models ({}):", selected.len());
            if selected.is_empty() {
                println!("  (none)");
            } else {
                for (i, m) in selected.iter().enumerate() {
                    println!("  {}. {m}", i + 1);
                }
            }
            println!("\nAvailable models:");
            for (i, m) in available.iter().enumerate() {
                let provider = m.get("provider").and_then(Value::as_str).unwrap_or("");
                let model = m.get("model").and_then(Value::as_str).unwrap_or("");
                println!("  {}. {provider}/{model}", i + 1);
            }
            println!("\nActions:");
            println!("  - Enter number to add model");
            println!("  - Type 'done' to finish (min 2 models)");
            println!("  - Type 'cancel' to abort");
            let Some(input) = prompt_line("\nAction: ") else { return };
            let lower = input.to_ascii_lowercase();
            if lower == "cancel" {
                status_warn("Cancelled");
                pause();
                return;
            }
            if lower == "done" {
                if selected.len() < 2 {
                    status_err("Please select at least 2 models");
                    pause();
                    continue;
                }
                break;
            }
            match input.parse::<usize>() {
                Ok(n) if n >= 1 && n <= available.len() => {
                    match format::combo_model_id(&available[n - 1]) {
                        Some(id) => selected.push(id),
                        None => {
                            status_err("Selected model has no valid identifier");
                            pause();
                        }
                    }
                }
                _ => {
                    status_err("Invalid model number");
                    pause();
                }
            }
        }
        status_info("Creating combo...");
        let res = self.api.create_combo(&json!({ "name": name, "models": selected }));
        self.report(res, &format!("Combo \"{name}\" created successfully!"));
    }

    fn edit_combo(&mut self, id: &str) {
        clear_screen().ok();
        let combos = self.api.get_combos();
        let Some(combo) = find_by_id(&combos.array("combos"), id) else {
            status_err("Combo not found");
            pause();
            return;
        };
        let current_name = combo.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let current_models = combo.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
        println!("\n✏️  Edit Combo: {current_name}\n");
        let new_name = prompt_line(&format!("New name (Enter to keep \"{current_name}\"): ")).unwrap_or_default();
        let name = if new_name.is_empty() { current_name.clone() } else { new_name };
        println!("\nCurrent models: {}", format::models_chain(&current_models));

        let mut models: Vec<String> = Vec::new();
        while confirm(&format!("Add model #{}?", models.len() + 1), models.is_empty()) {
            let Some(model) = prompt_line("Model id (e.g. cc/claude-opus-4-5-20251101): ") else { break };
            let model = model.trim().to_string();
            if model.is_empty() {
                break;
            }
            models.push(model);
        }
        let final_models = if models.is_empty() { current_models } else { models.iter().map(|m| json!(m)).collect() };

        let res = self.api.update_combo(id, &json!({ "name": name, "models": final_models }));
        self.report(res, "Combo updated!");
    }

    fn select_hermes_profile(&mut self) {
        let profiles = self.api.list_hermes_profiles().array("profiles");
        if profiles.is_empty() {
            status_err("No Hermes profiles found.");
            pause();
            return;
        }
        let labels: Vec<String> = profiles
            .iter()
            .map(|p| {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("default");
                let display = p.get("displayName").and_then(Value::as_str).unwrap_or("");
                let state = if p.get("has9Router").and_then(Value::as_bool).unwrap_or(false) {
                    "✓ 9router"
                } else if p.get("baseUrl").and_then(Value::as_str).is_some() {
                    "other base URL"
                } else {
                    "✗ not configured"
                };
                if display.is_empty() || p.get("isDefault").and_then(Value::as_bool).unwrap_or(false) {
                    format!("{name} — {state}")
                } else {
                    format!("{name} ({display}) — {state}")
                }
            })
            .collect();
        match Select::new("Select Hermes profile", labels).prompt() {
            Ok(selected) => {
                if let Some(index) = profiles.iter().position(|p| {
                    let n = p.get("name").and_then(Value::as_str).unwrap_or("default");
                    selected.starts_with(n)
                }) {
                    self.hermes_profile = profiles[index]
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_string();
                } else if let Some(index) = selected.split(' ').next() {
                    self.hermes_profile = index.to_string();
                }
                status_ok(&format!("Active profile: \"{}\"", self.hermes_profile));
                pause();
            }
            Err(_) => {}
        }
    }

    /// Drop a stale profile selection back to `default` when it disappeared.
    fn refresh_hermes_profile(&mut self) {
        let profiles = self.api.list_hermes_profiles().array("profiles");
        if profiles.is_empty() {
            return;
        }
        let exists = profiles
            .iter()
            .any(|p| p.get("name").and_then(Value::as_str) == Some(self.hermes_profile.as_str()));
        if !exists {
            self.hermes_profile = "default".to_string();
        }
    }

    fn cli_tool_quick_setup(&mut self, tool: &str) {
        let (endpoint, _) = api::endpoint(&self.api, self.port);
        let Some(key) = self.first_api_key() else {
            status_err("No API keys found. Create one in API Keys menu first.");
            pause();
            return;
        };
        let res = match tool {
            "claude" => {
                // Sets endpoint, key, and all default models (matching the Web UI).
                let env = json!({
                    "ANTHROPIC_BASE_URL": endpoint,
                    "ANTHROPIC_AUTH_TOKEN": key,
                    "API_TIMEOUT_MS": "600000",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "cc/claude-sonnet-4-5-20250929",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "cc/claude-opus-4-5-20251101",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "cc/claude-haiku-4-5-20251001",
                });
                self.api.apply_cli_tool_settings("claude", &json!({ "env": env }))
            }
            "codex" => self.api.apply_cli_tool_settings(
                "codex",
                &json!({ "baseUrl": endpoint, "apiKey": key, "model": "cx/claude-sonnet-4-5-20250929" }),
            ),
            "droid" => self.api.apply_cli_tool_settings(
                "droid",
                &json!({ "baseUrl": endpoint, "apiKey": key, "model": "cc/claude-sonnet-4-5-20250929" }),
            ),
            "openclaw" => self.api.apply_cli_tool_settings(
                "openclaw",
                &json!({ "baseUrl": endpoint, "apiKey": key, "model": "cc/claude-sonnet-4-5-20250929" }),
            ),
            "opencode" => self.api.apply_cli_tool_settings(
                "opencode",
                &json!({
                    "baseUrl": endpoint,
                    "apiKey": key,
                    "models": ["cc/claude-sonnet-4-5-20250929"],
                    "activeModel": "cc/claude-sonnet-4-5-20250929",
                    "subagentModel": "cc/claude-sonnet-4-5-20250929",
                }),
            ),
            "hermes" => self.api.apply_cli_tool_settings(
                "hermes",
                &json!({
                    "profile": self.hermes_profile,
                    "baseUrl": endpoint,
                    "apiKey": key,
                    "model": "cc/claude-sonnet-4-5-20250929",
                }),
            ),
            other => ApiResult::err(format!("Unknown CLI tool: {other}")),
        };
        let ok = res.ok;
        let err = res.error_text().to_string();
        if ok {
            status_ok(&format!("{tool} setup completed!"));
        } else {
            status_err(&format!("Failed: {err}"));
        }
        pause();
    }

    fn cli_tool_select_model(&mut self, tool: &str, env_key: &str) {
        let models = self.api.get_available_models();
        let options: Vec<String> = models
            .array("data")
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        if options.is_empty() {
            status_err("No models available. Connect a provider first.");
            pause();
            return;
        }
        let selected = match Select::new(&format!("Select {env_key}"), options)
            .with_page_size(20)
            .prompt()
        {
            Ok(s) => s,
            Err(_) => return,
        };
        let settings = self.api.get_cli_tool_settings(tool, None);
        let mut env = serde_json::Map::new();
        env.insert(env_key.to_string(), json!(selected));
        // Also set base URL + key when the tool is not configured yet.
        if settings.get("settings")["env"]["ANTHROPIC_BASE_URL"].as_str().is_none() {
            let (endpoint, _) = api::endpoint(&self.api, self.port);
            env.insert("ANTHROPIC_BASE_URL".to_string(), json!(endpoint));
            env.insert("API_TIMEOUT_MS".to_string(), json!("600000"));
            if let Some(key) = self.first_api_key() {
                env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), json!(key));
            }
        }
        let res = self.api.apply_cli_tool_settings(tool, &json!({ "env": env }));
        let ok = res.ok;
        let err = res.error_text().to_string();
        if ok {
            status_ok(&format!("{env_key} → {selected} saved!"));
        } else {
            status_err(&format!("Failed: {err}"));
        }
        pause();
    }

    fn first_api_key(&self) -> Option<String> {
        self.api
            .get_api_keys()
            .array("keys")
            .first()
            .and_then(|k| k.get("key").and_then(Value::as_str))
            .filter(|k| !k.is_empty())
            .map(str::to_string)
    }
}

/// Detail block for one API key (`handleViewFullKey`).
pub fn api_key_detail(key: &Value) -> String {
    let name = key.get("name").and_then(Value::as_str).unwrap_or("Unnamed");
    let value = key.get("key").and_then(Value::as_str).unwrap_or("");
    let id = key.get("id").and_then(Value::as_str).unwrap_or("");
    let created = format::format_date(&key.get("createdAt").cloned().unwrap_or(Value::Null));
    let last_used = match key.get("lastUsedAt").filter(|v| !v.is_null()) {
        Some(v) => format::relative_time(v),
        None => "Never".to_string(),
    };
    format!(
        "\n🔍 Full API Key\n──────────────────────────────\nName: {name}\nKey: {value}\nID: {id}\nCreated: {created}\nLast used: {last_used}\n"
    )
}

/// Extract `code` and `state` from a pasted callback URL
/// (the `new URL(callbackUrl)` block in providers.js).
pub fn parse_callback(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    let url = reqwest::Url::parse(trimmed).ok()?;
    let mut code = String::new();
    let mut state = String::new();
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = v.into_owned(),
            "state" => state = v.into_owned(),
            _ => {}
        }
    }
    if code.is_empty() {
        None
    } else {
        Some((code, state))
    }
}

fn find_by_id(list: &[Value], id: &str) -> Option<Value> {
    list.iter().find(|v| v.get("id").and_then(Value::as_str) == Some(id)).cloned()
}

fn print_chain(combos: &[Value], id: &str) {
    if let Some(combo) = find_by_id(combos, id) {
        let models = combo.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
        println!("\nCombo: {}", combo.get("name").and_then(Value::as_str).unwrap_or(""));
        println!("Models: {}", format::models_chain(&models));
    }
}

// ── terminal helpers (utils/input.js + utils/display.js) ────────────────────

pub fn status_ok(msg: &str) {
    println!("\x1b[32m✓ {msg}\x1b[0m");
}
pub fn status_err(msg: &str) {
    println!("\x1b[31m✗ {msg}\x1b[0m");
}
pub fn status_warn(msg: &str) {
    println!("\x1b[33m⚠ {msg}\x1b[0m");
}
pub fn status_info(msg: &str) {
    println!("\x1b[36mℹ {msg}\x1b[0m");
}

/// `pause("Press Enter to continue...")`.
pub fn pause() {
    print!("\nPress Enter to continue...");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().lock().read_line(&mut buf);
}

/// `prompt(question)` returning a trimmed answer, `None` on EOF.
pub fn prompt_line(question: &str) -> Option<String> {
    print!("{question}");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    match io::stdin().lock().read_line(&mut buf) {
        Ok(0) => None,
        Ok(_) => Some(buf.trim().to_string()),
        Err(_) => None,
    }
}

/// `confirm(question)` with a default for non-TTY runs.
pub fn confirm(question: &str, default: bool) -> bool {
    match Confirm::new(question).with_default(default).prompt() {
        Ok(v) => v,
        Err(_) => false,
    }
}

/// `prompt` for secrets that should not echo (dashboard password etc.).
pub fn prompt_secret(question: &str) -> Option<String> {
    Password::new(question).without_confirmation().prompt().ok()
}

/// `copyToClipboard` — pbcopy / clip / xclip / xsel.
pub fn copy_to_clipboard(text: &str) -> bool {
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else {
        &[("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"]), ("wl-copy", &[])]
    };
    for (cmd, args) in candidates {
        let child = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// `openBrowser` — used when the tray asks for the dashboard.
pub fn open_browser(url: &str) {
    let _ = open::that(url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_detail_shows_the_full_key_and_created_date() {
        let text = api_key_detail(&json!({
            "name": "laptop",
            "key": "sk-1234567890abcdef",
            "id": "k1",
            "createdAt": "2024-05-01T10:20:30Z",
        }));
        assert!(text.contains("Name: laptop"), "{text}");
        assert!(text.contains("Key: sk-1234567890abcdef"), "{text}");
        assert!(text.contains("ID: k1"), "{text}");
        assert!(text.contains("Created: 2024-05-01"), "{text}");
        assert!(text.contains("Last used: Never"), "{text}");
    }

    #[test]
    fn api_key_detail_reports_relative_last_use() {
        let text = api_key_detail(&json!({"name": "n", "key": "k", "lastUsedAt": "2020-01-01T00:00:00Z"}));
        assert!(text.contains("year"), "{text}");
    }

    #[test]
    fn parse_callback_reads_code_and_state() {
        assert_eq!(
            parse_callback("http://localhost:20128/callback?code=abc&state=xyz"),
            Some(("abc".to_string(), "xyz".to_string()))
        );
        assert_eq!(
            parse_callback("  http://localhost:1455/auth/callback?state=s&code=c  "),
            Some(("c".to_string(), "s".to_string()))
        );
        assert_eq!(parse_callback("http://localhost/callback?state=only"), None);
        assert_eq!(parse_callback("not a url"), None);
        assert_eq!(parse_callback(""), None);
    }

    #[test]
    fn find_by_id_matches_only_the_exact_id() {
        let list = vec![json!({"id": "a"}), json!({"id": "b"})];
        assert_eq!(find_by_id(&list, "b").unwrap()["id"], "b");
        assert!(find_by_id(&list, "c").is_none());
    }

    #[test]
    fn terminal_starts_at_the_main_menu_with_defaults() {
        let t = Terminal::new(20128);
        assert_eq!(t.nav.current(), MenuId::Main);
        assert_eq!(t.hermes_profile, "default");
        assert_eq!(t.usage_period, "7d");
        assert_eq!(t.api.base_url(), "http://localhost:20128");
    }
}
