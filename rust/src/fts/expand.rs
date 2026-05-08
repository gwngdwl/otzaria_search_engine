/// Port of FtsLib/Core/WildcardExpander.cs + FuzzyExpander.cs
///
/// Expands query patterns into concrete index terms using Tantivy's term dictionary.
///
/// Wildcard rules (mirrors WildcardExpander.cs):
///   MinAnchorLength        = 2 non-wildcard chars required
///   MaxPrefixWildcardChars = 3 (leading * caps: Hebrew prefix stack ≤ 3)
///   MaxSuffixWildcardChars = 4 (trailing * caps: Hebrew suffix ≤ 4)
///   MaxOptionalChars       = 4 (? operators per pattern)
///
/// Fuzzy uses n-gram filter (trigrams for len≥4, bigrams for len=3, substring for ≤2)
/// followed by Levenshtein confirmation — mirrors FuzzyExpander.cs.

use std::collections::HashSet;
use tantivy::schema::Field;
use tantivy::Searcher;

// ── Constants ─────────────────────────────────────────────────────────────────

const MIN_ANCHOR_LENGTH: usize = 2;
const MAX_PREFIX_WILDCARD_CHARS: usize = 3; // leading * budget (Hebrew prefix)
const MAX_SUFFIX_WILDCARD_CHARS: usize = 4; // trailing * budget (Hebrew suffix)
const MAX_OPTIONAL_CHARS: usize = 4;        // max ? operators per pattern

// ── Public entry points ───────────────────────────────────────────────────────

/// Expand a wildcard pattern (may contain `*` and/or `?`) into concrete terms.
/// Returns empty when the anchor is too short or nothing survives the filter.
pub fn expand_wildcard(pattern: &str, field: Field, searcher: &Searcher) -> Vec<String> {
    let has_optional = pattern.contains('?');

    if !has_optional {
        return expand_star(pattern, field, searcher);
    }

    let opt_count = count_effective_optionals(pattern);
    if opt_count > MAX_OPTIONAL_CHARS {
        return vec![];
    }

    // Generate all sub-patterns by including/excluding each optional char.
    let mut sub_patterns: HashSet<String> = HashSet::new();
    expand_optionals(pattern, 0, &mut String::with_capacity(pattern.len()), &mut sub_patterns);

    let mut seen: HashSet<String> = HashSet::new();
    let mut results: Vec<String> = Vec::new();

    for sub in &sub_patterns {
        let expanded = if sub.contains('*') {
            expand_star(sub, field, searcher)
        } else {
            lookup_literal(sub, field, searcher)
        };
        for term in expanded {
            if seen.insert(term.clone()) {
                results.push(term);
            }
        }
    }

    results
}

/// Expand a fuzzy term into all index terms within `max_distance` Levenshtein edits.
/// Returns empty if nothing matches.
pub fn expand_fuzzy(term: &str, max_distance: u32, field: Field, searcher: &Searcher) -> Vec<String> {
    let max_dist = (max_distance.min(3).max(1)) as usize;

    let chars: Vec<char> = term.chars().collect();
    let char_count = chars.len();

    let candidates: HashSet<String> = if char_count >= 4 {
        let ngrams = build_ngrams(term, 3);
        query_by_ngrams(&ngrams, field, searcher)
    } else if char_count == 3 {
        let ngrams = build_ngrams(term, 2);
        query_by_ngrams(&ngrams, field, searcher)
    } else {
        query_by_substring(term, field, searcher)
    };

    // Levenshtein confirmation (with early termination at max_dist)
    candidates
        .into_iter()
        .filter(|candidate| levenshtein_capped(term, candidate, max_dist) <= max_dist)
        .collect()
}

// ── Term dictionary helpers ───────────────────────────────────────────────────

/// Stream all terms in a field's term dictionary across all segments.
/// Calls `callback` for each term string. Stops when callback returns false.
fn for_each_term(field: Field, searcher: &Searcher, mut callback: impl FnMut(&str) -> bool) {
    for segment_reader in searcher.segment_readers() {
        let inv_index = match segment_reader.inverted_index(field) {
            Ok(idx) => idx,
            Err(_) => continue,
        };
        let mut stream = match inv_index.terms().stream() {
            Ok(s) => s,
            Err(_) => continue,
        };
        while stream.advance() {
            let key = stream.key();
            if let Ok(term_str) = std::str::from_utf8(key) {
                if !callback(term_str) {
                    return;
                }
            }
        }
    }
}

// ── Star-only wildcard expansion ──────────────────────────────────────────────

