//! Matching a query against an entry's name: a case-insensitive
//! subsequence test with a score that decides the order.
//!
//! The rule the whole module implements, in one sentence: **every
//! character of the query must appear in the name, in order, and the
//! tighter and earlier the run, the better the score.** That is the
//! behaviour every fuzzy launcher has trained users to expect — `fx`
//! finds Firefox — and it is one pass over the name with no allocation.
//!
//! # Why subsequence rather than substring
//!
//! A substring match is a strictly worse launcher for the same code: it
//! cannot find `Text Editor` from `txed`, which is exactly the kind of
//! thing a user types when they are not looking at the screen. The cost
//! is false positives on long names (`Disk Usage Analyzer` matches
//! `days`), which the scoring pushes to the bottom rather than removes —
//! and a false positive ranked twentieth is invisible, while a missing
//! match is a launcher that "does not work".
//!
//! # The score, and the one thing it must get right
//!
//! Three bonuses, in decreasing size:
//!
//! | case | why it wins |
//! |---|---|
//! | the whole query is a prefix of the name | `cal` must put `Calculator` above `Kcalc` |
//! | a character starts a word | `fs` should find `File Search` over `Fastfetch` |
//! | consecutive characters | a tight run is what the user was typing |
//!
//! And one penalty: each skipped character costs a little, so a query
//! scattered across a long name scores below the same query packed into a
//! short one.
//!
//! The property that matters more than any individual number is that the
//! **exact prefix always wins**, because that is the case a user can
//! predict and therefore rely on. `scoring_puts_an_exact_prefix_first`
//! pins it down; the rest of the constants are taste, and are tuned by
//! the tests rather than by argument.

/// An exact case-insensitive equality with the whole name.
const EXACT: i32 = 1_000;
/// The query is a prefix of the name.
const PREFIX: i32 = 400;
/// A matched character that starts a word (first character, or after a
/// space, `-`, `_`, `.` or `/`).
const WORD_START: i32 = 60;
/// A matched character immediately after the previous match.
const CONSECUTIVE: i32 = 25;
/// Every matched character is worth something on its own, so a longer
/// query that still matches outranks a shorter one that happens to.
const MATCHED: i32 = 10;
/// Each character skipped before a match.
const GAP: i32 = -2;
/// Each character of the name left over after the last match, so a short
/// name beats a long one that contains it.
const TRAILING: i32 = -1;

/// Score `name` against `query`, or `None` when the query is not a
/// subsequence of the name.
///
/// An **empty query matches everything** with score 0: that is what makes
/// a freshly opened launcher show the whole list rather than nothing, and
/// it falls out of "every character of the query appears" rather than
/// being a special case anywhere else.
///
/// Case is folded with `to_ascii_lowercase` on both sides. Unicode case
/// folding is a table and a dependency; every application name that
/// matters here is ASCII-cased, and a non-ASCII name still matches
/// exactly, which is the honest degradation.
#[must_use]
pub fn score(name: &str, query: &str) -> Option<i32> {
    let q: Vec<char> = query
        .trim()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let n: Vec<char> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.len() > n.len() {
        return None;
    }

    let mut total = 0;
    let mut qi = 0;
    // Index of the previous match, so "consecutive" is a comparison and
    // not a second pass.
    let mut last: Option<usize> = None;
    for (i, c) in n.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if *c != q[qi] {
            continue;
        }
        total += MATCHED;
        if is_word_start(&n, i) {
            total += WORD_START;
        }
        match last {
            Some(prev) if prev + 1 == i => total += CONSECUTIVE,
            Some(prev) => total += GAP * i32::try_from(i - prev - 1).unwrap_or(i32::MAX),
            // A run that starts late in the name is worth less than one
            // that starts at the front, and the gap penalty is what says
            // so — counted from the beginning for the first match.
            None => total += GAP * i32::try_from(i).unwrap_or(i32::MAX),
        }
        last = Some(i);
        qi += 1;
    }
    if qi < q.len() {
        return None;
    }
    if let Some(prev) = last {
        total += TRAILING * i32::try_from(n.len() - prev - 1).unwrap_or(i32::MAX);
    }
    if n.starts_with(&q[..]) {
        total += PREFIX;
    }
    if n.len() == q.len() && n == q {
        total += EXACT;
    }
    Some(total)
}

/// Whether the character at `i` starts a word.
fn is_word_start(name: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    matches!(name[i - 1], ' ' | '-' | '_' | '.' | '/' | ':')
}

