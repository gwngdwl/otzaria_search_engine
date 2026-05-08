/// Port of FtsLib/Core/SnippetBuilder.cs
///
/// Builds a highlighted HTML snippet from raw HTML text and a set of query
/// term groups.
///
/// Algorithm:
///   1. Tokenize via `fts_html::scan` — strips HTML tags + nikud, records raw
///      byte offsets and visible-char offsets.
///   2. Sliding window finds the tightest token window covering all groups.
///   3. Window expansion adds context margin (with scaling for loose matches).
///   4. Renderer writes the snippet, stripping HTML tags from gaps and
///      escaping `&` and `>`.
///
/// Returns a SnippetResult with:
///   html          — highlighted snippet HTML
///   score         — raw-byte span of tightest window (smaller = better);
///                   u32::MAX if no match
///   word_distance — extra tokens between matched groups; u32::MAX if no match
///   is_match      — false means index false positive (filter out)

use std::collections::HashMap;

use crate::fts::html::{append_stripped, encode_stripped, scan, ScannedWord};

pub struct SnippetBuilder {
    pub pre_tag: String,
    pub post_tag: String,
    pub snippet_length: usize,   // budget in visible chars
    pub context_margin: usize,   // max context on each side, visible chars
}

#[derive(Debug)]
pub struct SnippetResult {
    pub html: String,
    pub score: u32,
    pub word_distance: u32,
    pub is_match: bool,
}

#[derive(Clone)]
struct TextToken {
    normalised: String,
    raw_start: usize,
    raw_end: usize,
    visible_start: usize,
}

impl Default for SnippetBuilder {
    fn default() -> Self {
        SnippetBuilder {
            pre_tag: "<font color=red>".to_string(),
            post_tag: "</font>".to_string(),
            snippet_length: 800,
            context_margin: 150,
        }
    }
}

impl SnippetBuilder {
    pub fn new(pre_tag: &str, post_tag: &str, snippet_length: usize, context_margin: usize) -> Self {
        SnippetBuilder {
            pre_tag: pre_tag.to_string(),
            post_tag: post_tag.to_string(),
            snippet_length,
            context_margin,
        }
    }

    /// Build a snippet given a list of query groups (each group is a set of OR terms).
    /// `expanded_groups[i]` = the concrete index terms that match AND slot i.
    pub fn build(
        &self,
        text: &str,
        expanded_groups: &[Vec<String>],
    ) -> SnippetResult {
        self.build_with_options(text, expanded_groups, false)
    }

    /// Like `build`, but with `require_ordered = true` only returns a match
    /// when groups appear in left-to-right order in the text.
    pub fn build_with_options(
        &self,
        text: &str,
        expanded_groups: &[Vec<String>],
        require_ordered: bool,
    ) -> SnippetResult {
        if text.is_empty() || expanded_groups.is_empty() {
            return no_match(text);
        }

        let tokens = tokenize(text);
        if tokens.is_empty() {
            return no_match(text);
        }

        // Build term → group index map
        let mut term_to_group: HashMap<&str, usize> = HashMap::new();
        for (gi, group) in expanded_groups.iter().enumerate() {
            for term in group {
                term_to_group.entry(term.as_str()).or_insert(gi);
            }
        }

        let num_groups = expanded_groups.len();
        let (i_left, i_right, score) = run_sliding_window(&tokens, &term_to_group, num_groups);

        if score == u32::MAX {
            return no_match(text);
        }

        let (snap_start, snap_end) =
            self.expand_window(&tokens, text.len(), i_left, i_right);

        let all_terms: std::collections::HashSet<&str> =
            expanded_groups.iter().flatten().map(|s| s.as_str()).collect();

        let html = self.render(text, &tokens, &all_terms, snap_start, snap_end);

        let word_dist = {
            let d = (i_right as i64) - (i_left as i64) - ((num_groups as i64) - 1);
            d.max(0) as u32
        };

        if require_ordered && num_groups > 1 && !has_ordered_match(&tokens, &term_to_group, num_groups) {
            return SnippetResult {
                html,
                score,
                word_distance: word_dist,
                is_match: false,
            };
        }

        SnippetResult {
            html,
            score,
            word_distance: word_dist,
            is_match: true,
        }
    }
}

fn no_match(text: &str) -> SnippetResult {
    SnippetResult {
        html: encode_stripped(text),
        score: u32::MAX,
        word_distance: u32::MAX,
        is_match: false,
    }
}

// ── Tokenizer (delegates to fts_html) ─────────────────────────────────────────

fn tokenize(text: &str) -> Vec<TextToken> {
    let mut tokens = Vec::new();
    scan(text, |w: ScannedWord| {
        tokens.push(TextToken {
            normalised: w.normalized.to_string(),
            raw_start: w.raw_start,
            raw_end: w.raw_end,
            visible_start: w.visible_start,
        });
    });
    tokens
}