fn expand_star(pattern: &str, field: Field, searcher: &Searcher) -> Vec<String> {
    let anchor_len = anchor_char_count(pattern);
    if anchor_len < MIN_ANCHOR_LENGTH {
        return vec![];
    }

    let has_leading_star = pattern.starts_with('*');
    let has_trailing_star = pattern.ends_with('*');

    let mut seen: HashSet<String> = HashSet::new();
    let mut results: Vec<String> = Vec::new();

    for_each_term(field, searcher, |term_str| {
        if !glob_match(pattern, term_str) {
            return true;
        }

        // Hebrew wildcard budget filter
        let term_chars = term_str.chars().count();
        let extra = term_chars.saturating_sub(anchor_len);
        let passes = if has_leading_star && has_trailing_star {
            extra <= MAX_PREFIX_WILDCARD_CHARS + MAX_SUFFIX_WILDCARD_CHARS
        } else if has_leading_star {
            extra <= MAX_PREFIX_WILDCARD_CHARS
        } else {
            extra <= MAX_SUFFIX_WILDCARD_CHARS
        };

        if passes && seen.insert(term_str.to_string()) {
            results.push(term_str.to_string());
        }
        true
    });

    results
}

/// Simple glob match: `*` matches any sequence of chars (including empty).
/// No `?` expected here — those are pre-expanded in `expand_wildcard`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_inner(&p, &t)
}

fn glob_inner(p: &[char], t: &[char]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(&'*'), _) => {
            // * matches empty or advance text by one char
            glob_inner(&p[1..], t) || (!t.is_empty() && glob_inner(p, &t[1..]))
        }
        (Some(pc), Some(tc)) if pc == tc => glob_inner(&p[1..], &t[1..]),
        _ => false,
    }
}

// ── Literal lookup ────────────────────────────────────────────────────────────

fn lookup_literal(term: &str, field: Field, searcher: &Searcher) -> Vec<String> {
    if anchor_char_count(term) < MIN_ANCHOR_LENGTH {
        return vec![];
    }
    for segment_reader in searcher.segment_readers() {
        let inv_index = match segment_reader.inverted_index(field) {
            Ok(idx) => idx,
            Err(_) => continue,
        };
        if let Ok(Some(_)) = inv_index.terms().get(term.as_bytes()) {
            return vec![term.to_string()];
        }
    }
    vec![]
}

// ── Optional-char ('?') expansion ────────────────────────────────────────────

fn expand_optionals(pattern: &str, pos: usize, current: &mut String, results: &mut HashSet<String>) {
    let chars: Vec<char> = pattern.chars().collect();
    expand_optionals_inner(&chars, pos, current, results);
}

fn expand_optionals_inner(
    chars: &[char],
    pos: usize,
    current: &mut String,
    results: &mut HashSet<String>,
) {
    if pos == chars.len() {
        results.insert(current.clone());
        return;
    }

    let c = chars[pos];

    if c != '?' {
        current.push(c);
        expand_optionals_inner(chars, pos + 1, current, results);
        current.pop();
        return;
    }

    // c == '?'
    let has_target = current.chars().last().map(|c| c != '*').unwrap_or(false);

    if !has_target {
        // No-op '?' — skip it
        expand_optionals_inner(chars, pos + 1, current, results);
        return;
    }

    // Branch 1: include the optional char (already in current)
    expand_optionals_inner(chars, pos + 1, current, results);

    // Branch 2: exclude the optional char
    let saved = current.pop().unwrap();
    expand_optionals_inner(chars, pos + 1, current, results);
    current.push(saved);
}

fn count_effective_optionals(pattern: &str) -> usize {
    let chars: Vec<char> = pattern.chars().collect();
    let mut count = 0;
    for i in 0..chars.len() {
        if chars[i] != '?' { continue; }
        if i == 0 { continue; }
        let prev = chars[i - 1];
        if prev == '*' || prev == '?' { continue; }
        count += 1;
    }
    count
}

/// Number of non-wildcard chars in pattern (anchor length in char units).
fn anchor_char_count(pattern: &str) -> usize {
    pattern.chars().filter(|&c| c != '*' && c != '?').count()
}

// ── Fuzzy: n-gram generation ──────────────────────────────────────────────────

fn build_ngrams(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut list: Vec<String> = Vec::new();
    if chars.len() < n { return list; }
    for i in 0..=(chars.len() - n) {
        let ng: String = chars[i..i + n].iter().collect();
        if seen.insert(ng.clone()) {
            list.push(ng);
        }
    }
    list
}

fn query_by_ngrams(ngrams: &[String], field: Field, searcher: &Searcher) -> HashSet<String> {
    let mut results: HashSet<String> = HashSet::new();
    for_each_term(field, searcher, |term_str| {
        if ngrams.iter().any(|ng| term_str.contains(ng.as_str())) {
            results.insert(term_str.to_string());
        }
        true
    });
    results
}

