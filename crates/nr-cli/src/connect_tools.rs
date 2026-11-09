//! Per-tool config writers for `9router connect` / `show` / `--reset`.
//! Faithful port of cli/src/cli/commands/connectTools.js. File layout and keys
//! mirror the dashboard routes in src/app/api/cli-tools/<tool>-settings/route.js
//! so a tool configured here looks identical to one applied from the dashboard.
//!
//! Each tool: { id, name, paths(), apply(ctx) -> files written, show() -> settings, reset() -> files touched }
//! `ctx`: baseUrl (origin, no /v1), apiKey, model, claudeModels: { envKey: model }
//!
//! Every method takes `home: &Path` (the user's home dir) so tests can point at a
//! temp dir instead of the real `~`.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{value, DocumentMut};

/// One-time backup of the user's pre-9router file, then write (mode 0600).
fn write_file(file: &Path, content: &str) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    // JS: `${file}.bak-9router` — appended, so a multi-dot name keeps its extension.
    let mut backup = file.as_os_str().to_owned();
    backup.push(".bak-9router");
    let backup = PathBuf::from(backup);
    if file.exists() && !backup.exists() {
        fs::copy(file, &backup)?;
    }
    let mut f = fs::File::create(file)?;
    f.write_all(content.as_bytes())?;
    set_owner_only(file);
    Ok(())
}

/// Rewrite an existing file on reset (no backup, existing mode kept).
fn rewrite_file(file: &Path, content: &str) -> Result<()> {
    let mut f = fs::File::create(file)?;
    f.write_all(content.as_bytes())?;
    set_owner_only(file);
    Ok(())
}

#[cfg(unix)]
fn set_owner_only(file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(file) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(file, perms);
    }
}

#[cfg(not(unix))]
fn set_owner_only(_file: &Path) {}

/// Drop trailing commas (JSONC) outside string literals, so `"a,}"` survives.
fn strip_trailing_commas(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            // Peek past whitespace for } or ]
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == '}' || bytes[j] == ']') {
                i += 1; // skip the comma
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Read a JSON(C) file; Ok(None) if it doesn't exist, Err on parse failure.
fn read_json(file: &Path) -> Result<Option<Value>> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("Cannot read {}: {}", file.display(), e)),
    };
    let parsed = serde_json::from_str(&strip_trailing_commas(&text))
        .map_err(|e| anyhow!("Cannot parse {}: {}", file.display(), e))?;
    Ok(Some(parsed))
}

fn is_str(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if !s.is_empty())
}

fn v1(base_url: &str) -> String {
    if base_url.ends_with("/v1") {
        base_url.to_string()
    } else {
        format!("{}/v1", base_url)
    }
}

/// Strip `/v1` for tools whose config expects the bare origin (cline).
fn origin_of(base_url: &str) -> String {
    let rest = base_url.strip_suffix("/v1").unwrap_or(base_url);
    rest.trim_end_matches('/').to_string()
}

/// Settings read back by `show()` for one tool.
#[derive(Debug, Default, Clone)]
pub struct ShowInfo {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub models: BTreeMap<String, String>,
    /// Only set by tools where 9router can be present but not the active provider.
    pub active: Option<bool>,
}

impl ShowInfo {
    pub fn is_configured(&self) -> bool {
        self.base_url.is_some() || self.api_key.is_some() || !self.models.is_empty()
    }
}

/// Context passed to a tool's `apply`.
#[derive(Debug, Clone, Default)]
pub struct ApplyCtx {
    /// Server origin, no `/v1` suffix.
    pub base_url: String,
    pub api_key: String,
    /// Model id for tools that take a single model (None skips most tools).
    pub model: Option<String>,
    /// Claude tier envKey -> model id; tiers absent here are unset.
    pub claude_models: BTreeMap<String, String>,
}

/// A per-tool config writer, mirroring one entry of the JS `TOOLS` array.
pub trait ToolConfig: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// Local config files this tool owns.
    fn paths(&self, home: &Path) -> Vec<PathBuf>;
    /// Dashboard route (`/api/cli-tools/<route>`) used to inherit the server's model.
    fn route(&self) -> Option<&'static str> {
        None
    }
    /// false for tools that discover models themselves (omp) — no model needed.
    fn needs_model(&self) -> bool {
        true
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>>;
    fn show(&self, home: &Path) -> Result<ShowInfo>;
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>>;
    /// Extract this tool's model id from the server's cli-tools settings.
    fn server_model(&self, _data: &Value) -> Option<String> {
        None
    }
}

/// Let `Box<dyn ToolConfig>` be used wherever `T: ToolConfig` is expected.
impl ToolConfig for Box<dyn ToolConfig> {
    fn id(&self) -> &'static str {
        (**self).id()
    }
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        (**self).paths(home)
    }
    fn route(&self) -> Option<&'static str> {
        (**self).route()
    }
    fn needs_model(&self) -> bool {
        (**self).needs_model()
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        (**self).apply(home, ctx)
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        (**self).show(home)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        (**self).reset(home)
    }
    fn server_model(&self, data: &Value) -> Option<String> {
        (**self).server_model(data)
    }
}

