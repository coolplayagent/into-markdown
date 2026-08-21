//! Deterministic, context-free Han glyph normalization for transcript tokens.
//!
//! The embedded OpenCC character dictionaries are fixed at commit
//! `4f90418b9ed73a91023897095c762e5fdaadc016`. Only the first one-code-point
//! candidate is selected, so token timing and exact token/segment concatenation
//! remain valid. Phrase-level ambiguity is intentionally left to the transcript
//! text rather than merging or splitting recognized tokens.

use into_markdown_core::{Block, BlockNode, ChineseScript, Inline};
use std::collections::HashMap;
use std::sync::LazyLock;

const TRADITIONAL_TO_SIMPLIFIED: &str =
    include_str!("../../../third_party/opencc/TSCharacters.txt");
const SIMPLIFIED_TO_TRADITIONAL: &str =
    include_str!("../../../third_party/opencc/STCharacters.txt");

static TO_SIMPLIFIED: LazyLock<HashMap<char, char>> =
    LazyLock::new(|| parse_character_dictionary(TRADITIONAL_TO_SIMPLIFIED));
static TO_TRADITIONAL: LazyLock<HashMap<char, char>> =
    LazyLock::new(|| parse_character_dictionary(SIMPLIFIED_TO_TRADITIONAL));

fn parse_character_dictionary(dictionary: &str) -> HashMap<char, char> {
    dictionary
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (source, candidates) = line.split_once('\t')?;
            let source = one_character(source)?;
            let target = one_character(candidates.split_ascii_whitespace().next()?)?;
            Some((source, target))
        })
        .collect()
}

fn one_character(value: &str) -> Option<char> {
    let mut characters = value.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

fn normalize_text(value: &str, script: ChineseScript) -> String {
    let map = match script {
        ChineseScript::Preserve => return value.to_owned(),
        ChineseScript::Simplified => &*TO_SIMPLIFIED,
        ChineseScript::Traditional => &*TO_TRADITIONAL,
    };
    value.chars().map(|character| map.get(&character).copied().unwrap_or(character)).collect()
}

pub(crate) fn normalize_segment(node: &mut BlockNode, script: ChineseScript) {
    let Block::TimedSegment { tokens, content, .. } = &mut node.block else { return };
    for token in tokens {
        token.text = normalize_text(&token.text, script);
    }
    for inline in content {
        if let Inline::Text { value, .. } = inline {
            *value = normalize_text(value, script);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn loads_the_complete_fixed_character_tables() {
        assert_eq!(TO_SIMPLIFIED.len(), 4_148);
        assert_eq!(TO_TRADITIONAL.len(), 4_012);
        assert_eq!(
            format!("{:x}", Sha256::digest(TRADITIONAL_TO_SIMPLIFIED.as_bytes())),
            "737c21c66f55a419dd6956cb3089476cdefc5a36877452631617696df1e5d925"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(SIMPLIFIED_TO_TRADITIONAL.as_bytes())),
            "a0ca1601c70648cf48b33c3c6210ccbecc5c7eead4b4c3daf76587ba2c03582b"
        );
        assert_eq!(normalize_text("㑯", ChineseScript::Simplified), "㑔");
        assert_eq!(normalize_text("㑔", ChineseScript::Traditional), "㑯");
    }

    #[test]
    fn normalizes_common_meeting_text_in_both_directions() {
        assert_eq!(
            normalize_text("他現在在開會，講個話。", ChineseScript::Simplified),
            "他现在在开会，讲个话。"
        );
        assert_eq!(
            normalize_text("他现在在开会，讲个话。", ChineseScript::Traditional),
            "他現在在開會，講個話。"
        );
    }

    #[test]
    fn preserves_unmapped_text_and_code_point_count() {
        let input = "Speaker 1：你好🙂";
        assert_eq!(normalize_text(input, ChineseScript::Simplified), input);
        assert_eq!(normalize_text(input, ChineseScript::Traditional), input);
    }
}
