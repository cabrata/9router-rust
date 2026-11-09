//! Integration tests for the `connect` / `show` / `--reset` config flows.
//!
//! These exercise the real per-tool config writers against a temp home dir, so no
//! network, no server, and no writes to the developer's real `~/.claude` etc.

use nr_cli::connect_tools::{
    self, tool_ids, ApplyCtx, ShowInfo, ToolConfig,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn ctx(model: Option<&str>) -> ApplyCtx {
    ApplyCtx {
        base_url: "http://10.0.0.5:20128".into(),
        api_key: "sk-live-abcdef1234567890".into(),
        model: model.map(String::from),
        claude_models: BTreeMap::from([
            ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), "cc/claude-sonnet-5-5".into()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), "cc/claude-opus-5".into()),
        ]),
    }
}

fn find(home: &Path, id: &str) -> Box<dyn ToolConfig> {
    connect_tools::tools()
        .into_iter()
        .find(|t| t.id() == id)
        .unwrap_or_else(|| panic!("missing tool {id}"))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn every_tool_is_apply_show_reset_clean() {
    // Full lifecycle for all nine tools: no panics, and reset leaves nothing of ours.
    for id in tool_ids() {
        let home = tempfile::tempdir().unwrap();
        let tool = find(home.path(), id);
        let model = if connect_tools::needs_model(tool.as_ref()) {
            Some("srv/model-1")
        } else {
            None
        };
        tool.apply(home.path(), &ctx(model))
            .unwrap_or_else(|e| panic!("{id} apply failed: {e}"));
        let info = tool
            .show(home.path())
            .unwrap_or_else(|e| panic!("{id} show failed: {e}"));
        assert!(info.is_configured(), "{id} should be configured after apply");
        assert!(
            info.base_url.as_deref().is_some_and(|u| u.contains("10.0.0.5:20128")),
            "{id} base url should point at the server, got {:?}",
            info.base_url
        );
        assert_eq!(
            info.api_key.as_deref(),
            Some("sk-live-abcdef1234567890"),
            "{id} should store the API key"
        );
        tool.reset(home.path())
            .unwrap_or_else(|e| panic!("{id} reset failed: {e}"));
        let after = tool.show(home.path()).unwrap();
        assert!(
            !after.is_configured(),
            "{id} should not be configured after reset, got {after:?}"
        );
    }
}

#[test]
fn reset_is_idempotent_for_every_tool() {
    // Reset on a fresh home is a no-op for every tool. After the first reset the
    // config file still exists (it may hold unrelated user settings), so a second
    // reset may report that file again — as the JS tools do — but must never error
    // and must leave the tool unconfigured.
    for id in tool_ids() {
        let home = tempfile::tempdir().unwrap();
        let tool = find(home.path(), id);
        assert!(tool.reset(home.path()).unwrap().is_empty(), "{id} reset on empty home");
        let model = connect_tools::needs_model(tool.as_ref()).then_some("m");
        tool.apply(home.path(), &ctx(model)).unwrap();
        tool.reset(home.path()).unwrap();
        tool.reset(home.path())
            .unwrap_or_else(|e| panic!("{id} second reset errored: {e}"));
        assert!(
            !tool.show(home.path()).unwrap().is_configured(),
            "{id} must stay unconfigured after a second reset"
        );
    }
}

#[test]
fn model_less_tools_skip_but_omp_works() {
    // Only omp declares needs_model: false; everything else needs a model id.
    for id in tool_ids() {
        let tool = find(Path::new("/nonexistent"), id);
        if id == "omp" {
            assert!(!connect_tools::needs_model(tool.as_ref()));
        } else if id == "claude" {
            // claude takes tier env vars, not a single model
            assert!(!connect_tools::needs_model(tool.as_ref()));
        } else {
            assert!(connect_tools::needs_model(tool.as_ref()), "{id} needs a model");
        }
    }
}

#[test]
fn claude_writes_tiers_and_keeps_unrelated_settings() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".claude").join("settings.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, r#"{"theme":"dark","permissions":{"allow":["Bash"]}}"#).unwrap();

    find(home.path(), "claude").apply(home.path(), &ctx(None)).unwrap();
    let doc = read_json(&file);
    assert_eq!(doc["theme"], "dark", "unrelated keys survive");
    assert_eq!(doc["permissions"]["allow"][0], "Bash");
    assert_eq!(doc["hasCompletedOnboarding"], true);
    assert_eq!(doc["env"]["ANTHROPIC_BASE_URL"], "http://10.0.0.5:20128/v1");
    assert_eq!(doc["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "cc/claude-sonnet-5-5");
    assert_eq!(doc["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "cc/claude-opus-5");
    // haiku/fable absent from ctx must not be written
    assert!(doc["env"].get("ANTHROPIC_DEFAULT_HAIKU_MODEL").is_none());

    find(home.path(), "claude").reset(home.path()).unwrap();
    let doc = read_json(&file);
    assert_eq!(doc["theme"], "dark");
    assert!(doc.get("env").is_none(), "empty env removed entirely");
}

#[test]
fn claude_tier_dropped_when_server_unsets_it() {
    // A tier present from an earlier run must be removed once the server unsets it,
    // otherwise Claude Code silently keeps a stale model.
    let home = tempfile::tempdir().unwrap();
    let tool = find(home.path(), "claude");
    tool.apply(home.path(), &ctx(None)).unwrap();
    let mut next = ctx(None);
    next.claude_models.remove("ANTHROPIC_DEFAULT_OPUS_MODEL");
    tool.apply(home.path(), &next).unwrap();
    let info = tool.show(home.path()).unwrap();
    assert_eq!(info.models.get("sonnet").map(String::as_str), Some("cc/claude-sonnet-5-5"));
    assert!(!info.models.contains_key("opus"), "stale tier must be gone");
}

#[test]
fn codex_toml_roundtrip_preserves_other_providers() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".codex").join("config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        r#"
model = "gpt-5"
approval_policy = "on-request"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com/v1"
"#,
    )
    .unwrap();

    find(home.path(), "codex").apply(home.path(), &ctx(Some("ocg/deepseek-flash"))).unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    assert_eq!(doc["model"].as_str(), Some("ocg/deepseek-flash"));
    assert_eq!(doc["model_provider"].as_str(), Some("9router"));
    assert_eq!(doc["approval_policy"].as_str(), Some("on-request"));
    // The user's own provider is untouched.
    assert_eq!(doc["model_providers"]["openai"]["base_url"].as_str(), Some("https://api.openai.com/v1"));
    assert_eq!(
        doc["model_providers"]["9router"]["http_headers"]["Authorization"].as_str(),
        Some("Bearer sk-live-abcdef1234567890")
    );

    find(home.path(), "codex").reset(home.path()).unwrap();
    let doc: toml_edit::DocumentMut = fs::read_to_string(&file).unwrap().parse().unwrap();
    assert!(doc.get("model").is_none(), "active model cleared");
    assert!(doc.get("model_providers").unwrap().get("9router").is_none());
    assert!(doc.get("model_providers").unwrap().get("openai").is_some(), "other provider kept");
    assert_eq!(doc["approval_policy"].as_str(), Some("on-request"));
}

#[test]
fn show_reports_inactive_provider_for_codex() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".codex").join("config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    // 9router configured but a different provider is active.
    fs::write(
        &file,
        r#"
model = "gpt-5"
model_provider = "openai"

[model_providers.9router]
base_url = "http://10.0.0.5:20128/v1"
http_headers = { Authorization = "Bearer sk-live-abcdef1234567890" }
"#,
    )
    .unwrap();
    let info = find(home.path(), "codex").show(home.path()).unwrap();
    assert_eq!(info.active, Some(false), "codex reports presence without activation");
    assert!(info.is_configured());
}

#[test]
fn opencode_merges_existing_provider_and_reset_keeps_user_model() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".config").join("opencode").join("opencode.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        r#"{"theme":"tokyonight","provider":{"9router":{"npm":"@ai-sdk/openai-compatible","models":{}}}}"#,
    )
    .unwrap();

    find(home.path(), "opencode").apply(home.path(), &ctx(Some("ocg/m1"))).unwrap();
    let doc = read_json(&file);
    assert_eq!(doc["theme"], "tokyonight");
    assert_eq!(doc["provider"]["9router"]["options"]["baseURL"], "http://10.0.0.5:20128/v1");
    assert_eq!(doc["model"], "9router/ocg/m1");
    assert_eq!(doc["agent"]["explorer"]["mode"], "subagent");

    find(home.path(), "opencode").reset(home.path()).unwrap();
    let doc = read_json(&file);
    assert_eq!(doc["theme"], "tokyonight");
    assert!(doc["provider"].get("9router").is_none());
    assert!(doc.get("model").is_none());
    assert!(doc.get("agent").is_none(), "empty agent table removed");
}