// ── Claude Code ─────────────────────────────────────────────────────────────
pub const CLAUDE_MODELS: [(&str, &str); 4] = [
    ("fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
    ("opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
    ("sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
    ("haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
];
const CLAUDE_RESET_KEYS: [&str; 6] = [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

struct Claude;
fn claude_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

impl ToolConfig for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn name(&self) -> &'static str {
        "Claude Code"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![claude_path(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("claude-settings")
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = claude_path(home);
        let mut cur = read_json(&file)?.unwrap_or_else(|| json!({}));
        let obj = cur.as_object_mut().ok_or_else(|| anyhow!("settings.json not an object"))?;
        obj.insert("hasCompletedOnboarding".into(), json!(true));
        let env = obj.entry("env").or_insert_with(|| json!({}));
        let env = env.as_object_mut().ok_or_else(|| anyhow!("env not an object"))?;
        env.insert("ANTHROPIC_BASE_URL".into(), json!(v1(&ctx.base_url)));
        env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(ctx.api_key.clone()));
        for (_, env_key) in CLAUDE_MODELS {
            if let Some(model) = ctx.claude_models.get(env_key) {
                env.insert(env_key.to_string(), json!(model.clone()));
            } else {
                env.remove(env_key);
            }
        }
        write_file(&file, &serde_json::to_string_pretty(&cur)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let env = read_json(&claude_path(home))?
            .and_then(|v| v.get("env").cloned())
            .unwrap_or_else(|| json!({}));
        let mut info = ShowInfo {
            base_url: env.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str()).map(String::from),
            api_key: env.get("ANTHROPIC_AUTH_TOKEN").and_then(|v| v.as_str()).map(String::from),
            ..Default::default()
        };
        if is_str(env.get("ANTHROPIC_MODEL")) {
            info.models.insert("main".into(), env["ANTHROPIC_MODEL"].as_str().unwrap().into());
        }
        for (flag, env_key) in CLAUDE_MODELS {
            if is_str(env.get(env_key)) {
                info.models.insert(flag.into(), env[env_key].as_str().unwrap().into());
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = claude_path(home);
        let Some(mut cur) = read_json(&file)? else {
            return Ok(vec![]);
        };
        if let Some(env) = cur.get_mut("env").and_then(|v| v.as_object_mut()) {
            for k in CLAUDE_RESET_KEYS {
                env.remove(k);
            }
            if env.is_empty() {
                cur.as_object_mut().unwrap().remove("env");
            }
        }
        rewrite_file(&file, &serde_json::to_string_pretty(&cur)?)?;
        Ok(vec![file])
    }
}

// ── OpenAI Codex CLI ────────────────────────────────────────────────────────
struct Codex;
fn codex_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

/// Get (creating if absent) a sub-table under `key` in a TOML document.
fn toml_sub_table<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut toml_edit::Table {
    if !doc.contains_key(key) || doc[key].as_table().is_none() {
        doc[key] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc[key].as_table_mut().unwrap()
}

/// Get (creating if absent) a nested sub-table: `doc[parent][child]`.
fn toml_nested_table<'a>(doc: &'a mut DocumentMut, parent: &str, child: &str) -> &'a mut toml_edit::Table {
    let parent_tbl = toml_sub_table(doc, parent);
    if !parent_tbl.contains_key(child) || parent_tbl[child].as_table().is_none() {
        parent_tbl.insert(child, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    parent_tbl[child].as_table_mut().unwrap()
}

/// Read the Authorization bearer token out of a codex provider's http_headers.
/// `http_headers` may be an inline table, a standard table, or a string.
fn codex_auth_token(provider: &toml_edit::Table) -> Option<String> {
    let item = provider.get("http_headers")?;
    let auth = if let Some(s) = item.as_str() {
        s.to_string()
    } else {
        let table: &dyn toml_edit::TableLike = item
            .as_inline_table()
            .map(|t| t as &dyn toml_edit::TableLike)
            .or_else(|| item.as_table().map(|t| t as &dyn toml_edit::TableLike))?;
        table.get("Authorization")?.as_str()?.to_string()
    };
    Some(auth.trim_start_matches("Bearer ").trim_start_matches("bearer ").to_string())
}

impl ToolConfig for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn name(&self) -> &'static str {
        "OpenAI Codex CLI"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![codex_path(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("codex-settings")
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = codex_path(home);
        let model = ctx.model.clone().ok_or_else(|| anyhow!("codex requires a model"))?;
        let mut doc: DocumentMut = match fs::read_to_string(&file) {
            Ok(text) => text.parse().map_err(|e| anyhow!("Cannot parse {}: {}", file.display(), e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
            Err(e) => return Err(anyhow!("{}", e)),
        };
        doc["model"] = value(model.clone());
        doc["model_provider"] = value("9router");
        // Custom providers ignore auth.json — key must travel as a static header.
        let provider = toml_nested_table(&mut doc, "model_providers", "9router");
        provider.insert("name", value("9Router"));
        provider.insert("base_url", value(v1(&ctx.base_url)));
        provider.insert("wire_api", value("responses"));
        let mut headers = toml_edit::InlineTable::new();
        headers.insert("Authorization", format!("Bearer {}", ctx.api_key).into());
        provider.insert("http_headers", headers.into());
        let agents = toml_sub_table(&mut doc, "agents");
        agents.remove("subagent");
        agents.insert("default_subagent_model", value(model));
        write_file(&file, &doc.to_string())?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let text = match fs::read_to_string(&codex_path(home)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ShowInfo::default()),
            Err(e) => return Err(anyhow!("{}", e)),
        };
        let doc: DocumentMut = text.parse().map_err(|e| anyhow!("Cannot parse codex config: {}", e))?;
        let root = doc.as_table();
        let provider = root
            .get("model_providers")
            .and_then(|i| i.as_table())
            .and_then(|t| t.get("9router"))
            .and_then(|i| i.as_table());
        let mut info = ShowInfo {
            base_url: provider.and_then(|p| p.get("base_url")).and_then(|v| v.as_str()).map(String::from),
            api_key: provider.and_then(codex_auth_token),
            // Codex only routes through 9router when this provider is the active one.
            active: Some(
                root.get("model_provider")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "9router")
                    .unwrap_or(false),
            ),
            ..Default::default()
        };
        if is_str_value(root.get("model")) {
            info.models.insert("main".into(), root["model"].as_str().unwrap().into());
        }
        if let Some(sub) = root.get("agents").and_then(|i| i.as_table()) {
            if is_str_value(sub.get("default_subagent_model")) {
                info.models.insert(
                    "subagent".into(),
                    sub["default_subagent_model"].as_str().unwrap().into(),
                );
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = codex_path(home);
        let text = match fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(anyhow!("{}", e)),
        };
        let mut doc: DocumentMut = text.parse().map_err(|e| anyhow!("Cannot parse codex config: {}", e))?;
        let is_9r = doc
            .get("model_provider")
            .and_then(|v| v.as_str())
            .map(|s| s == "9router")
            .unwrap_or(false);
        if is_9r {
            doc.as_table_mut().remove("model");
            doc.as_table_mut().remove("model_provider");
        }
        if let Some(mp) = doc.get_mut("model_providers").and_then(|i| i.as_table_mut()) {
            mp.remove("9router");
        }
        if let Some(agents) = doc.get_mut("agents").and_then(|i| i.as_table_mut()) {
            agents.remove("default_subagent_model");
            agents.remove("subagent");
        }
        for k in ["model_providers", "agents"] {
            let empty = doc.get(k).and_then(|i| i.as_table()).map(|t| t.is_empty()).unwrap_or(false);
            if empty {
                doc.as_table_mut().remove(k);
            }
        }
        rewrite_file(&file, &doc.to_string())?;
        Ok(vec![file])
    }
}

fn is_str_value(item: Option<&toml_edit::Item>) -> bool {
    item.and_then(|i| i.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
}

// ── OpenCode ────────────────────────────────────────────────────────────────
struct OpenCode;
fn opencode_path(home: &Path) -> PathBuf {
    home.join(".config").join("opencode").join("opencode.json")
}

impl ToolConfig for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn name(&self) -> &'static str {
        "OpenCode"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![opencode_path(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("opencode-settings")
    }
    fn server_model(&self, data: &Value) -> Option<String> {
        let m = data.get("config")?.get("model")?.as_str()?;
        m.strip_prefix("9router/").map(String::from)
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = opencode_path(home);
        let model = ctx.model.clone().ok_or_else(|| anyhow!("opencode requires a model"))?;
        let mut cfg = read_json(&file)?.unwrap_or_else(|| json!({}));
        let obj = cfg.as_object_mut().ok_or_else(|| anyhow!("opencode.json not an object"))?;
        let provider = obj
            .entry("provider")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("provider not an object"))?;
        let p = provider
            .entry("9router")
            .or_insert_with(|| json!({"npm": "@ai-sdk/openai-compatible", "options": {}, "models": {}}));
        let p = p.as_object_mut().ok_or_else(|| anyhow!("provider[9router] not an object"))?;
        let options = p.entry("options").or_insert_with(|| json!({}));
        let options = options.as_object_mut().ok_or_else(|| anyhow!("options not an object"))?;
        options.insert("baseURL".into(), json!(v1(&ctx.base_url)));
        options.insert("apiKey".into(), json!(ctx.api_key.clone()));
        let models = p.entry("models").or_insert_with(|| json!({}));
        let models = models.as_object_mut().ok_or_else(|| anyhow!("models not an object"))?;
        models.insert(
            model.clone(),
            json!({"name": model, "modalities": {"input": ["text", "image"], "output": ["text"]}}),
        );
        obj.insert("model".into(), json!(format!("9router/{}", model)));
        let agent = obj
            .entry("agent")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("agent not an object"))?;
        agent.insert(
            "explorer".into(),
            json!({
                "description": "Fast explorer subagent for codebase exploration",
                "mode": "subagent",
                "model": format!("9router/{}", model),
            }),
        );
        write_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let cfg = read_json(&opencode_path(home))?.unwrap_or_else(|| json!({}));
        let mut info = ShowInfo::default();
        if is_str(cfg.get("model")) {
            info.models.insert("main".into(), cfg["model"].as_str().unwrap().into());
        }
        if is_str(cfg.get("agent").and_then(|a| a.get("explorer")).and_then(|e| e.get("model"))) {
            info.models.insert(
                "explorer".into(),
                cfg["agent"]["explorer"]["model"].as_str().unwrap().into(),
            );
        }
        if let Some(p) = cfg.get("provider").and_then(|p| p.get("9router")) {
            info.base_url = p.pointer("/options/baseURL").and_then(|v| v.as_str()).map(String::from);
            info.api_key = p.pointer("/options/apiKey").and_then(|v| v.as_str()).map(String::from);
            let listed = p.get("models").and_then(|m| m.as_object()).map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
            if !listed.is_empty() {
                info.models.insert("available".into(), listed.join(", "));
            }
        }
        info.active = Some(
            cfg.get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.starts_with("9router/"))
                .unwrap_or(false),
        );
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = opencode_path(home);
        let Some(mut cfg) = read_json(&file)? else {
            return Ok(vec![]);
        };
        if let Some(provider) = cfg.get_mut("provider").and_then(|v| v.as_object_mut()) {
            provider.remove("9router");
        }
        let obj = cfg.as_object_mut().unwrap();
        if let Some(model) = obj.get("model").and_then(|v| v.as_str()) {
            if model.starts_with("9router/") {
                obj.remove("model");
            }
        }
        let explorer_has_9r = obj
            .get("agent")
            .and_then(|a| a.get("explorer"))
            .and_then(|e| e.get("model"))
            .and_then(|m| m.as_str())
            .map(|s| s.starts_with("9router/"))
            .unwrap_or(false);
        if explorer_has_9r {
            if let Some(agent) = obj.get_mut("agent").and_then(|v| v.as_object_mut()) {
                agent.remove("explorer");
            }
            if obj.get("agent").and_then(|v| v.as_object()).map(|a| a.is_empty()).unwrap_or(false) {
                obj.remove("agent");
            }
        }
        rewrite_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
}

// ── Factory Droid ───────────────────────────────────────────────────────────
struct Droid;
fn droid_path(home: &Path) -> PathBuf {
    home.join(".factory").join("settings.json")
}
fn is_droid_9r(m: &Value) -> bool {
    m.get("id").and_then(|v| v.as_str()).map(|s| s.starts_with("custom:9Router")).unwrap_or(false)
}

impl ToolConfig for Droid {
    fn id(&self) -> &'static str {
        "droid"
    }
    fn name(&self) -> &'static str {
        "Factory Droid"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![droid_path(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("droid-settings")
    }
    fn server_model(&self, data: &Value) -> Option<String> {
        let models = data.get("settings")?.get("customModels")?.as_array()?;
        models.iter().find(|m| is_droid_9r(m))?.get("model")?.as_str().map(String::from)
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = droid_path(home);
        let model = ctx.model.clone().ok_or_else(|| anyhow!("droid requires a model"))?;
        let mut cfg = read_json(&file)?.unwrap_or_else(|| json!({}));
        let obj = cfg.as_object_mut().ok_or_else(|| anyhow!("settings.json not an object"))?;
        let others: Vec<Value> = obj
            .get("customModels")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter(|m| !is_droid_9r(m)).cloned().collect())
            .unwrap_or_default();
        let mut custom = vec![json!({
            "model": model,
            "id": "custom:9Router-0",
            "index": 0,
            "baseUrl": v1(&ctx.base_url),
            "apiKey": ctx.api_key,
            "displayName": model,
            "maxOutputTokens": 131072,
            "noImageSupport": false,
            "provider": "openai",
        })];
        custom.extend(others);
        for (i, m) in custom.iter_mut().enumerate() {
            m.as_object_mut().unwrap().insert("index".into(), json!(i));
        }
        obj.insert("customModels".into(), Value::Array(custom));
        write_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let ours: Vec<Value> = read_json(&droid_path(home))?
            .and_then(|v| v.get("customModels").cloned())
            .and_then(|v| v.as_array().cloned())
            .map(|a| a.into_iter().filter(|m| is_droid_9r(m)).collect())
            .unwrap_or_default();
        let mut info = ShowInfo::default();
        if let Some(first) = ours.first() {
            info.base_url = first.get("baseUrl").and_then(|v| v.as_str()).map(String::from);
            info.api_key = first.get("apiKey").and_then(|v| v.as_str()).map(String::from);
            let models: Vec<String> = ours.iter().filter_map(|m| m.get("model").and_then(|v| v.as_str())).map(String::from).collect();
            if let Some(main) = models.first() {
                info.models.insert("main".into(), main.clone());
            }
            if models.len() > 1 {
                info.models.insert("available".into(), models.join(", "));
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = droid_path(home);
        let Some(mut cfg) = read_json(&file)? else {
            return Ok(vec![]);
        };
        let obj = cfg.as_object_mut().unwrap();
        if let Some(models) = obj.get("customModels").and_then(|v| v.as_array()).cloned() {
            let kept: Vec<Value> = models.into_iter().filter(|m| !is_droid_9r(m)).collect();
            if kept.is_empty() {
                obj.remove("customModels");
            } else {
                obj.insert("customModels".into(), Value::Array(kept));
            }
        }
        rewrite_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
}

// ── Crush ───────────────────────────────────────────────────────────────────
struct Crush;
fn crush_path(home: &Path) -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    base.join("crush").join("crush.json")
}

impl ToolConfig for Crush {
    fn id(&self) -> &'static str {
        "crush"
    }
    fn name(&self) -> &'static str {
        "Crush"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![crush_path(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("crush-settings")
    }
    fn server_model(&self, data: &Value) -> Option<String> {
        data.pointer("/config/providers/9router/models/0/id")
            .and_then(|v| v.as_str())
            .map(String::from)
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = crush_path(home);
        let model = ctx.model.clone().ok_or_else(|| anyhow!("crush requires a model"))?;
        let mut cfg = read_json(&file)?.unwrap_or_else(|| json!({}));
        let obj = cfg.as_object_mut().ok_or_else(|| anyhow!("crush.json not an object"))?;
        let providers = obj
            .entry("providers")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("providers not an object"))?;
        providers.insert(
            "9router".into(),
            json!({
                "type": "openai-compat",
                "base_url": v1(&ctx.base_url),
                "api_key": ctx.api_key,
                "models": [{"id": model, "name": model, "context_window": 128000}],
            }),
        );
        write_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let p = read_json(&crush_path(home))?
            .and_then(|v| v.pointer("/providers/9router").cloned());
        let mut info = ShowInfo::default();
        if let Some(p) = &p {
            info.base_url = p.get("base_url").and_then(|v| v.as_str()).map(String::from);
            info.api_key = p.get("api_key").and_then(|v| v.as_str()).map(String::from);
            let ids: Vec<String> = p.get("models").and_then(|m| m.as_array()).map(|a| {
                a.iter().filter_map(|m| m.get("id").and_then(|v| v.as_str())).map(String::from).collect()
            }).unwrap_or_default();
            if let Some(main) = ids.first() {
                info.models.insert("main".into(), main.clone());
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = crush_path(home);
        let Some(mut cfg) = read_json(&file)? else {
            return Ok(vec![]);
        };
        if cfg.pointer("/providers/9router").is_none() {
            return Ok(vec![]);
        }
        let providers = cfg.get_mut("providers").and_then(|v| v.as_object_mut()).unwrap();
        providers.remove("9router");
        if providers.is_empty() {
            cfg.as_object_mut().unwrap().remove("providers");
        }
        rewrite_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
}

// ── Kilo Code (CLI auth only; VS Code settings left to the dashboard) ───────
struct Kilo;
fn kilo_path(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("kilo").join("auth.json")
}

impl ToolConfig for Kilo {
    fn id(&self) -> &'static str {
        "kilo"
    }
    fn name(&self) -> &'static str {
        "Kilo Code CLI"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![kilo_path(home)]
    }
    // No route: the dashboard GET exposes only auth key names, not the model,
    // so kilo falls back to the shared model.
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = kilo_path(home);
        let model = ctx.model.clone().ok_or_else(|| anyhow!("kilo requires a model"))?;
        let mut auth = read_json(&file)?.unwrap_or_else(|| json!({}));
        let obj = auth.as_object_mut().ok_or_else(|| anyhow!("auth.json not an object"))?;
        obj.insert(
            "openai-compatible".into(),
            json!({"type": "api-key", "apiKey": ctx.api_key, "baseUrl": v1(&ctx.base_url), "model": model}),
        );
        write_file(&file, &serde_json::to_string_pretty(&auth)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let a = read_json(&kilo_path(home))?.and_then(|v| v.get("openai-compatible").cloned());
        let mut info = ShowInfo::default();
        if let Some(a) = &a {
            info.base_url = a.get("baseUrl").and_then(|v| v.as_str()).map(String::from);
            info.api_key = a.get("apiKey").and_then(|v| v.as_str()).map(String::from);
            if is_str(a.get("model")) {
                info.models.insert("main".into(), a["model"].as_str().unwrap().into());
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = kilo_path(home);
        let Some(mut auth) = read_json(&file)? else {
            return Ok(vec![]);
        };
        let obj = auth.as_object_mut().unwrap();
        obj.remove("openai-compatible");
        obj.remove("9router");
        rewrite_file(&file, &serde_json::to_string_pretty(&auth)?)?;
        Ok(vec![file])
    }
}

// ── Cline CLI ───────────────────────────────────────────────────────────────
struct Cline;
fn cline_dir(home: &Path) -> PathBuf {
    home.join(".cline").join("data")
}
fn cline_state(home: &Path) -> PathBuf {
    cline_dir(home).join("globalState.json")
}
fn cline_secrets(home: &Path) -> PathBuf {
    cline_dir(home).join("secrets.json")
}

impl ToolConfig for Cline {
    fn id(&self) -> &'static str {
        "cline"
    }
    fn name(&self) -> &'static str {
        "Cline CLI"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![cline_state(home), cline_secrets(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("cline-settings")
    }
    fn server_model(&self, data: &Value) -> Option<String> {
        let st = data.get("settings")?;
        if st.get("actModeApiProvider").and_then(|v| v.as_str()) != Some("openai") {
            return None;
        }
        st.get("openAiModelId").and_then(|v| v.as_str()).map(String::from)
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let model = ctx.model.clone().ok_or_else(|| anyhow!("cline requires a model"))?;
        let state_file = cline_state(home);
        let mut state = read_json(&state_file)?.unwrap_or_else(|| json!({}));
        let obj = state.as_object_mut().ok_or_else(|| anyhow!("globalState.json not an object"))?;
        obj.insert("actModeApiProvider".into(), json!("openai"));
        obj.insert("planModeApiProvider".into(), json!("openai"));
        obj.insert("openAiBaseUrl".into(), json!(origin_of(&ctx.base_url))); // Cline expects base WITHOUT /v1
        obj.insert("openAiModelId".into(), json!(model.clone()));
        obj.insert("planModeOpenAiModelId".into(), json!(model));
        write_file(&state_file, &serde_json::to_string_pretty(&state)?)?;
        let secrets_file = cline_secrets(home);
        let mut secrets = read_json(&secrets_file)?.unwrap_or_else(|| json!({}));
        secrets
            .as_object_mut()
            .ok_or_else(|| anyhow!("secrets.json not an object"))?
            .insert("openAiApiKey".into(), json!(ctx.api_key.clone()));
        write_file(&secrets_file, &serde_json::to_string_pretty(&secrets)?)?;
        Ok(vec![state_file, secrets_file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let st = read_json(&cline_state(home))?.unwrap_or_else(|| json!({}));
        let mut info = ShowInfo::default();
        if is_str(st.get("openAiModelId")) {
            info.models.insert("act".into(), st["openAiModelId"].as_str().unwrap().into());
        }
        if is_str(st.get("planModeOpenAiModelId")) {
            info.models.insert("plan".into(), st["planModeOpenAiModelId"].as_str().unwrap().into());
        }
        info.base_url = st.get("openAiBaseUrl").and_then(|v| v.as_str()).map(String::from);
        info.api_key = read_json(&cline_secrets(home))?
            .and_then(|v| v.get("openAiApiKey").cloned())
            .and_then(|v| v.as_str().map(String::from));
        info.active = Some(
            st.get("actModeApiProvider")
                .and_then(|v| v.as_str())
                .map(|s| s == "openai")
                .unwrap_or(false),
        );
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let state_file = cline_state(home);
        let Some(mut state) = read_json(&state_file)? else {
            return Ok(vec![]);
        };
        if state.get("actModeApiProvider").and_then(|v| v.as_str()) == Some("openai") {
            let obj = state.as_object_mut().unwrap();
            obj.remove("openAiBaseUrl");
            obj.remove("openAiModelId");
            obj.remove("planModeOpenAiModelId");
            obj.insert("actModeApiProvider".into(), json!("cline"));
            obj.insert("planModeApiProvider".into(), json!("cline"));
        }
        rewrite_file(&state_file, &serde_json::to_string_pretty(&state)?)?;
        let mut touched = vec![state_file];
        let secrets_file = cline_secrets(home);
        if let Some(mut secrets) = read_json(&secrets_file)? {
            secrets
                .as_object_mut()
                .ok_or_else(|| anyhow!("secrets.json not an object"))?
                .remove("openAiApiKey");
            rewrite_file(&secrets_file, &serde_json::to_string_pretty(&secrets)?)?;
            touched.push(secrets_file);
        }
        Ok(touched)
    }
}

// ── Pi (pi-coding-agent) ────────────────────────────────────────────────────
// Prefers ~/.pi/agent/models.json; an existing legacy ~/.pi/models.json is used instead.
struct Pi;
fn pi_path(home: &Path) -> PathBuf {
    let agent = home.join(".pi").join("agent").join("models.json");
    let root = home.join(".pi").join("models.json");
    if !agent.exists() && root.exists() {
        root
    } else {
        agent
    }
}

impl ToolConfig for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }
    fn name(&self) -> &'static str {
        "Pi"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![pi_path(home)]
    }
    fn route(&self) -> Option<&'static str> {
        Some("pi-settings")
    }
    fn server_model(&self, data: &Value) -> Option<String> {
        let m = data.pointer("/config/providers/9router/models/0")?;
        let id = m.as_str().map(String::from).unwrap_or_else(|| m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string());
        if id.is_empty() { None } else { Some(id) }
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = pi_path(home);
        let model = ctx.model.clone().ok_or_else(|| anyhow!("pi requires a model"))?;
        let mut cfg = read_json(&file)?.unwrap_or_else(|| json!({}));
        let obj = cfg.as_object_mut().ok_or_else(|| anyhow!("models.json not an object"))?;
        let providers = obj
            .entry("providers")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("providers not an object"))?;
        let existing = providers.get("9router").cloned().unwrap_or_else(|| json!({}));
        let mut merged = existing.as_object().cloned().unwrap_or_else(Map::new);
        merged.insert("baseUrl".into(), json!(v1(&ctx.base_url)));
        merged.insert("apiKey".into(), json!(ctx.api_key.clone()));
        merged.insert("api".into(), json!("openai-completions"));
        merged.insert(
            "models".into(),
            json!([{"id": model, "name": model, "contextWindow": 128000, "maxTokens": 16384}]),
        );
        providers.insert("9router".into(), Value::Object(merged));
        write_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let p = read_json(&pi_path(home))?.and_then(|v| v.pointer("/providers/9router").cloned());
        let mut info = ShowInfo::default();
        if let Some(p) = &p {
            info.base_url = p.get("baseUrl").and_then(|v| v.as_str()).map(String::from);
            info.api_key = p.get("apiKey").and_then(|v| v.as_str()).map(String::from);
            let ids: Vec<String> = p.get("models").and_then(|m| m.as_array()).map(|a| {
                a.iter()
                    .filter_map(|m| {
                        m.as_str().map(String::from).or_else(|| m.get("id").and_then(|v| v.as_str()).map(String::from))
                    })
                    .collect()
            }).unwrap_or_default();
            if let Some(main) = ids.first() {
                info.models.insert("main".into(), main.clone());
            }
            if ids.len() > 1 {
                info.models.insert("available".into(), ids.join(", "));
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = pi_path(home);
        let Some(mut cfg) = read_json(&file)? else {
            return Ok(vec![]);
        };
        if cfg.pointer("/providers/9router").is_none() {
            return Ok(vec![]);
        }
        let providers = cfg.get_mut("providers").and_then(|v| v.as_object_mut()).unwrap();
        providers.remove("9router");
        if providers.is_empty() {
            cfg.as_object_mut().unwrap().remove("providers");
        }
        rewrite_file(&file, &serde_json::to_string_pretty(&cfg)?)?;
        Ok(vec![file])
    }
}

// ── Oh My Pi ────────────────────────────────────────────────────────────────
// Proxy discovery lists every server model under /model, so no model is written.
struct Omp;
fn omp_path(home: &Path) -> PathBuf {
    home.join(".omp").join("agent").join("models.yml")
}

fn read_yaml(file: &Path) -> Result<Option<Value>> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("{}", e)),
    };
    let parsed: Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow!("Cannot parse {}: {}", file.display(), e))?;
    Ok(Some(parsed))
}

impl ToolConfig for Omp {
    fn id(&self) -> &'static str {
        "omp"
    }
    fn name(&self) -> &'static str {
        "Oh My Pi"
    }
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![omp_path(home)]
    }
    fn needs_model(&self) -> bool {
        false
    }
    fn apply(&self, home: &Path, ctx: &ApplyCtx) -> Result<Vec<PathBuf>> {
        let file = omp_path(home);
        let mut cfg = read_yaml(&file)?.unwrap_or_else(|| json!({}));
        let obj = cfg.as_object_mut().ok_or_else(|| anyhow!("models.yml not a mapping"))?;
        let providers = obj
            .entry("providers")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("providers not a mapping"))?;
        providers.insert(
            "9router".into(),
            json!({
                "baseUrl": v1(&ctx.base_url),
                "apiKey": ctx.api_key,
                "api": "openai-completions",
                "authHeader": true,
                "disableStrictTools": true,
                "discovery": {"type": "proxy"},
            }),
        );
        write_file(&file, &serde_yaml::to_string(&cfg)?)?;
        Ok(vec![file])
    }
    fn show(&self, home: &Path) -> Result<ShowInfo> {
        let p = read_yaml(&omp_path(home))?.and_then(|v| v.pointer("/providers/9router").cloned());
        let mut info = ShowInfo::default();
        if let Some(p) = &p {
            info.base_url = p.get("baseUrl").and_then(|v| v.as_str()).map(String::from);
            info.api_key = p.get("apiKey").and_then(|v| v.as_str()).map(String::from);
            if p.pointer("/discovery/type").and_then(|v| v.as_str()) == Some("proxy") {
                info.models.insert("main".into(), "(all server models, via discovery)".into());
            }
        }
        Ok(info)
    }
    fn reset(&self, home: &Path) -> Result<Vec<PathBuf>> {
        let file = omp_path(home);
        let Some(mut cfg) = read_yaml(&file)? else {
            return Ok(vec![]);
        };
        if cfg.pointer("/providers/9router").is_none() {
            return Ok(vec![]);
        }
        let providers = cfg.get_mut("providers").and_then(|v| v.as_object_mut()).unwrap();
        providers.remove("9router");
        let body = if providers.is_empty() {
            cfg.as_object_mut().unwrap().remove("providers");
            if cfg.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                String::new()
            } else {
                serde_yaml::to_string(&cfg)?
            }
        } else {
            serde_yaml::to_string(&cfg)?
        };
        rewrite_file(&file, &body)?;
        Ok(vec![file])
    }
}

