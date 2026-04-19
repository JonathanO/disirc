//! Shared proptest strategies for formatting tests.
//!
//! Proptest's regex-based string strategies are unlikely to spontaneously
//! generate emoji ZWJ sequences, skin-tone modifiers, or flag sequences
//! (they need specific codepoints in specific orders).  This module
//! provides a weighted atom-based generator that exercises those
//! hard-to-handle grapheme clusters alongside ordinary text.

use proptest::prelude::*;

/// Generate strings composed entirely of adversarial Unicode atoms with
/// no ASCII whitespace or word characters.
///
/// Used to force truncation / hard-split paths to land inside a
/// grapheme-cluster-rich region instead of falling back to a word
/// boundary.  Atoms include emoji ZWJ sequences, skin-tone modifiers,
/// flag sequences, and combining accents — the Unicode shapes most
/// likely to expose bugs in codepoint-vs-grapheme splitting logic.
pub(crate) fn adversarial_unicode_no_spaces(max_atoms: usize) -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        // Family emoji (ZWJ sequence, 7 codepoints, 1 grapheme).
        3 => Just("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}".to_owned()),
        // Couple emoji with variation selector.
        2 => Just("\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}".to_owned()),
        // Waving hand with skin-tone modifier.
        2 => Just("\u{1F44B}\u{1F3FD}".to_owned()),
        // Flag sequence (two regional indicator symbols).
        2 => Just("\u{1F1EF}\u{1F1F5}".to_owned()),
        // Plain multi-byte base emoji.
        2 => Just("\u{1F389}".to_owned()),
        // Combining accent (e + combining acute).
        1 => Just("e\u{0301}".to_owned()),
    ];
    prop::collection::vec(atom, 1..=max_atoms).prop_map(|atoms| atoms.concat())
}
