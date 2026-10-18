//! Extended grapheme clusters, the user-perceived characters of Unicode
//! Standard Annex #29: a letter and its accents, a whole emoji sequence, a
//! flag, or a Hangul syllable written as separate jamo.

use crate::unicode_tables::GRAPHEME_PROPERTIES;

/// Grapheme_Cluster_Break values, numbered as in the generated table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Break {
    Other,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    RegionalIndicator,
    Prepend,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
}

/// Indic_Conjunct_Break values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Conjunct {
    None,
    Linker,
    Consonant,
    Extend,
}

#[derive(Clone, Copy)]
struct Properties {
    kind: Break,
    pictographic: bool,
    conjunct: Conjunct,
}

fn properties(c: char) -> Properties {
    let code = c as u32;
    let value = GRAPHEME_PROPERTIES
        .binary_search_by(|&(first, last, _)| {
            if last < code {
                std::cmp::Ordering::Less
            } else if first > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .map(|index| GRAPHEME_PROPERTIES[index].2)
        .unwrap_or(0);
    let kind = match value & 0xF {
        1 => Break::Cr,
        2 => Break::Lf,
        3 => Break::Control,
        4 => Break::Extend,
        5 => Break::Zwj,
        6 => Break::RegionalIndicator,
        7 => Break::Prepend,
        8 => Break::SpacingMark,
        9 => Break::L,
        10 => Break::V,
        11 => Break::T,
        12 => Break::Lv,
        13 => Break::Lvt,
        _ => Break::Other,
    };
    let conjunct = match (value >> 5) & 0x3 {
        1 => Conjunct::Linker,
        2 => Conjunct::Consonant,
        3 => Conjunct::Extend,
        _ => Conjunct::None,
    };
    Properties {
        kind,
        pictographic: value & 0x10 != 0,
        conjunct,
    }
}

/// What the characters so far mean for the rules that look further back
/// than one character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Emoji {
    None,
    /// An Extended_Pictographic character, then Extend characters.
    Pictograph,
    /// The same, then a zero width joiner (rule GB11).
    Joined,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Indic {
    None,
    /// A conjunct consonant, then conjunct Extend characters.
    Consonant,
    /// The same with a conjunct Linker among them (rule GB9c).
    Linked,
}

/// Whether there is a cluster boundary between `before` and `after`.
fn boundary(
    before: Properties,
    after: Properties,
    emoji: Emoji,
    indic: Indic,
    flags: usize,
) -> bool {
    use Break::*;
    match (before.kind, after.kind) {
        (Cr, Lf) => false,                                               // GB3
        (Cr | Lf | Control, _) | (_, Cr | Lf | Control) => true,         // GB4, GB5
        (L, L | V | Lv | Lvt) | (Lv | V, V | T) | (Lvt | T, T) => false, // GB6 to GB8
        (_, Extend | Zwj | SpacingMark) | (Prepend, _) => false,         // GB9 to GB9b
        _ if after.conjunct == Conjunct::Consonant && indic == Indic::Linked => false, // GB9c
        (Zwj, _) if after.pictographic && emoji == Emoji::Joined => false, // GB11
        (RegionalIndicator, RegionalIndicator) => flags.is_multiple_of(2), // GB12, GB13
        _ => true,                                                       // GB999
    }
}

/// The byte offsets where the clusters of `text` start, in order; the first
/// is 0 unless the text is empty.
pub fn starts(text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut previous: Option<Properties> = None;
    let mut emoji = Emoji::None;
    let mut indic = Indic::None;
    // Regional indicators just before the current character.
    let mut flags = 0usize;
    for (offset, c) in text.char_indices() {
        let current = properties(c);
        let starts_cluster = match previous {
            None => true,
            Some(before) => boundary(before, current, emoji, indic, flags),
        };
        if starts_cluster {
            found.push(offset);
        }
        emoji = match current.kind {
            _ if current.pictographic => Emoji::Pictograph,
            Break::Extend if emoji == Emoji::Pictograph => Emoji::Pictograph,
            Break::Zwj if emoji == Emoji::Pictograph => Emoji::Joined,
            _ => Emoji::None,
        };
        indic = match current.conjunct {
            Conjunct::Consonant => Indic::Consonant,
            Conjunct::Linker if indic != Indic::None => Indic::Linked,
            Conjunct::Extend => indic,
            _ => Indic::None,
        };
        flags = if current.kind == Break::RegionalIndicator {
            flags + 1
        } else {
            0
        };
        previous = Some(current);
    }
    found
}

/// The extended grapheme clusters of `text`.
pub fn clusters(text: &str) -> Vec<&str> {
    let starts = starts(text);
    let mut found = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        found.push(&text[start..end]);
    }
    found
}

/// The number of extended grapheme clusters in `text`.
pub fn count(text: &str) -> usize {
    starts(text).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_clusters() {
        assert_eq!(clusters("e\u{301}x"), ["e\u{301}", "x"]);
        assert_eq!(clusters("\r\n\n"), ["\r\n", "\n"]);
        assert_eq!(count("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"), 1);
        assert_eq!(count("\u{1F1EB}\u{1F1F7}\u{1F1E9}"), 2);
        assert_eq!(count(""), 0);
    }
}
