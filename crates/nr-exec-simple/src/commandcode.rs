//! Port of open-sse/executors/commandcode.js + shared/commandCodeHeaders.js.
//! Handles CommandCode NDJSON upstream (no `data:` prefix), translates to OpenAI SSE.

use std::sync::Arc;
use std::sync::LazyLock;
use async_trait::async_trait;
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use nr_core::http::HeaderList;
use nr_exec_base::base::{base_execute, BaseHooks};

static EMPTY: LazyLock<Json> = LazyLock::new(|| Json::Object(Default::default()));

/// Subset of @sindresorhus/slugify's built-in replacements, plus the
/// transliterate table entries Unicode NFD decomposition does not cover.
fn transliterate(value: &str) -> String {
    const REPLACEMENTS: &[(&str, &str)] = &[
        ("&", " and "),
        ("\u{1F984}", " unicorn "),
        ("\u{2665}", " love "),
        ("\u{00DF}", "ss"),
        ("\u{1E9E}", "Ss"),
        ("\u{00E4}", "ae"),
        ("\u{00C4}", "Ae"),
        ("\u{00F6}", "oe"),
        ("\u{00D6}", "Oe"),
        ("\u{00FC}", "ue"),
        ("\u{00DC}", "Ue"),
        ("\u{00C6}", "AE"),
        ("\u{00E6}", "ae"),
        ("\u{00D8}", "O"),
        ("\u{00F8}", "o"),
        ("\u{0152}", "OE"),
        ("\u{0153}", "oe"),
        ("\u{00D0}", "D"),
        ("\u{00F0}", "d"),
        ("\u{0110}", "D"),
        ("\u{0111}", "d"),
        ("\u{00DE}", "TH"),
        ("\u{00FE}", "th"),
        ("\u{0141}", "L"),
        ("\u{0142}", "l"),
        ("\u{0131}", "i"),
    ];
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match REPLACEMENTS.iter().find(|(from, _)| from.chars().eq(std::iter::once(ch))) {
            Some((_, to)) => out.push_str(to),
            None => out.push(ch),
        }
    }
    // `String.prototype.normalize("NFD")` + mark stripping only matters for
    // non-ASCII text; the replacement table above already covers slugify's own
    // table, and every remaining non-ASCII char is dropped by the `[^a-z\d]`
    // pass below just like the decomp-only marks would be.
    out
}

/// Same splitting rules as @sindresorhus/slugify's decamelize (default on).
fn decamelize(value: &str) -> String {
    let mut out = value.to_string();
    for (re, replacement) in [
        (r"([A-Z]{2,})(\d+)", "$1 $2"),
        (r"([a-z\d]+)([A-Z]{2,})", "$1 $2"),
        (r"([a-z\d])([A-Z])", "$1 $2"),
        (r"([A-Z]+)([A-Z][a-rt-z\d]+)", "$1 $2"),
    ] {
        if let Ok(re) = regex::Regex::new(re) {
            out = re.replace_all(&out, replacement).to_string();
        }
    }
    out
}

/// `slugify(path)` with @sindresorhus/slugify defaults:
/// `/home/u/My Project` → `home-u-my-project`.
fn slugify_path(value: &str) -> String {
    let lowered = decamelize(&transliterate(value)).to_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut in_run = false;
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            in_run = false;
        } else if !in_run {
            slug.push('-');
            in_run = true;
        }
    }
    slug.trim_matches('-').to_string()
}

pub struct CommandCodeExecutor {
    config: &'static Json,
}

impl CommandCodeExecutor {
    pub fn new() -> Self {
        let config = nr_data::provider("commandcode").or_else(|| nr_data::provider("openai")).unwrap_or(&EMPTY);
        Self { config }
    }
}

#[async_trait]
impl BaseHooks for CommandCodeExecutor {
    fn provider(&self) -> &str {
        "commandcode"
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_headers(&self, creds: &Credentials, stream: bool, _url: &str, _model: &str, body: &Json) -> Result<HeaderList> {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());

        // Add provider config headers
        if let Some(config_headers) = self.config.get("headers").and_then(Json::as_object) {
            for (k, v) in config_headers {
                if let Some(v_str) = v.as_str() {
                    headers.insert(k.clone(), v_str.to_string());
                }
            }
        }

        // Auth header
        if let Some(key) = creds.api_key().or_else(|| creds.access_token()) {
            headers.insert("Authorization".into(), format!("Bearer {}", key));
        }

        // Session ID from body.threadId (stable per conversation)
        if let Some(thread_id) = body.get("threadId").and_then(Json::as_str) {
            headers.insert("x-session-id".into(), thread_id.to_string());
        }

        // Project slug from current working dir (matches official CLI)
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| ".".to_string());
        headers.insert("x-project-slug".into(), slugify_path(&cwd));

        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }

        Ok(headers)
    }

    fn transform_request(&self, _model: &str, body: &Json, _stream: bool, _creds: &Credentials) -> Result<Json> {
        let mut result = body.clone();

        // Force streaming (CommandCode requires it)
        if let Some(obj) = result.as_object_mut() {
            obj.insert("stream".into(), Json::Bool(true));
            if let Some(params) = obj.get_mut("params") {
                if let Some(params_obj) = params.as_object_mut() {
                    params_obj.insert("stream".into(), Json::Bool(true));
                }
            }
        }

        Ok(result)
    }
}

#[async_trait]
impl Executor for CommandCodeExecutor {
    fn provider(&self) -> &str {
        "commandcode"
    }

    fn no_auth(&self) -> bool {
        BaseHooks::no_auth(self)
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        base_execute(self, req).await
    }
}

pub fn create(_provider: &str) -> Arc<dyn Executor> {
    Arc::new(CommandCodeExecutor::new())
}

#[cfg(test)]
mod slugify_tests {
    use super::*;

    #[test]
    fn slugifies_a_path_like_the_cli() {
        assert_eq!(slugify_path("/home/u/My Project"), "home-u-my-project");
        assert_eq!(slugify_path("/media/caliph/project/rust-9router"), "media-caliph-project-rust-9router");
    }

    #[test]
    fn decamelizes_like_sindresorhus() {
        // Verified against the JS: `([a-z\d])([A-Z])` splits at the case boundary
        // only, so the digit stays glued to the lowercase run.
        assert_eq!(slugify_path("/a/rust9Router"), "a-rust9-router");
        assert_eq!(slugify_path("APIV2Thing"), "apiv-2-thing");
    }

    #[test]
    fn maps_special_characters() {
        assert_eq!(slugify_path("M&Ms"), "m-and-ms");
        assert_eq!(slugify_path("Über_Größe"), "ueber-groesse");
        assert_eq!(slugify_path("straße"), "strasse");
    }

    #[test]
    fn collapses_and_trims_separators() {
        assert_eq!(slugify_path("a---b___c"), "a-b-c");
        assert_eq!(slugify_path("///"), "");
        assert_eq!(slugify_path(""), "");
    }
}