// ── Sliding window ────────────────────────────────────────────────────────────

fn run_sliding_window(
    tokens: &[TextToken],
    term_to_group: &HashMap<&str, usize>,
    num_groups: usize,
) -> (usize, usize, u32) {
    let mut group_count = vec![0usize; num_groups];
    let mut covered = 0usize;
    let mut best = (0usize, 0usize, u32::MAX);
    let mut l = 0usize;

    for r in 0..tokens.len() {
        let rt = tokens[r].normalised.as_str();
        if let Some(&rg) = term_to_group.get(rt) {
            if group_count[rg] == 0 {
                covered += 1;
            }
            group_count[rg] += 1;
        }

        while covered == num_groups {
            let span = (tokens[r].raw_end - tokens[l].raw_start) as u32;
            if span < best.2 {
                best = (l, r, span);
            }
            let lt = tokens[l].normalised.as_str();
            if let Some(&lg) = term_to_group.get(lt) {
                group_count[lg] -= 1;
                if group_count[lg] == 0 {
                    covered -= 1;
                }
            }
            l += 1;
        }
    }

    best
}

// ── Ordered-match validation ──────────────────────────────────────────────────

/// Returns true iff there is some position in `tokens` from which each group
/// is satisfied in left-to-right order (group 0, then 1, then 2, ...).
fn has_ordered_match(
    tokens: &[TextToken],
    term_to_group: &HashMap<&str, usize>,
    num_groups: usize,
) -> bool {
    if num_groups <= 1 {
        return true;
    }
    for start in 0..tokens.len() {
        let g0 = match term_to_group.get(tokens[start].normalised.as_str()) {
            Some(&g) if g == 0 => g,
            _ => continue,
        };
        let _ = g0;
        let mut pos = start + 1;
        let mut next_group = 1;
        while next_group < num_groups && pos < tokens.len() {
            if let Some(&tg) = term_to_group.get(tokens[pos].normalised.as_str()) {
                if tg == next_group {
                    next_group += 1;
                }
            }
            pos += 1;
        }
        if next_group == num_groups {
            return true;
        }
    }
    false
}

// ── Window expansion ──────────────────────────────────────────────────────────

impl SnippetBuilder {
    fn expand_window(
        &self,
        tokens: &[TextToken],
        raw_len: usize,
        i_left: usize,
        i_right: usize,
    ) -> (usize, usize) {
        if tokens.is_empty() {
            return (0, raw_len);
        }

        let vis_left = tokens[i_left].visible_start;
        let vis_right = tokens[i_right].visible_start + tokens[i_right].normalised.chars().count();
        let win_visible = vis_right.saturating_sub(vis_left);

        let last = &tokens[tokens.len() - 1];
        let total_visible = last.visible_start + last.normalised.chars().count();

        if total_visible <= self.snippet_length {
            return (0, raw_len);
        }

        let (s_idx, e_idx) = if win_visible >= self.snippet_length {
            let centre = (i_left + i_right) / 2;
            let half = self.snippet_length / 2;
            let s = centre.saturating_sub(half);
            let e = (centre + half).min(tokens.len() - 1);
            (s, e)
        } else {
            let remaining = self.snippet_length - win_visible;
            let scale = 1.0 - (win_visible as f64 / self.snippet_length as f64);
            let scaled_margin = (self.context_margin as f64 * scale) as usize;
            let margin = (remaining / 2).min(scaled_margin);

            let target_left = vis_left.saturating_sub(margin);
            let target_right = vis_right + margin;

            let s = binary_search_left(tokens, target_left);
            let e = binary_search_right(tokens, target_right);
            (s, e)
        };

        let snap_start = tokens[s_idx].raw_start;
        let snap_end = if e_idx + 1 < tokens.len() {
            tokens[e_idx + 1].raw_start
        } else {
            raw_len
        };

        (snap_start.min(raw_len), snap_end.min(raw_len))
    }
}

