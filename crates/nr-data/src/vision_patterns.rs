use std::sync::LazyLock;
use regex::Regex;

const SEP: &str = "[-_/:.]]";

// Image GENERATION, video generation, and non-chat models also carry these
// words but take no image input — checked first so they can never match.
static NOT_VISION: LazyLock<Regex> = LazyLock::new(|| {
    let patterns = [
        r"(^|[-_/:.])((image|img)([-_/:.]|$))",
        "stable-image",
        "gen[0-9]_image",
        "nanobanana",
        "imagine",
        "t2v",
        "i2v",
        "flux",
        "dall",
        "sdxl",
        "diffusion",
        "embed",
        "rerank",
        "guard",
        "moderation",
        "tts",
        "stt",
        "whisper",
        "voice",
        "speech",
        "audio",
    ];
    Regex::new(&format!("(?i)({})", patterns.join("|"))).unwrap()
});

// Explicit modality words, plus the "<digit>v" suffix vendors use for vision
// variants (glm-4.6v, glm-5v-turbo). The digit-v branch requires a dotted
// version so the never-shipped `gpt-4v` cannot match.
static VISION_NAME: LazyLock<Regex> = LazyLock::new(|| {
    let patterns = [
        r"(^|[-_/:.])((vision|vl|vlm|multimodal|omni|visual)([-_/:.]|$))",
        r"[0-9]\.[0-9]+v([-_/:.]|$)",
        r"(^|[-_/:.])(glm-[0-9]+v)([-_/:.]|$)",
        r"(^|[-_/:.])(llava|pixtral|internvl|cogvlm|minicpm-v|moondream|idefics|fuyu)",
    ];
    Regex::new(&format!("(?i)({})", patterns.join("|"))).unwrap()
});

/// Does this model id look like a vision model? Name signal only.
pub fn looks_like_vision_model(model_id: Option<&str>) -> bool {
    let id = match model_id {
        Some(s) => s,
        None => return false,
    };

    if id.is_empty() {
        return false;
    }

    let lower = id.to_lowercase();
    if NOT_VISION.is_match(&lower) {
        return false;
    }
    VISION_NAME.is_match(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vision_models_recognized() {
        assert!(looks_like_vision_model(Some("qwen3-vl-plus")));
        assert!(looks_like_vision_model(Some("glm-4.6v")));
        assert!(looks_like_vision_model(Some("deepseek-v4-flash-vision-exp")));
        assert!(looks_like_vision_model(Some("gpt-4-vision")));
        assert!(looks_like_vision_model(Some("llava")));
    }

    #[test]
    fn test_non_vision_excluded() {
        assert!(!looks_like_vision_model(Some("dall-e-3")));
        assert!(!looks_like_vision_model(Some("flux-1.0")));
        assert!(!looks_like_vision_model(Some("text-embedding-ada-002")));
        assert!(!looks_like_vision_model(Some("gpt-4")));
    }

    #[test]
    fn test_none_returns_false() {
        assert!(!looks_like_vision_model(None));
    }
}