#[test]
fn opencode_reset_keeps_a_user_chosen_non_9router_model() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".config").join("opencode").join("opencode.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        r#"{"model":"anthropic/claude","provider":{"9router":{"options":{"baseURL":"http://x/v1"}}}}"#,
    )
    .unwrap();
    find(home.path(), "opencode").reset(home.path()).unwrap();
    let doc = read_json(&file);
    assert_eq!(doc["model"], "anthropic/claude", "user's own model kept");
    assert!(doc["provider"].get("9router").is_none());
}

#[test]
fn cline_writes_origin_and_secrets_separately() {
    let home = tempfile::tempdir().unwrap();
    let tool = find(home.path(), "cline");
    let files = tool.apply(home.path(), &ctx(Some("cline/act-model"))).unwrap();
    assert_eq!(files.len(), 2, "state + secrets");
    let state = read_json(&home.path().join(".cline/data/globalState.json"));
    let secrets = read_json(&home.path().join(".cline/data/secrets.json"));
    // Cline's openAiBaseUrl must not carry /v1.
    assert_eq!(state["openAiBaseUrl"], "http://10.0.0.5:20128");
    assert_eq!(state["actModeApiProvider"], "openai");
    assert_eq!(state["planModeApiProvider"], "openai");
    assert_eq!(state["openAiModelId"], "cline/act-model");
    assert_eq!(state["planModeOpenAiModelId"], "cline/act-model");
    assert_eq!(secrets["openAiApiKey"], "sk-live-abcdef1234567890");

    tool.reset(home.path()).unwrap();
    let state = read_json(&home.path().join(".cline/data/globalState.json"));
    let secrets = read_json(&home.path().join(".cline/data/secrets.json"));
    assert_eq!(state["actModeApiProvider"], "cline");
    assert!(state.get("openAiBaseUrl").is_none());
    assert!(secrets.get("openAiApiKey").is_none());
}

