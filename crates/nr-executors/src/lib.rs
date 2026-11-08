//! Provider id → executor. Port of open-sse/executors/index.js.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use nr_core::Executor;
use nr_exec_base::default_executor;
use nr_exec_binary as binary;
use nr_exec_browser as browser;
use nr_exec_cli as cli;
use nr_exec_ide as ide;
use nr_exec_simple as simple;
use nr_exec_webchat as webchat;

type Ctor = fn(&str) -> Arc<dyn Executor>;

/// Specialized executors keyed by provider id or alias, with the canonical
/// provider id each instance is built for (JS: `cu: new CursorExecutor()`).
const SPECIALIZED: &[(&str, &str, Ctor)] = &[
    ("antigravity", "antigravity", ide::antigravity::create),
    ("azure", "azure", simple::azure::create),
    ("gemini-cli", "gemini-cli", simple::gemini_cli::create),
    ("github", "github", simple::github::create),
    ("iflow", "iflow", simple::iflow::create),
    ("qoder", "qoder", ide::qoder::create),
    ("qoder-cn", "qoder-cn", ide::qoder::create),
    ("kiro", "kiro", binary::kiro::create),
    ("kimchi", "kimchi", simple::kimchi::create),
    ("codex", "codex", simple::codex::create),
    ("cursor", "cursor", binary::cursor::create),
    ("cu", "cursor", binary::cursor::create),
    ("vertex", "vertex", simple::vertex::create),
    ("vertex-partner", "vertex-partner", simple::vertex::create),
    ("opencode", "opencode", simple::opencode::create),
    ("opencode-go", "opencode-go", simple::opencode_go::create),
    ("opencode-zen", "opencode-zen", simple::opencode_zen::create),
    ("grok-web", "grok-web", browser::grok_web::create),
    ("grok-cli", "grok-cli", cli::grok_cli::create),
    ("gcli", "grok-cli", cli::grok_cli::create),
    ("gb", "grok-cli", cli::grok_cli::create),
    ("perplexity-web", "perplexity-web", webchat::perplexity_web::create),
    ("deepseek-web", "deepseek-web", webchat::deepseek_web::create),
    ("gemini-web", "gemini-web", browser::gemini_web::create),
    ("muse-web", "muse-web", browser::muse_web::create),
    ("veoaifree-web", "veoaifree-web", browser::veoaifree_web::create),
    ("veo-free", "veoaifree-web", browser::veoaifree_web::create),
    ("uc-web", "uc-web", webchat::uc_web::create),
    ("chatgpt-web", "chatgpt-web", webchat::chatgpt_web::create),
    ("ollama-local", "ollama-local", simple::ollama_local::create),
    ("commandcode", "commandcode", simple::commandcode::create),
    ("xiaomi-tokenplan", "xiaomi-tokenplan", simple::xiaomi_tokenplan::create),
    ("xiaomi-mimo", "xiaomi-mimo", simple::xiaomi_mimo::create),
    ("mimo-free", "mimo-free", simple::mimo_free::create),
    ("mmf", "mimo-free", simple::mimo_free::create),
    ("codebuddy-cn", "codebuddy-cn", simple::codebuddy_cn::create),
    ("codebuddy-intl", "codebuddy-intl", simple::codebuddy_intl::create),
    ("trae", "trae", ide::trae::create),
    ("zed", "zed", ide::zed::create),
    ("windsurf", "windsurf", ide::windsurf::create),
    ("minimax-code", "minimax-code", simple::minimax_code::create),
    ("minimax-code-global", "minimax-code-global", simple::minimax_code::create),
    ("devin-cli", "devin-cli", cli::devin_cli::create),
    ("bedrock", "bedrock", binary::bedrock::create),
    ("br", "bedrock", binary::bedrock::create),
    ("bedrock-xai", "bedrock-xai", binary::bedrock::create),
    ("brx", "bedrock-xai", binary::bedrock::create),
    ("perchai", "perchai", cli::perchai::create),
    ("perch", "perchai", cli::perchai::create),
];

static CACHE: LazyLock<Mutex<HashMap<String, Arc<dyn Executor>>>> = LazyLock::new(Default::default);

/// JS `getExecutor(provider)`: the specialized executor, else a cached `DefaultExecutor`.
pub fn get_executor(provider: &str) -> Arc<dyn Executor> {
    let mut cache = CACHE.lock().expect("executor cache poisoned");
    if let Some(e) = cache.get(provider) {
        return e.clone();
    }
    let exec = match SPECIALIZED.iter().find(|(id, _, _)| *id == provider) {
        Some((_, canonical, ctor)) => ctor(canonical),
        None => default_executor::create(provider),
    };
    cache.insert(provider.to_string(), exec.clone());
    exec
}

/// JS `hasSpecializedExecutor(provider)`.
pub fn has_specialized_executor(provider: &str) -> bool {
    SPECIALIZED.iter().any(|(id, _, _)| *id == provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_canonical_provider() {
        assert_eq!(get_executor("cu").provider(), "cursor");
        assert_eq!(get_executor("brx").provider(), "bedrock-xai");
        assert_eq!(get_executor("some-openai-compatible").provider(), "some-openai-compatible");
        assert!(has_specialized_executor("kiro"));
        assert!(!has_specialized_executor("openai"));
    }
}
