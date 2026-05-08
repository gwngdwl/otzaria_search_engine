/// Port of FtsLib/Core/QueryParser.cs
///
/// Query syntax:
///   word        — literal AND term
///   word*       — wildcard (prefix / infix / suffix)
///   wor?d       — optional char: the char before '?' is optional
///   word~       — fuzzy, edit distance 1
///   word~N      — fuzzy, edit distance N (1–3)
///   a | b       — OR within one AND slot
///
/// Multiple tokens are AND-ed; '|'-separated tokens are OR-ed within one slot.
/// Wildcard + fuzzy on the same token: wildcard wins (fuzzy suffix stripped).

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub groups: Vec<QueryGroup>,
}

#[derive(Debug, Clone)]
pub struct QueryGroup {
    pub alternatives: Vec<SubPattern>,
}

#[derive(Debug, Clone)]
pub struct SubPattern {
    pub pattern: String,
    pub is_wildcard: bool,
    pub is_fuzzy: bool,
    pub fuzzy_distance: u32,
}

impl ParsedQuery {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

impl QueryGroup {
    pub fn is_single(&self) -> bool {
        self.alternatives.len() == 1
    }
}

pub fn parse(query: &str) -> ParsedQuery {
    if query.trim().is_empty() {
        return ParsedQuery { groups: vec![] };
    }

    let mut groups: Vec<QueryGroup> = Vec::new();
    let mut pending: Vec<SubPattern> = Vec::new();
    let mut last_was_pipe = false;

    for raw in query.split_whitespace() {
        if is_pipe_token(raw) {
            last_was_pipe = true;
            continue;
        }

        let sp = match parse_token(raw) {
            Some(s) => s,
            None => continue,
        };

        if !last_was_pipe && !pending.is_empty() {
            groups.push(QueryGroup { alternatives: std::mem::take(&mut pending) });
        }

        pending.push(sp);
        last_was_pipe = false;
    }

    if !pending.is_empty() {
        groups.push(QueryGroup { alternatives: pending });
    }

    ParsedQuery { groups }
}

fn is_pipe_token(raw: &str) -> bool {
    !raw.is_empty() && raw.chars().all(|c| c == '|')
}

fn parse_token(raw: &str) -> Option<SubPattern> {
    // Detect trailing fuzzy suffix (~  or ~N) before normalising,
    // because '~' and digits are dropped by normalise.
    let mut token_text = raw;
    let mut is_fuzzy = false;
    let mut fuzzy_distance: u32 = 1;

    if let Some(tilde_pos) = raw.rfind('~') {
        let suffix = &raw[tilde_pos + 1..];
        let prefix = &raw[..tilde_pos];
        // suffix must be empty or a single digit 1–9
        if suffix.is_empty()
            || (suffix.len() == 1 && suffix.as_bytes()[0] >= b'1' && suffix.as_bytes()[0] <= b'9')
        {
            is_fuzzy = true;
            fuzzy_distance = if suffix.is_empty() {
                1
            } else {
                let d = (suffix.as_bytes()[0] - b'0') as u32;
                d.min(3)
            };
            token_text = prefix;
        }
    }

    let normalised = normalise(token_text);
    if normalised.is_empty() {
        return None;
    }

    let is_wildcard = normalised.contains('*') || normalised.contains('?');

    // Wildcard + fuzzy: wildcard wins.
    if is_wildcard && is_fuzzy {
        is_fuzzy = false;
    }

    Some(SubPattern {
        pattern: normalised,
        is_wildcard,
        is_fuzzy,
        fuzzy_distance,
    })
}

/// Strips nikud/cantillation, lowercases ASCII, drops non-letter non-wildcard chars.
/// Preserves '*' and '?' so the caller can detect wildcard positions.
pub fn normalise(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for c in token.chars() {
        // Strip nikud (U+05B0–U+05C7) and cantillation (U+0591–U+05AF)
        if ('\u{0591}'..='\u{05C7}').contains(&c) {
            continue;
        }
        if c == '*' || c == '?' {
            out.push(c);
            continue;
        }
        // Hebrew letters U+05D0–U+05EA
        if ('\u{05D0}'..='\u{05EA}').contains(&c) {
            out.push(c);
            continue;
        }
        // ASCII letters — lowercase
        if c.is_ascii_alphabetic() {
            out.push(c.to_ascii_lowercase());
            continue;
        }
        // Everything else dropped
    }
    out
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_query() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
    }