fn query_by_substring(term: &str, field: Field, searcher: &Searcher) -> HashSet<String> {
    let mut results: HashSet<String> = HashSet::new();
    for_each_term(field, searcher, |term_str| {
        if term_str.contains(term) {
            results.insert(term_str.to_string());
        }
        true
    });
    results
}

// ── Levenshtein distance ──────────────────────────────────────────────────────

pub fn levenshtein(a: &str, b: &str) -> usize {
    levenshtein_capped(a, b, usize::MAX)
}

/// Levenshtein with early termination: returns `max_dist + 1` (or any value
/// greater than `max_dist`) as soon as it is certain the true distance exceeds
/// `max_dist`. Mirrors FtsLib/Core/Levenshtein.cs.
pub fn levenshtein_capped(a: &str, b: &str, max_dist: usize) -> usize {
    let mut a_chars: Vec<char> = a.chars().collect();
    let mut b_chars: Vec<char> = b.chars().collect();

    // Keep the shorter string in `a_chars` to minimise row width.
    if a_chars.len() > b_chars.len() {
        std::mem::swap(&mut a_chars, &mut b_chars);
    }

    let na = a_chars.len();
    let nb = b_chars.len();

    if na == 0 { return nb; }
    // If lengths differ by more than max_dist, the answer is at least
    // (nb - na), which exceeds max_dist — bail.
    if nb.saturating_sub(na) > max_dist {
        return max_dist.saturating_add(1);
    }

    let mut prev: Vec<usize> = (0..=na).collect();
    let mut curr: Vec<usize> = vec![0; na + 1];

    for j in 1..=nb {
        curr[0] = j;
        let mut row_min = curr[0];
        for i in 1..=na {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[i] = (curr[i - 1] + 1)
                .min(prev[i] + 1)
                .min(prev[i - 1] + cost);
            if curr[i] < row_min {
                row_min = curr[i];
            }
        }
        if row_min > max_dist {
            return max_dist.saturating_add(1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[na]
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_levenshtein_exact() {
        assert_eq!(levenshtein("שלום", "שלום"), 0);
    }

    #[test]
    fn test_levenshtein_one_edit_deletion() {
        assert_eq!(levenshtein("שלום", "שלם"), 1);
    }

    #[test]
    fn test_levenshtein_one_edit_insertion() {
        assert_eq!(levenshtein("שלם", "שלום"), 1);
    }

    #[test]
    fn test_levenshtein_two_edits() {
        assert_eq!(levenshtein("שלום", "שם"), 2);
    }

    #[test]
    fn test_anchor_char_count() {
        assert_eq!(anchor_char_count("שלו*"), 3);
        assert_eq!(anchor_char_count("*לום"), 3);
        assert_eq!(anchor_char_count("*לו*"), 2);
        assert_eq!(anchor_char_count("שלו?ם"), 4);
    }

    #[test]
    fn test_glob_match_prefix() {
        assert!(glob_match("שלו*", "שלום"));
        assert!(glob_match("שלו*", "שלומי"));
        assert!(!glob_match("שלו*", "שלם"));
    }

    #[test]
    fn test_glob_match_suffix() {
        assert!(glob_match("*לום", "שלום"));
        assert!(glob_match("*לום", "הלום"));
        assert!(!glob_match("*לום", "שלומי"));
    }

    #[test]
    fn test_glob_match_infix() {
        assert!(glob_match("*לו*", "שלום"));
        assert!(glob_match("*לו*", "בלוק"));
        assert!(!glob_match("*לו*", "שמאל"));
    }

    #[test]
    fn test_expand_optionals_basic() {
        let mut subs: HashSet<String> = HashSet::new();
        expand_optionals("שלו?ם", 0, &mut String::new(), &mut subs);
        assert!(subs.contains("שלום"), "with ו");
        assert!(subs.contains("שלם"), "without ו");
        assert_eq!(subs.len(), 2);
    }

    #[test]
    fn test_expand_optionals_leading_question_noop() {
        let mut subs: HashSet<String> = HashSet::new();
        expand_optionals("?שלום", 0, &mut String::new(), &mut subs);
        assert!(subs.contains("שלום"));
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn test_build_ngrams_trigrams() {
        let ng = build_ngrams("שלום", 3);
        assert!(ng.contains(&"שלו".to_string()));
        assert!(ng.contains(&"לום".to_string()));
    }

    #[test]
    fn test_build_ngrams_too_short() {
        let ng = build_ngrams("של", 3);
        assert!(ng.is_empty());
    }
}