fn binary_search_left(tokens: &[TextToken], target: usize) -> usize {
    let mut lo = 0;
    let mut hi = tokens.len() - 1;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if tokens[mid].visible_start < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn binary_search_right(tokens: &[TextToken], target: usize) -> usize {
    let mut lo = 0;
    let mut hi = tokens.len() - 1;
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if tokens[mid].visible_start <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

// ── Renderer ──────────────────────────────────────────────────────────────────

impl SnippetBuilder {
    fn render(
        &self,
        text: &str,
        tokens: &[TextToken],
        term_set: &std::collections::HashSet<&str>,
        snap_start: usize,
        snap_end: usize,
    ) -> String {
        let mut out = String::with_capacity((snap_end - snap_start) + 64);

        if snap_start > 0 {
            out.push('…');
        }

        let mut pos = snap_start;

        for tok in tokens {
            if tok.raw_end <= snap_start {
                continue;
            }
            if tok.raw_start >= snap_end {
                break;
            }
            if !term_set.contains(tok.normalised.as_str()) {
                continue;
            }

            // Append the gap between current pos and this token (HTML-stripped)
            append_stripped(text, pos, tok.raw_start, &mut out);

            out.push_str(&self.pre_tag);
            let tok_end = tok.raw_end.min(snap_end);
            // The matched token slice is appended verbatim — it covers the
            // word's source span, which may include adjacent inline tags.
            // FtsLib semantics: paste raw HTML inside the highlight tags.
            out.push_str(&text[tok.raw_start..tok_end]);
            out.push_str(&self.post_tag);
            pos = tok.raw_end;
        }

        append_stripped(text, pos, snap_end, &mut out);

        if snap_end < text.len() {
            out.push('…');
        }

        out
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> SnippetBuilder {
        SnippetBuilder {
            pre_tag: "<mark>".to_string(),
            post_tag: "</mark>".to_string(),
            snippet_length: 400,
            context_margin: 100,
        }
    }

    #[test]
    fn test_single_term_is_match() {
        let b = builder();
        let result = b.build("שלום עולם", &[vec!["שלום".to_string()]]);
        assert!(result.is_match);
        assert!(result.html.contains("<mark>שלום</mark>"));
    }

    #[test]
    fn test_missing_term_not_match() {
        let b = builder();
        let result = b.build("שלום עולם", &[vec!["ביי".to_string()]]);
        assert!(!result.is_match);
    }

    #[test]
    fn test_two_term_word_distance_zero() {
        let b = builder();
        let result = b.build(
            "שלום עולם",
            &[vec!["שלום".to_string()], vec!["עולם".to_string()]],
        );
        assert!(result.is_match);
        assert_eq!(result.word_distance, 0);
    }

    #[test]
    fn test_two_term_word_distance_nonzero() {
        let b = builder();
        let result = b.build(
            "שלום רב חבר עולם",
            &[vec!["שלום".to_string()], vec!["עולם".to_string()]],
        );
        assert!(result.is_match);
        assert_eq!(result.word_distance, 2);
    }

    #[test]
    fn test_or_group_match() {
        let b = builder();
        let result = b.build(
            "שלום עולם",
            &[vec!["שלום".to_string(), "הי".to_string()]],
        );
        assert!(result.is_match);
    }

    #[test]
    fn test_score_smaller_for_closer_terms() {
        let b = builder();
        let close = b.build(
            "שלום עולם",
            &[vec!["שלום".to_string()], vec!["עולם".to_string()]],
        );
        let far = b.build(
            "שלום אא בב גג דד הה וו זז חח טט יי כך לל ממ נן עע פף צץ קק רר שש תת עולם",
            &[vec!["שלום".to_string()], vec!["עולם".to_string()]],
        );
        assert!(close.score < far.score);
    }

    #[test]
    fn test_strips_html_in_gaps() {
        let b = builder();
        let result = b.build(
            "<p>שלום <b>עולם</b> חבר</p>",
            &[vec!["חבר".to_string()]],
        );
        assert!(result.is_match);
        // No raw <p> or <b> tags should appear in the output gaps
        assert!(!result.html.contains("<p>"));
        assert!(!result.html.contains("</p>"));
        // The highlight tag itself is present
        assert!(result.html.contains("<mark>"));
    }

    #[test]
    fn test_strips_nikud_for_matching() {
        let b = builder();
        let result = b.build(
            "שָׁלוֹם עוֹלָם",
            &[vec!["שלום".to_string()]],
        );
        assert!(result.is_match);
    }

    #[test]
    fn test_require_ordered_pass() {
        let b = builder();
        let result = b.build_with_options(
            "שלום עולם חבר",
            &[
                vec!["שלום".to_string()],
                vec!["עולם".to_string()],
                vec!["חבר".to_string()],
            ],
            true,
        );
        assert!(result.is_match);
    }

    #[test]
    fn test_require_ordered_fail() {
        let b = builder();
        // "חבר עולם שלום" — appears in reverse order; require_ordered should reject
        let result = b.build_with_options(
            "חבר עולם שלום",
            &[
                vec!["שלום".to_string()],
                vec!["עולם".to_string()],
                vec!["חבר".to_string()],
            ],
            true,
        );
        assert!(!result.is_match);
    }

    #[test]
    fn test_require_ordered_default_off() {
        let b = builder();
        // Without require_ordered, reverse order still matches
        let result = b.build(
            "חבר עולם שלום",
            &[
                vec!["שלום".to_string()],
                vec!["עולם".to_string()],
                vec!["חבר".to_string()],
            ],
        );
        assert!(result.is_match);
    }
}