/// Every supported tool, in the same order as the JS `TOOLS` array.
pub fn tools() -> Vec<Box<dyn ToolConfig>> {
    vec![
        Box::new(Claude),
        Box::new(Codex),
        Box::new(OpenCode),
        Box::new(Droid),
        Box::new(Crush),
        Box::new(Kilo),
        Box::new(Cline),
        Box::new(Pi),
        Box::new(Omp),
    ]
}

pub fn tool_ids() -> Vec<&'static str> {
    tools().iter().map(|t| t.id()).collect()
}

const TOOL_ALIASES: [(&str, &str); 6] = [
    ("claude-code", "claude"),
    ("claudecode", "claude"),
    ("factory", "droid"),
    ("kilocode", "kilo"),
    ("pi-coding-agent", "pi"),
    ("oh-my-pi", "omp"),
];

/// Resolve a list of tool names/aliases into concrete tools. "all" selects every tool.
pub fn resolve_tools(list: &[String]) -> Result<Vec<Box<dyn ToolConfig>>> {
    let all = tools();
    let mut ids: Vec<&'static str> = Vec::new();
    for raw in list {
        let id = raw.trim().to_lowercase();
        if id.is_empty() {
            continue;
        }
        if id == "all" {
            ids.extend(all.iter().map(|t| t.id()));
            continue;
        }
        let real = TOOL_ALIASES
            .iter()
            .find(|(a, _)| *a == id)
            .map(|(_, real)| *real)
            .unwrap_or_else(|| match id.as_str() {
                "claude" => "claude",
                "codex" => "codex",
                "opencode" => "opencode",
                "droid" => "droid",
                "crush" => "crush",
                "kilo" => "kilo",
                "cline" => "cline",
                "pi" => "pi",
                "omp" => "omp",
                _ => "",
            });
        if real.is_empty() || !all.iter().any(|t| t.id() == real) {
            return Err(anyhow!(
                "Unknown tool \"{}\". Supported: {}, all",
                raw,
                tool_ids().join(", ")
            ));
        }
        ids.push(real);
    }
    Ok(all.into_iter().filter(|t| ids.contains(&t.id())).collect())
}

