//! Shared Hegel generators for formatting tests.
//!
//! General string generators are unlikely to spontaneously generate emoji
//! ZWJ sequences, skin-tone modifiers, or flag sequences (they need specific
//! codepoints in specific orders).  This module provides an atom-based
//! generator that exercises those hard-to-handle grapheme clusters.

use hegel::prelude::*;

/// Adversarial Unicode atoms with no ASCII whitespace or word characters.
const ADVERSARIAL_ATOMS: &[&str] = &[
    // Family emoji (ZWJ sequence, 7 codepoints, 1 grapheme).
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}",
    // Couple emoji with variation selector.
    "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}",
    // Waving hand with skin-tone modifier.
    "\u{1F44B}\u{1F3FD}",
    // Flag sequence (two regional indicator symbols).
    "\u{1F1EF}\u{1F1F5}",
    // Plain multi-byte base emoji.
    "\u{1F389}",
    // Combining accent (e + combining acute).
    "e\u{0301}",
];

/// Generate strings composed entirely of adversarial Unicode atoms with
/// no ASCII whitespace or word characters.
///
/// Used to force truncation / hard-split paths to land inside a
/// grapheme-cluster-rich region instead of falling back to a word
/// boundary.  Atoms include emoji ZWJ sequences, skin-tone modifiers,
/// flag sequences, and combining accents — the Unicode shapes most
/// likely to expose bugs in codepoint-vs-grapheme splitting logic.
pub(crate) fn adversarial_unicode_no_spaces(max_atoms: usize) -> impl PrintableGenerator<String> {
    gs::vecs(gs::sampled_from(ADVERSARIAL_ATOMS))
        .min_size(1)
        .max_size(max_atoms)
        .map(|atoms: Vec<&str>| atoms.concat())
}

/// Generate a string that contains one grapheme cluster larger than
/// `min_cluster_bytes`.
///
/// [`adversarial_unicode_no_spaces`] joins many small clusters. It never builds
/// a single cluster that is larger than a line budget. A splitter can therefore
/// pass that generator and still fail on one oversized cluster, because the
/// "first grapheme does not fit" path is never reached.
///
/// A ZWJ sequence has no length limit. Any number of emoji that `U+200D` joins
/// is one grapheme cluster.
pub(crate) fn oversized_grapheme_cluster(
    min_cluster_bytes: usize,
) -> impl PrintableGenerator<String> {
    // "\u{1F468}\u{200D}" is 7 bytes, so this gives the requested size.
    let joins = min_cluster_bytes / 7 + 2;
    hegel::tuples!(
        gs::integers::<usize>().min_value(1).max_value(joins),
        gs::integers::<usize>().max_value(3),
    )
    .map(|(n, pad)| {
        let mut cluster: String = "\u{1F468}\u{200D}".repeat(n);
        cluster.push('\u{1F468}');
        // Pad with plain text so the cluster is not always at offset 0.
        format!("{}{cluster}", "a".repeat(pad))
    })
}

/// Generate text with no characters from Unicode general category C
/// (control, format, surrogate, private use, and unassigned). This is the
/// `\PC` class, which the Hegel regex syntax does not support.
pub(crate) fn non_control_text(
    min_size: usize,
    max_size: usize,
) -> impl PrintableGenerator<String> {
    gs::text()
        .exclude_categories(&["C"])
        .min_size(min_size)
        .max_size(max_size)
}