    #[test]
    fn test_single_literal() {
        let q = parse("שלום");
        assert_eq!(q.groups.len(), 1);
        let g = &q.groups[0];
        assert_eq!(g.alternatives.len(), 1);
        assert_eq!(g.alternatives[0].pattern, "שלום");
        assert!(!g.alternatives[0].is_wildcard);
        assert!(!g.alternatives[0].is_fuzzy);
    }

    #[test]
    fn test_two_and_terms() {
        let q = parse("שלום עולם");
        assert_eq!(q.groups.len(), 2);
        assert_eq!(q.groups[0].alternatives[0].pattern, "שלום");
        assert_eq!(q.groups[1].alternatives[0].pattern, "עולם");
    }

    #[test]
    fn test_or_group() {
        let q = parse("שלום | עולם");
        assert_eq!(q.groups.len(), 1);
        assert_eq!(q.groups[0].alternatives.len(), 2);
        assert_eq!(q.groups[0].alternatives[0].pattern, "שלום");
        assert_eq!(q.groups[0].alternatives[1].pattern, "עולם");
    }

    #[test]
    fn test_or_then_and() {
        let q = parse("שלום | עולם חבר");
        assert_eq!(q.groups.len(), 2);
        assert_eq!(q.groups[0].alternatives.len(), 2);
        assert_eq!(q.groups[1].alternatives.len(), 1);
        assert_eq!(q.groups[1].alternatives[0].pattern, "חבר");
    }

    #[test]
    fn test_wildcard_prefix() {
        let q = parse("שלו*");
        assert!(q.groups[0].alternatives[0].is_wildcard);
        assert!(!q.groups[0].alternatives[0].is_fuzzy);
        assert_eq!(q.groups[0].alternatives[0].pattern, "שלו*");
    }

    #[test]
    fn test_fuzzy_default_distance() {
        let q = parse("שלום~");
        let sp = &q.groups[0].alternatives[0];
        assert!(sp.is_fuzzy);
        assert!(!sp.is_wildcard);
        assert_eq!(sp.fuzzy_distance, 1);
        assert_eq!(sp.pattern, "שלום");
    }

    #[test]
    fn test_fuzzy_explicit_distance() {
        let q = parse("שלום~2");
        assert_eq!(q.groups[0].alternatives[0].fuzzy_distance, 2);
    }

    #[test]
    fn test_fuzzy_clamped_to_3() {
        let q = parse("שלום~9");
        assert_eq!(q.groups[0].alternatives[0].fuzzy_distance, 3);
    }

    #[test]
    fn test_wildcard_wins_over_fuzzy() {
        let q = parse("שלו*~");
        let sp = &q.groups[0].alternatives[0];
        assert!(sp.is_wildcard);
        assert!(!sp.is_fuzzy);
    }

    #[test]
    fn test_nikud_stripped() {
        // שָׁלוֹם with nikud → שלום
        let q = parse("שָׁלוֹם");
        assert_eq!(q.groups[0].alternatives[0].pattern, "שלום");
    }

    #[test]
    fn test_leading_trailing_pipe_ignored() {
        let q = parse("| שלום |");
        assert_eq!(q.groups.len(), 1);
        assert_eq!(q.groups[0].alternatives[0].pattern, "שלום");
    }

    #[test]
    fn test_optional_char_wildcard() {
        let q = parse("שלו?ם");
        let sp = &q.groups[0].alternatives[0];
        assert!(sp.is_wildcard);
        assert_eq!(sp.pattern, "שלו?ם");
    }

    #[test]
    fn test_ascii_lowercased() {
        let q = parse("Hello");
        assert_eq!(q.groups[0].alternatives[0].pattern, "hello");
    }
}