#[test]
fn omp_yaml_roundtrip() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".omp").join("agent").join("models.yml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "providers:\n  other:\n    apiKey: keep-me\n").unwrap();

    find(home.path(), "omp").apply(home.path(), &ctx(None)).unwrap();
    let doc: Value = serde_yaml::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(doc["providers"]["9router"]["api"], "openai-completions");
    assert_eq!(doc["providers"]["9router"]["authHeader"], true);
    assert_eq!(doc["providers"]["9router"]["discovery"]["type"], "proxy");
    assert_eq!(doc["providers"]["other"]["apiKey"], "keep-me", "unrelated provider kept");

    find(home.path(), "omp").reset(home.path()).unwrap();
    let doc: Value = serde_yaml::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert!(doc["providers"].get("9router").is_none());
    assert_eq!(doc["providers"]["other"]["apiKey"], "keep-me");
}

#[test]
fn apply_backs_up_user_file_once() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".factory").join("settings.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, r#"{"customModels":[],"userSetting":true}"#).unwrap();

    find(home.path(), "droid").apply(home.path(), &ctx(Some("droid/m"))).unwrap();
    let backup = home.path().join(".factory").join("settings.json.bak-9router");
    assert!(backup.exists(), "pre-9router backup created");
    assert_eq!(fs::read_to_string(&backup).unwrap(), r#"{"customModels":[],"userSetting":true}"#);

    // A second apply must not overwrite the backup with 9router's own output.
    find(home.path(), "droid").apply(home.path(), &ctx(Some("droid/m2"))).unwrap();
    assert_eq!(fs::read_to_string(&backup).unwrap(), r#"{"customModels":[],"userSetting":true}"#);
}

#[test]
fn resolve_tools_matches_js_ids_and_aliases() {
    // Same order and ids as the JS TOOLS array.
    assert_eq!(
        tool_ids(),
        vec!["claude", "codex", "opencode", "droid", "crush", "kilo", "cline", "pi", "omp"]
    );
    let aliases = [
        ("claude-code", "claude"),
        ("claudecode", "claude"),
        ("factory", "droid"),
        ("kilocode", "kilo"),
        ("pi-coding-agent", "pi"),
        ("oh-my-pi", "omp"),
    ];
    for (alias, real) in aliases {
        let t = connect_tools::resolve_tools(&[alias.into()]).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].id(), real, "alias {alias}");
    }
    // Unknown names fail fast, before any network call.
    assert!(connect_tools::resolve_tools(&["bogus".into()]).is_err());
}

#[test]
fn show_info_configured_semantics() {
    assert!(!ShowInfo::default().is_configured());
    let only_key = ShowInfo { api_key: Some("k".into()), ..Default::default() };
    assert!(only_key.is_configured());
    let only_model = ShowInfo {
        models: BTreeMap::from([("main".into(), "m".into())]),
        ..Default::default()
    };
    assert!(only_model.is_configured());
}