/// Non-Claude tools that take a single model id (omp discovers models itself).
pub fn needs_model(t: &dyn ToolConfig) -> bool {
    t.id() != "claude" && t.needs_model()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn ctx(model: &str) -> ApplyCtx {
        ApplyCtx {
            base_url: "http://srv:20128".into(),
            api_key: "sk-test-1234567890abcd".into(),
            model: Some(model.into()),
            claude_models: BTreeMap::from([("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), "cc/sonnet".into())]),
        }
    }

    #[test]
    fn strip_trailing_commas_keeps_strings() {
        assert_eq!(strip_trailing_commas(r#"{"a":1,}"#), r#"{"a":1}"#);
        assert_eq!(strip_trailing_commas(r#"{"a":"x,}",}"#), r#"{"a":"x,}"}"#);
    }

    #[test]
    fn v1_appends_suffix() {
        assert_eq!(v1("http://x:1"), "http://x:1/v1");
        assert_eq!(v1("http://x:1/v1"), "http://x:1/v1");
    }

    #[test]
    fn origin_of_strips_v1() {
        assert_eq!(origin_of("http://srv:20128/v1"), "http://srv:20128");
        assert_eq!(origin_of("http://srv:20128"), "http://srv:20128");
    }

    #[test]
    fn resolve_tools_all_and_alias() {
        assert_eq!(resolve_tools(&["all".into()]).unwrap().len(), 9);
        assert_eq!(resolve_tools(&["claude-code".into()]).unwrap()[0].id(), "claude");
        assert!(resolve_tools(&["nope".into()]).is_err());
    }

    #[test]
    fn claude_apply_show_reset_roundtrip() {
        let home = tmp_home();
        let p = claude_path(home.path());
        Claude.apply(home.path(), &ctx("cc/x")).unwrap();
        let doc: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["env"]["ANTHROPIC_BASE_URL"], "http://srv:20128/v1");
        assert_eq!(doc["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "cc/sonnet");
        // tier absent from ctx is removed, not left stale
        assert!(doc["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none());
        let info = Claude.show(home.path()).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128/v1"));
        assert_eq!(info.models.get("sonnet").map(String::as_str), Some("cc/sonnet"));
        assert!(info.is_configured());
        let touched = Claude.reset(home.path()).unwrap();
        assert_eq!(touched, vec![p.clone()]);
        let doc: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert!(doc.get("env").is_none());
        assert!(!Claude.show(home.path()).unwrap().is_configured());
    }

    #[test]
    fn codex_apply_show_reset_roundtrip() {
        let home = tmp_home();
        let p = codex_path(home.path());
        // Pre-existing unrelated config must survive.
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "theme = \"dark\"\n[other]\nkey = 1\n").unwrap();
        Codex.apply(home.path(), &ctx("ocg/deepseek")).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("ocg/deepseek"));
        assert_eq!(doc["model_provider"].as_str(), Some("9router"));
        let token = codex_auth_token(doc["model_providers"]["9router"].as_table().unwrap()).unwrap();
        assert_eq!(token, "sk-test-1234567890abcd");
        assert_eq!(doc["theme"].as_str(), Some("dark"));
        let info = Codex.show(home.path()).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128/v1"));
        assert_eq!(info.models.get("main").map(String::as_str), Some("ocg/deepseek"));
        assert_eq!(info.models.get("subagent").map(String::as_str), Some("ocg/deepseek"));
        assert_eq!(info.active, Some(true));
        Codex.reset(home.path()).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        assert!(doc.get("model").is_none());
        assert!(doc.get("model_providers").is_none());
        assert_eq!(doc["theme"].as_str(), Some("dark")); // unrelated keys kept
    }

    #[test]
    fn codex_show_absent_returns_empty() {
        let home = tmp_home();
        let info = Codex.show(home.path()).unwrap();
        assert!(!info.is_configured());
        assert_eq!(info.active, None);
        // reset with no file is a no-op
        assert!(Codex.reset(home.path()).unwrap().is_empty());
    }

    #[test]
    fn opencode_apply_show_reset_roundtrip() {
        let home = tmp_home();
        OpenCode.apply(home.path(), &ctx("ocg/m")).unwrap();
        let info = OpenCode.show(home.path()).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128/v1"));
        assert_eq!(info.models.get("main").map(String::as_str), Some("9router/ocg/m"));
        assert_eq!(info.models.get("explorer").map(String::as_str), Some("9router/ocg/m"));
        assert!(info.models.contains_key("available"));
        assert_eq!(info.active, Some(true));
        let files = OpenCode.reset(home.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(!OpenCode.show(home.path()).unwrap().is_configured());
    }

    #[test]
    fn droid_preserves_other_custom_models_and_reindexes() {
        let home = tmp_home();
        let p = droid_path(home.path());
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        serde_json::to_writer(
            fs::File::create(&p).unwrap(),
            &json!({"customModels": [
                {"id": "custom:9Router-0", "model": "old", "index": 0},
                {"id": "custom:Other", "model": "keep", "index": 1}
            ]}),
        ).unwrap();
        Droid.apply(home.path(), &ctx("droid/m")).unwrap();
        let doc: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let arr = doc["customModels"].as_array().unwrap();
        assert_eq!(arr.len(), 2); // old 9Router entry replaced, other kept
        assert_eq!(arr[0]["model"], "droid/m");
        assert_eq!(arr[0]["baseUrl"], "http://srv:20128/v1");
        assert_eq!(arr[1]["id"], "custom:Other");
        arr.iter().enumerate().for_each(|(i, m)| assert_eq!(m["index"], i));
        Droid.reset(home.path()).unwrap();
        let doc: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["customModels"].as_array().unwrap().len(), 1);
        assert_eq!(doc["customModels"][0]["id"], "custom:Other");
    }

    #[test]
    fn crush_apply_show_reset_roundtrip() {
        let home = tmp_home();
        Crush.apply(home.path(), &ctx("crush/m")).unwrap();
        let info = Crush.show(home.path()).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128/v1"));
        assert_eq!(info.models.get("main").map(String::as_str), Some("crush/m"));
        Crush.reset(home.path()).unwrap();
        assert!(!Crush.show(home.path()).unwrap().is_configured());
        assert!(Crush.reset(home.path()).unwrap().is_empty());
    }

    #[test]
    fn kilo_apply_show_reset_roundtrip() {
        let home = tmp_home();
        Kilo.apply(home.path(), &ctx("kilo/m")).unwrap();
        let info = Kilo.show(home.path()).unwrap();
        assert_eq!(info.api_key.as_deref(), Some("sk-test-1234567890abcd"));
        assert_eq!(info.models.get("main").map(String::as_str), Some("kilo/m"));
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128/v1"));
        Kilo.reset(home.path()).unwrap();
        assert!(!Kilo.show(home.path()).unwrap().is_configured());
    }

    #[test]
    fn cline_uses_origin_not_v1_and_writes_secrets() {
        let home = tmp_home();
        let files = Cline.apply(home.path(), &ctx("cline/m")).unwrap();
        assert_eq!(files.len(), 2);
        let st: Value = serde_json::from_str(&fs::read_to_string(cline_state(home.path())).unwrap()).unwrap();
        assert_eq!(st["openAiBaseUrl"], "http://srv:20128"); // no /v1
        assert_eq!(st["openAiModelId"], "cline/m");
        let secrets: Value = serde_json::from_str(&fs::read_to_string(cline_secrets(home.path())).unwrap()).unwrap();
        assert_eq!(secrets["openAiApiKey"], "sk-test-1234567890abcd");
        let info = Cline.show(home.path()).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128"));
        assert_eq!(info.models.get("act").map(String::as_str), Some("cline/m"));
        assert_eq!(info.active, Some(true));
        let touched = Cline.reset(home.path()).unwrap();
        assert_eq!(touched.len(), 2);
        assert_eq!(touched[1], cline_secrets(home.path()));
        let st: Value = serde_json::from_str(&fs::read_to_string(cline_state(home.path())).unwrap()).unwrap();
        assert_eq!(st["actModeApiProvider"], "cline");
    }

    #[test]
    fn cline_reset_noop_when_no_state() {
        let home = tmp_home();
        assert!(Cline.reset(home.path()).unwrap().is_empty());
    }

    #[test]
    fn pi_prefers_legacy_root_when_present() {
        let home = tmp_home();
        let root = home.path().join(".pi").join("models.json");
        fs::create_dir_all(root.parent().unwrap()).unwrap();
        fs::write(&root, "{}").unwrap();
        Pi.apply(home.path(), &ctx("pi/m")).unwrap();
        assert!(root.exists());
        let doc: Value = serde_json::from_str(&fs::read_to_string(&root).unwrap()).unwrap();
        assert_eq!(doc["providers"]["9router"]["baseUrl"], "http://srv:20128/v1");
        assert_eq!(Pi.show(home.path()).unwrap().models.get("main").map(String::as_str), Some("pi/m"));
    }

    #[test]
    fn omp_needs_no_model_and_uses_yaml() {
        let home = tmp_home();
        let ctx = ApplyCtx {
            base_url: "http://srv:20128".into(),
            api_key: "sk-test-1234567890abcd".into(),
            model: None,
            claude_models: BTreeMap::new(),
        };
        Omp.apply(home.path(), &ctx).unwrap();
        let info = Omp.show(home.path()).unwrap();
        assert_eq!(info.base_url.as_deref(), Some("http://srv:20128/v1"));
        assert_eq!(info.models.get("main").map(String::as_str), Some("(all server models, via discovery)"));
        assert!(Omp.reset(home.path()).unwrap().len() == 1);
        assert!(!Omp.show(home.path()).unwrap().is_configured());
        // empty file after full removal
        let text = fs::read_to_string(omp_path(home.path())).unwrap();
        assert!(text.trim().is_empty());
    }

    #[test]
    fn omp_needs_model_false() {
        let omp = tools().into_iter().find(|t| t.id() == "omp").unwrap();
        assert!(!needs_model(omp.as_ref()));
    }

    #[test]
    fn write_file_backs_up_once_and_sets_mode_0600() {
        let home = tmp_home();
        let file = home.path().join("tool").join("config.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "original").unwrap();
        write_file(&file, "first").unwrap();
        write_file(&file, "second").unwrap();
        // JS appends `.bak-9router`, so a multi-dot name keeps its extension.
        let backup = home.path().join("tool").join("config.json.bak-9router");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "original");
        assert_eq!(fs::read_to_string(&file).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn malformed_json_is_an_error_not_silent() {
        let home = tmp_home();
        let file = claude_path(home.path());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{not json").unwrap();
        assert!(Claude.show(home.path()).is_err());
        assert!(Claude.apply(home.path(), &ctx("x")).is_err());
    }
}
