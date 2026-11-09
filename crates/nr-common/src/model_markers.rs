//! Port of open-sse/utils/modelMarkers.js.
//!
//! Claude Code appends a `[1m]` context marker to the model name when the 1M-context
//! beta is toggled on. The marker is a client-side annotation, not part of any model
//! id: it never matches a combo name, an alias or a `provider/model` pair, so a request
//! carrying it dies at model resolution with "Invalid model format". The capability
//! itself travels in the `anthropic-beta: context-1m-2025-08-07` header, which is
//! forwarded untouched — stripping the marker is enough to route normally.

use serde::{Deserialize, Serialize};

const CONTEXT_MARKER: &str = "[1m]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrippedModel<'a> {
    pub model: &'a str,
    /// Lowercased marker body (`"1m"`) or `None` when absent — mirrors JS `null`.
    pub context_marker: Option<String>,
}

/// `stripModelContextMarker`. Non-string input cannot occur in Rust, so the JS
/// `typeof !== "string"` passthrough branch is unreachable here.
pub fn strip_model_context_marker(model: &str) -> StrippedModel<'_> {
    let trimmed = model.trim();
    // Case-insensitive match on the trailing `[1m]`.
    if trimmed.len() >= CONTEXT_MARKER.len()
        && trimmed[trimmed.len() - CONTEXT_MARKER.len()..].eq_ignore_ascii_case(CONTEXT_MARKER)
    {
        return StrippedModel {
            model: &trimmed[..trimmed.len() - CONTEXT_MARKER.len()],
            context_marker: Some(CONTEXT_MARKER[1..CONTEXT_MARKER.len() - 1].to_lowercase()),
        };
    }
    StrippedModel {
        model,
        context_marker: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_marker() {
        let r = strip_model_context_marker("claude-opus-5[1m]");
        assert_eq!(r.model, "claude-opus-5");
        assert_eq!(r.context_marker.as_deref(), Some("1m"));
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            strip_model_context_marker("claude-opus-5[1M]").context_marker.as_deref(),
            Some("1m")
        );
    }

    #[test]
    fn trims_surrounding_space() {
        assert_eq!(strip_model_context_marker("  claude-opus-5[1m] ").model, "claude-opus-5");
    }

    #[test]
    fn no_marker_passthrough() {
        let r = strip_model_context_marker("claude-opus-5");
        assert_eq!(r.model, "claude-opus-5");
        assert_eq!(r.context_marker, None);
    }
}
