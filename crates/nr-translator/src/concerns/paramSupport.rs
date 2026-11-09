//! Strip request params a provider/model has to reject upstream (e.g. HTTP 400).
//! Port of open-sse/translator/concerns/paramSupport.js.
//! Config-driven: add a rule instead of scattering `delete body.x` across executors.

use std::sync::LazyLock;

use nr_core::Json;
use regex::Regex;
use serde_json::json;

/// How a rule decides whether it applies to a model id.
enum Match {
    Any,
    Regex(Regex),
    /// GitHub Copilot Claude (except opus/sonnet 4.6): thinking + reasoning_effort rejected. #713
    GithubClaudeButNot46,
}

#[derive(Default)]
struct Rule {
    provider: Option<&'static str>,
    matcher: Option<Match>,
    drop: &'static [&'static str],
    rename: &'static [(&'static str, &'static str)],
    drop_message_fields: &'static [&'static str],
    rename_message_fields: &'static [(&'static str, &'static str)],
    flatten_content: bool,
    clamp_to_model_max_output: bool,
    max_output_cap: Option<i64>,
}

impl Rule {
    fn new(matcher: Match) -> Self {
        Self { matcher: Some(matcher), ..Default::default() }
    }

    fn provider(mut self, provider: &'static str) -> Self {
        self.provider = Some(provider);
        self
    }

    fn drop(mut self, keys: &'static [&'static str]) -> Self {
        self.drop = keys;
        self
    }

    fn rename(mut self, pairs: &'static [(&'static str, &'static str)]) -> Self {
        self.rename = pairs;
        self
    }

    fn drop_message_fields(mut self, keys: &'static [&'static str]) -> Self {
        self.drop_message_fields = keys;
        self
    }

    fn rename_message_fields(mut self, pairs: &'static [(&'static str, &'static str)]) -> Self {
        self.rename_message_fields = pairs;
        self
    }

    fn clamp_to_model_max_output(mut self) -> Self {
        self.clamp_to_model_max_output = true;
        self
    }

    fn max_output_cap(mut self, cap: i64) -> Self {
        self.max_output_cap = Some(cap);
        self
    }
}

const REASONING_FIELDS: &[&str] = &["reasoning_content", "reasoning", "reasoning_details"];

/// Each rule: optional provider, model matcher, and what to drop/rename.
fn rules() -> &'static [Rule] {
    static RULES: LazyLock<Vec<Rule>> = LazyLock::new(build_rules);
    RULES.as_slice()
}

fn build_rules() -> Vec<Rule> {
    let re = |pattern: &str| Match::Regex(Regex::new(pattern).expect("paramSupport regex"));
    vec![
        // All Claude models: temperature deprecated/rejected upstream (Anthropic 400). #1748
        Rule::new(re("claude")).drop(&["temperature"]),
        // GitHub Copilot gpt-5.4: temperature unsupported.
        Rule::new(re("gpt-5\\.4")).provider("github").drop(&["temperature"]),
        // GitHub Copilot Claude (except opus/sonnet 4.6): thinking + reasoning_effort rejected. #713
        Rule::new(Match::GithubClaudeButNot46)
            .provider("github")
            .drop(&["thinking", "reasoning_effort"]),
        // Cloudflare Workers AI: content must be plain string, rejects OpenAI content-part array (#1926)
        Rule { flatten_content: true, ..Rule::new(Match::Any).provider("cloudflare-ai") },
        Rule::new(re("glm-5"))
            .provider("volcengine-ark")
            .clamp_to_model_max_output(),
        // VolcEngine Ark caps the Kimi family at max_tokens <= 32768, but the model's
        // advertised ceiling is far higher (Kimi-K2.7-Code resolves to maxOutput 262144),
        // so clampToModelMaxOutput alone leaves it uncapped and the request 400s with
        // "integer above maximum value, expected <= 32768".
        Rule::new(re("kimi"))
            .provider("volcengine-ark")
            .max_output_cap(32768)
            .clamp_to_model_max_output(),
        // Strict OpenAI-compatible validators reject unknown assistant-message fields.
        // Groq answers 400 and Mistral 422 ("extra_forbidden") on replayed reasoning.
        Rule::new(Match::Any).provider("groq").drop_message_fields(REASONING_FIELDS),
        Rule::new(Match::Any).provider("mistral").drop_message_fields(REASONING_FIELDS),
        Rule::new(Match::Any).provider("cerebras").drop_message_fields(REASONING_FIELDS),
        // NVIDIA NIM treats `reasoning` and `reasoning_content` as aliases for one
        // field; sending both makes its deserializer report a duplicate field.
        Rule::new(Match::Any)
            .provider("nvidia")
            .rename_message_fields(&[("reasoning", "reasoning_content")]),
        // OpenCode Go's relay rejects `response_format` for its deepseek family with
        // HTTP 400 — both json_schema and json_object — while other families accept it.
        Rule::new(re("^deepseek"))
            .provider("opencode-go")
            .drop(&["response_format"]),
        // Current OpenAI models (gpt-5.x / gpt-6.x) reject `max_tokens` with HTTP 400
        // ("Use 'max_completion_tokens' instead"). Clients send `max_tokens`, so rename it.
        Rule::new(Match::Any)
            .provider("openai")
            .rename(&[("max_tokens", "max_completion_tokens")]),
    ]
}