/// Rank `items` against `query`, best first, keeping at most `limit`.
///
/// The comparison is score first and then the item's own order, which is
/// the caller's — [`crate::desktop::scan`] sorts by name — so equal
/// scores come out alphabetically rather than in whatever order the
/// filesystem produced. A list that reshuffled between two identical
/// queries would be unusable with muscle memory, and that is the whole
/// reason the tiebreak is here rather than left to `sort_by_key`'s
/// stability alone.
///
/// Returns indices into `items`, so the caller keeps its own storage and
/// nothing is cloned to rank it.
pub fn rank<'a, I>(items: I, query: &str, limit: usize) -> Vec<usize>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut scored: Vec<(i32, usize)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(i, name)| score(name, query).map(|s| (s, i)))
        .collect();
    // Descending by score, ascending by original index.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names, best-ranked first.
    fn ranked<'a>(names: &[&'a str], query: &str) -> Vec<&'a str> {
        rank(names.iter().copied(), query, 20)
            .into_iter()
            .map(|i| names[i])
            .collect()
    }

    #[test]
    fn a_subsequence_matches_and_a_missing_character_does_not() {
        assert!(score("Firefox", "fx").is_some(), "fx is in f-ire-f-o-x");
        assert!(score("Firefox", "ffx").is_some());
        assert!(score("Firefox", "fxz").is_none(), "no z anywhere");
        // Order matters: the characters must appear *in sequence*.
        assert!(score("Firefox", "xf").is_none());
        // A query longer than the name cannot be a subsequence.
        assert!(score("Cat", "catalogue").is_none());
    }

    #[test]
    fn matching_ignores_case_on_both_sides() {
        assert!(score("Calculator", "CALC").is_some());
        assert!(score("CALCULATOR", "calc").is_some());
        assert_eq!(score("Calculator", "calc"), score("calculator", "CALC"));
    }

    #[test]
    fn an_empty_query_matches_everything() {
        // What makes a freshly opened launcher show the whole list.
        assert_eq!(score("anything", ""), Some(0));
        assert_eq!(score("anything", "   "), Some(0));
        assert_eq!(ranked(&["A", "B"], ""), vec!["A", "B"]);
    }

    #[test]
    fn scoring_puts_an_exact_prefix_first() {
        // The property a user can predict, and therefore the one that
        // must hold regardless of how the constants are tuned.
        assert_eq!(
            ranked(&["KCalc", "Calculator"], "calc"),
            vec!["Calculator", "KCalc"]
        );
        // And "Calendar" is not a match at all: `calc` needs a second
        // `c`, which it does not have. A substring matcher would agree;
        // the point is that the subsequence rule does not over-match.
        assert!(score("Calendar", "calc").is_none());
        assert_eq!(
            ranked(&["Terminator", "Terminal", "Term"], "term"),
            vec!["Term", "Terminal", "Terminator"]
        );
    }

    #[test]
    fn a_word_start_beats_a_character_in_the_middle() {
        // `fs` should find "File Search", not "Fastfetch" — both match,
        // but one matches at word boundaries.
        assert_eq!(
            ranked(&["Fastfetch", "File Search"], "fs"),
            vec!["File Search", "Fastfetch"]
        );
        // Hyphens and underscores are word boundaries too, because
        // program names use them where a sentence would use a space.
        assert!(
            score("hello-dialog", "hd").unwrap() > score("hoard", "hd").unwrap(),
            "a hyphen starts a word"
        );
    }

    #[test]
    fn a_tight_run_beats_a_scattered_one() {
        assert!(
            score("nitro-calc", "calc").unwrap() > score("chameleon alchemy", "calc").unwrap(),
            "consecutive characters are what the user typed"
        );
    }

    #[test]
    fn a_shorter_name_wins_a_tie() {
        // Both start with the query; the shorter one is the more likely
        // intent, and the trailing penalty is what says so.
        assert_eq!(
            ranked(&["Calculator Plus Extra Edition", "Calculator"], "calcu"),
            vec!["Calculator", "Calculator Plus Extra Edition"]
        );
    }

    #[test]
    fn an_exact_name_outranks_everything() {
        assert_eq!(
            ranked(&["Terminal", "Term", "Terminator"], "term"),
            vec!["Term", "Terminal", "Terminator"]
        );
    }

    #[test]
    fn the_limit_is_honoured_and_ties_keep_the_input_order() {
        let names = ["Aa", "Ab", "Ac", "Ad", "Ae"];
        assert_eq!(ranked(&names, "a").len(), 5);
        let top: Vec<&str> = rank(names.iter().copied(), "a", 2)
            .into_iter()
            .map(|i| names[i])
            .collect();
        assert_eq!(top, vec!["Aa", "Ab"], "a tie keeps the caller's order");
    }

    #[test]
    fn nothing_matches_a_query_no_name_contains() {
        assert!(ranked(&["Calculator", "Firefox"], "zzz").is_empty());
    }
}
