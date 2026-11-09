//! Port of open-sse/utils/tagBlocks.js.

/// Every `open ... close` block in `text`, in order, found by scanning forward once.
///
/// The JS callers use a single lazy regex `<tag>\s*([\s\S]*?)\s*<\/tag>`; a long run of
/// spaces or unclosed tags makes that quadratic, and the text here is upstream-model
/// controlled. We scan forward once instead, which is linear. An opening tag with no
/// closing tag after it ends the scan: no later opening tag can be closed either.
///
/// `open`/`close` are literal tag strings (e.g. "").
pub fn find_tag_blocks(text: &str, open_tag: &str, close_tag: &str) -> Vec<TagBlock> {
    let mut blocks = Vec::new();
    let mut position = 0;
    loop {
        let Some(open_start) = text[position..].find(open_tag) else { break };
        let open_start = position + open_start;
        let inner_start = open_start + open_tag.len();
        let Some(close_start) = text[inner_start..].find(close_tag) else { break };
        let close_start = inner_start + close_start;
        let end = close_start + close_tag.len();
        blocks.push(TagBlock {
            start: open_start,
            end,
            inner: text[inner_start..close_start].to_string(),
        });
        position = end;
    }
    blocks
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagBlock {
    /// Byte offset of the opening tag.
    pub start: usize,
    /// Byte offset just past the closing tag.
    pub end: usize,
    /// Text between the tags, with no surrounding-whitespace trimming (JS has none).
    pub inner: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_blocks_in_order() {
        let text = "a<think>x</think>b<think> y </think>c";
        let blocks = find_tag_blocks(text, "<think>", "</think>");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].inner, "x");
        assert_eq!(blocks[1].inner, " y ");
        assert_eq!(&text[blocks[0].start..blocks[0].end], "<think>x</think>");
        assert_eq!(&text[blocks[1].start..blocks[1].end], "<think> y </think>");
    }

    #[test]
    fn unclosed_tag_ends_scan() {
        // No later opening tag can be closed either — stop after the last closable one.
        assert!(find_tag_blocks("x<think>no close", "<think>", "</think>").is_empty());
        let text = "<think>a</think><think>b";
        let blocks = find_tag_blocks(text, "<think>", "</think>");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].inner, "a");
    }

    #[test]
    fn multiple_bytes_offsets_are_char_boundaries() {
        let text = "héllo<think>wörld</think>";
        let blocks = find_tag_blocks(text, "<think>", "</think>");
        assert_eq!(blocks[0].inner, "wörld");
    }
}