impl Match {
    fn is_match(&self, model: &str) -> bool {
        match self {
            Match::Any => true,
            Match::Regex(regex) => regex.is_match(model),
            Match::GithubClaudeButNot46 => {
                static CLAUDE: LazyLock<Regex> = LazyLock::new(|| Regex::new("claude").unwrap());
                static V46: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"claude.*(opus|sonnet).*4\.6").unwrap());
                CLAUDE.is_match(model) && !V46.is_match(model)
            }
        }
    }
}

fn num(body: &Json, key: &str) -> Option<f64> {
    body.get(key).and_then(|v| v.as_f64())
}

fn clamp_number(body: &mut Json, key: &str, ceiling: f64) {
    if let Some(v) = num(body, key) {
        if v > ceiling {
            body[key] = json!(ceiling);
        }
    }
}

fn is_blank(v: Option<&Json>) -> bool {
    match v {
        None | Some(Json::Null) => true,
        Some(Json::String(s)) => s.trim().is_empty(),
        _ => false,
    }
}

/// Remove unsupported params from body in place.
pub fn strip_unsupported_params(provider: &str, model: &str, body: &mut Json) {
    if model.is_empty() || !body.is_object() {
        return;
    }
    let caps_cache: std::cell::OnceCell<Json> = std::cell::OnceCell::new();

    for rule in rules() {
        if let Some(p) = rule.provider {
            if p != provider {
                continue;
            }
        }
        if !rule.matcher.as_ref().map(|m| m.is_match(model)).unwrap_or(true) {
            continue;
        }

        // Rename params (keeps an explicit target value if the client already sent one).
        for (from, to) in rule.rename {
            let Some(value) = body.get(*from).cloned() else { continue };
            if body.get(*to).is_none() {
                body[*to] = value;
            }
            if let Some(obj) = body.as_object_mut() {
                obj.remove(*from);
            }
        }

        for key in rule.drop {
            if let Some(obj) = body.as_object_mut() {
                obj.remove(*key);
            }
        }

        // Normalize and drop assistant-message fields (clients replay reasoning here).
        if (!rule.rename_message_fields.is_empty() || !rule.drop_message_fields.is_empty())
            && body.get("messages").map(|m| m.is_array()).unwrap_or(false)
        {
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for msg in messages.iter_mut() {
                    if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                        continue;
                    }
                    for (from, to) in rule.rename_message_fields {
                        let Some(value) = msg.get(*from).cloned() else { continue };
                        if is_blank(msg.get(*to)) {
                            msg[*to] = value;
                        }
                        if let Some(obj) = msg.as_object_mut() {
                            obj.remove(*from);
                        }
                    }
                    for key in rule.drop_message_fields {
                        if let Some(obj) = msg.as_object_mut() {
                            obj.remove(*key);
                        }
                    }
                }
            }
        }

        // CF Workers AI oneOf root schema only accepts content as plain string (#1926)
        if rule.flatten_content {
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for msg in messages.iter_mut() {
                    if !msg.get("content").map(|c| c.is_array()).unwrap_or(false) {
                        continue;
                    }
                    let flattened: String = msg["content"]
                        .as_array()
                        .map(|blocks| {
                            blocks
                                .iter()
                                .map(|b| {
                                    if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                                        b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string()
                                    } else {
                                        String::new()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    msg["content"] = json!(flattened);
                }
            }
        }

        if rule.clamp_to_model_max_output || rule.max_output_cap.is_some() {
            let caps = caps_cache.get_or_init(|| {
                nr_data::capabilities::get_capabilities_for_model(Some(provider), Some(model))
            });
            let mut candidates: Vec<f64> = Vec::new();
            if rule.clamp_to_model_max_output {
                if let Some(ceiling) = caps.get("maxOutput").and_then(|v| v.as_f64()) {
                    if ceiling > 0.0 {
                        candidates.push(ceiling);
                    }
                }
            }
            if let Some(cap) = rule.max_output_cap {
                if cap > 0 {
                    candidates.push(cap as f64);
                }
            }
            if let Some(ceiling) = candidates.into_iter().reduce(f64::min) {
                for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                    clamp_number(body, key, ceiling);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_unsupported_params_noop() {
        let mut body = json!({"model": "gpt-4", "temperature": 0.7});
        strip_unsupported_params("openai", "gpt-4", &mut body);
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn test_claude_drops_temperature() {
        let mut body = json!({"model": "claude-sonnet-4-5", "temperature": 0.7, "max_tokens": 10});
        strip_unsupported_params("claude", "claude-sonnet-4-5", &mut body);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["max_tokens"], 10);
    }

    #[test]
    fn test_github_copilot_claude_drops_thinking() {
        let mut body = json!({"thinking": {"type": "enabled"}, "reasoning_effort": "high"});
        strip_unsupported_params("github", "claude-sonnet-4-5", &mut body);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_github_copilot_claude_46_keeps_thinking() {
        let mut body = json!({"thinking": {"type": "enabled"}});
        strip_unsupported_params("github", "claude-opus-4.6", &mut body);
        assert!(body.get("thinking").is_some());
    }

    #[test]
    fn test_cloudflare_flattens_content() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "a"},
                {"type": "image_url", "image_url": {"url": "x"}},
                {"type": "text", "text": "b"}
            ]}
        ]});
        strip_unsupported_params("cloudflare-ai", "@cf/meta/llama", &mut body);
        assert_eq!(body["messages"][0]["content"], "ab");
    }

    #[test]
    fn test_openai_renames_max_tokens() {
        let mut body = json!({"max_tokens": 100});
        strip_unsupported_params("openai", "gpt-5.2", &mut body);
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn test_openai_keeps_explicit_target() {
        let mut body = json!({"max_tokens": 100, "max_completion_tokens": 50});
        strip_unsupported_params("openai", "gpt-5.2", &mut body);
        assert_eq!(body["max_completion_tokens"], 50);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn test_groq_drops_reasoning_fields() {
        let mut body = json!({"messages": [
            {"role": "assistant", "content": "x", "reasoning_content": "why"},
            {"role": "user", "content": "y"}
        ]});
        strip_unsupported_params("groq", "llama-3", &mut body);
        assert!(body["messages"][0].get("reasoning_content").is_none());
        assert_eq!(body["messages"][0]["content"], "x");
    }

    #[test]
    fn test_nvidia_renames_reasoning() {
        let mut body = json!({"messages": [{"role": "assistant", "reasoning": "why"}]});
        strip_unsupported_params("nvidia", "nvidia/llama", &mut body);
        assert_eq!(body["messages"][0]["reasoning_content"], "why");
        assert!(body["messages"][0].get("reasoning").is_none());
    }

    #[test]
    fn test_nvidia_keeps_existing_reasoning_content() {
        let mut body = json!({"messages": [
            {"role": "assistant", "reasoning": "old", "reasoning_content": "keep"}
        ]});
        strip_unsupported_params("nvidia", "nvidia/llama", &mut body);
        assert_eq!(body["messages"][0]["reasoning_content"], "keep");
        assert!(body["messages"][0].get("reasoning").is_none());
    }

    #[test]
    fn test_opencode_go_deepseek_drops_response_format() {
        let mut body = json!({"response_format": {"type": "json_object"}});
        strip_unsupported_params("opencode-go", "deepseek-v4.1-flash", &mut body);
        assert!(body.get("response_format").is_none());

        let mut other = json!({"response_format": {"type": "json_object"}});
        strip_unsupported_params("opencode-go", "glm-5", &mut other);
        assert!(other.get("response_format").is_some());
    }

    #[test]
    fn test_volcengine_clamps_kimi_max_tokens() {
        let mut body = json!({"max_tokens": 200000});
        strip_unsupported_params("volcengine-ark", "kimi-k2.7-code", &mut body);
        let clamped = body["max_tokens"].as_f64().unwrap();
        assert!(clamped <= 32768.0, "clamped {clamped}");
    }
}
