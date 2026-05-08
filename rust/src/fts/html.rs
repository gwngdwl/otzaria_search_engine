/// Port of FtsLib/Core/HtmlTextScanner.cs + HtmlScannerHelpers.cs
///
/// Single-pass HTML-aware text scanner. Emits normalized words with byte
/// offsets into the original UTF-8 text.
///
/// Strips:
///   - HTML tags (block tags act as word separators; inline tags ignored)
///   - Nikud / cantillation (U+0591–U+05C7), except paseq (U+05C0),
///     sof pasuq (U+05C3), nun hafukha (U+05C6)
///   - Non-spacing marks beyond ASCII (Unicode category Mn) above U+007F
///
/// Lowercases ASCII. Treats Hebrew (U+05D0–U+05EA) and ASCII a–z as letters.
/// Maqaf (U+05BE) acts as a separator. Whitespace HTML entities flush words.
///
/// Word length filter mirrors C#: emits words with 2..=29 chars.

pub struct ScannedWord<'a> {
    /// Byte offset of the first letter of the word in the source string.
    pub raw_start: usize,
    /// Byte offset just past the separator that ended the word.
    pub raw_end: usize,
    /// Cumulative visible-char count up to (excluding) the first letter.
    pub visible_start: usize,
    /// Normalized word form (Hebrew letters + lowercased ASCII).
    pub normalized: &'a str,
}

/// Scan `text` and call `on_word` once per emitted word.
pub fn scan<F: FnMut(ScannedWord)>(text: &str, mut on_word: F) {
    let mut buffer = String::with_capacity(64);
    let mut tag_name: [u8; 16] = [0; 16];
    let mut tag_len: usize = 0;
    let mut in_tag = false;
    let mut word_start: usize = 0;
    let mut visible_count: usize = 0;

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let total_len = text.len();
    let mut i = 0usize;

    while i < chars.len() {
        let (idx, c) = chars[i];

        // ── HTML tags ────────────────────────────────────────────
        if in_tag {
            if c == '>' {
                if is_block_tag(&tag_name[..tag_len]) {
                    flush_word(&mut buffer, &mut word_start, idx, visible_count, &mut on_word);
                }
                in_tag = false;
                tag_len = 0;
            } else if tag_len < 16 && c != ' ' && c != '\t' && c != '/' && c.is_ascii() {
                tag_name[tag_len] = c as u8;
                tag_len += 1;
            }
            i += 1;
            continue;
        }

        if c == '<' {
            in_tag = true;
            tag_len = 0;
            i += 1;
            continue;
        }

        // ── HTML entities ────────────────────────────────────────
        if c == '&' {
            // Look ahead for `;` within 10 chars
            if let Some(advanced_to) = scan_entity(&chars, i, text) {
                i = advanced_to + 1; // past the ';'
                if let Some(is_ws) = is_whitespace_entity(text, idx, chars[advanced_to].0) {
                    if is_ws {
                        // raw_end = position of `;` (exclusive of it), matches C# Flush(i)
                        let raw_end = chars[advanced_to].0;
                        flush_word(&mut buffer, &mut word_start, raw_end, visible_count, &mut on_word);
                        visible_count += 1;
                    }
                }
                continue;
            }
            // Malformed — skip the '&' silently
            i += 1;
            continue;
        }

        // ── Maqaf — word-joining hyphen acts as a separator ──────
        if c == '\u{05BE}' {
            // raw_end = position of separator (exclusive of it), matches C# Flush(i)
            flush_word(&mut buffer, &mut word_start, idx, visible_count, &mut on_word);
            visible_count += 1;
            i += 1;
            continue;
        }

        // ── Nikud + cantillation removal ─────────────────────────
        if ('\u{0591}'..='\u{05C7}').contains(&c)
            && c != '\u{05C0}'
            && c != '\u{05C3}'
            && c != '\u{05C6}'
        {
            i += 1;
            continue;
        }

        // Non-spacing marks above ASCII are dropped
        if (c as u32) > 127 && is_non_spacing_mark(c) {
            i += 1;
            continue;
        }

        // ── Word building ────────────────────────────────────────
        if is_letter(c) {
            let lc = if ('A'..='Z').contains(&c) {
                ((c as u8) | 32) as char
            } else {
                c
            };
            if buffer.is_empty() {
                word_start = idx;
            }
            buffer.push(lc);
            visible_count += 1;
        } else {
            // raw_end = position of separator (exclusive of it), matches C# Flush(i)
            flush_word(&mut buffer, &mut word_start, idx, visible_count, &mut on_word);
            visible_count += 1;
        }

        i += 1;
    }

    flush_word(&mut buffer, &mut word_start, total_len, visible_count, &mut on_word);
}

fn flush_word<F: FnMut(ScannedWord)>(
    buffer: &mut String,
    word_start: &mut usize,
    raw_end: usize,
    visible_count: usize,
    on_word: &mut F,
) {
    let char_count = buffer.chars().count();
    if char_count > 1 && char_count < 30 {
        let visible_start = visible_count - char_count;
        on_word(ScannedWord {
            raw_start: *word_start,
            raw_end,
            visible_start,
            normalized: buffer.as_str(),
        });
    }
    buffer.clear();
}

/// Hebrew letters (U+05D0–U+05EA) or ASCII a-z / A-Z.
pub fn is_letter(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '\u{05D0}'..='\u{05EA}')
}

/// Returns the index in `chars` of the entity's terminating `;`, or None if malformed.
fn scan_entity(chars: &[(usize, char)], amp_pos: usize, _text: &str) -> Option<usize> {
    let mut k = amp_pos + 1;
    let mut count = 0;
    while k < chars.len() && count < 10 {
        if chars[k].1 == ';' {
            return Some(k);
        }
        k += 1;
        count += 1;
    }
    None
}

/// Given the byte indices of `&` and `;`, returns Some(true) if the entity is a
/// whitespace separator. Returns None if not parseable. Returns Some(false) for
/// non-whitespace entities (caller treats them as invisible).
fn is_whitespace_entity(text: &str, amp_byte: usize, semi_byte: usize) -> Option<bool> {
    let inner = &text[amp_byte + 1..semi_byte];
    if inner.is_empty() {
        return Some(false);
    }
    if inner == "nbsp" || inner == "ensp" || inner == "emsp" {
        return Some(true);
    }
    if let Some(rest) = inner.strip_prefix('#') {
        if let Ok(val) = rest.parse::<u32>() {
            return Some(matches!(val, 160 | 8194 | 8195 | 8201));
        }
    }
    Some(false)
}

/// Block tag detection — matches FtsLib's HtmlScannerHelpers.IsBlockTag.
fn is_block_tag(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    let start = if name[0] == b'/' || name[0] == b'!' { 1 } else { 0 };
    let len = name.len() - start;
    if len == 0 {
        return false;
    }
    let lc = |b: u8| if b.is_ascii_uppercase() { b | 32 } else { b };
    let c0 = lc(name[start]);
    match len {
        1 => c0 == b'p',
        2 => {
            let c1 = lc(name[start + 1]);
            matches!(
                (c0, c1),
                (b'b', b'r') | (b'h', b'r') | (b'l', b'i') | (b'u', b'l')
                | (b'o', b'l') | (b't', b'r') | (b't', b'd') | (b't', b'h')
                | (b'd', b'd') | (b'd', b't')
            ) || (c0 == b'h' && (b'1'..=b'6').contains(&c1))
        }
        3 => {
            let c1 = lc(name[start + 1]);
            let c2 = lc(name[start + 2]);
            matches!(
                (c0, c1, c2),
                (b'd', b'i', b'v') | (b'p', b'r', b'e') | (b'n', b'a', b'v')
            )
        }
        4 => {
            let c1 = lc(name[start + 1]);
            let c2 = lc(name[start + 2]);
            let c3 = lc(name[start + 3]);
            (c0, c1, c2, c3) == (b'm', b'a', b'i', b'n')
        }
        5 => {
            let c1 = lc(name[start + 1]);
            let c2 = lc(name[start + 2]);
            let c3 = lc(name[start + 3]);
            let c4 = lc(name[start + 4]);
            matches!(
                (c0, c1, c2, c3, c4),
                (b't', b'a', b'b', b'l', b'e') | (b'a', b's', b'i', b'd', b'e')
            )
        }
        _ => {
            let buf: String = name[start..start + len]
                .iter()
                .map(|&b| lc(b) as char)
                .collect();
            matches!(
                buf.as_str(),
                "header" | "footer" | "figure" | "section"
                    | "article" | "caption" | "figcaption" | "blockquote"
            )
        }
    }
}

/// Quick check for Unicode non-spacing marks (Mn category) above ASCII.
/// We only need this for diacritical marks that aren't already caught by the
/// nikud range. Conservative: covers common combining-mark ranges.
fn is_non_spacing_mark(c: char) -> bool {
    let cp = c as u32;
    // Combining Diacritical Marks
    (0x0300..=0x036F).contains(&cp)
        // Combining Diacritical Marks Extended / Supplement
        || (0x1AB0..=0x1AFF).contains(&cp)
        || (0x1DC0..=0x1DFF).contains(&cp)
        // Combining Diacritical Marks for Symbols
        || (0x20D0..=0x20FF).contains(&cp)
        // Hebrew nikud range already handled above; Arabic combining marks:
        || (0x064B..=0x065F).contains(&cp)
        || (0x0670..=0x0670).contains(&cp)
        || (0x06D6..=0x06DC).contains(&cp)
        || (0x06DF..=0x06E4).contains(&cp)
        || (0x06E7..=0x06E8).contains(&cp)
        || (0x06EA..=0x06ED).contains(&cp)
}

/// Strip HTML tags from `text[from..to]` and append to `out` with HTML escape
/// for `&` and `>`. If `from` lands mid-tag (a `<` appears before any `>` when
/// scanning backwards), the partial tag is also skipped.
///
/// Mirrors FtsLib SnippetBuilder.AppendRawStripped.
pub fn append_stripped(text: &str, from: usize, to: usize, out: &mut String) {
    if from >= to || to > text.len() {
        return;
    }

    // Detect mid-tag start: scan backwards for '<' before any '>'
    let mut in_tag = false;
    let mut k = from;
    while k > 0 {
        k -= 1;
        let b = text.as_bytes()[k];
        if b == b'>' {
            break;
        }
        if b == b'<' {
            in_tag = true;
            break;
        }
    }

    let slice = &text[from..to];
    for c in slice.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            continue;
        }
        if c == '<' {
            in_tag = true;
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Strip HTML tags from `text` and HTML-escape `&` and `>`.
/// Used for the no-match fallback in snippet rendering.
pub fn encode_stripped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    append_stripped(text, 0, text.len(), &mut out);
    out
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(text: &str) -> Vec<(usize, usize, usize, String)> {
        let mut v = Vec::new();
        scan(text, |w| {
            v.push((w.raw_start, w.raw_end, w.visible_start, w.normalized.to_string()));
        });
        v
    }

    #[test]
    fn test_plain_hebrew() {
        let words = collect("שלום עולם");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].3, "שלום");
        assert_eq!(words[1].3, "עולם");
    }

    #[test]
    fn test_strips_nikud() {
        let words = collect("שָׁלוֹם");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].3, "שלום");
    }

    #[test]
    fn test_strips_inline_tags() {
        let words = collect("שלום<b>עולם</b>");
        // Inline tags don't act as separators, so words are joined visually
        // but the closing/opening of tags between letters does NOT join words —
        // because `<` triggers in_tag without flushing for inline tags.
        // FtsLib semantics: inline <b> doesn't separate, so "שלום" and "עולם"
        // would be joined into "שלוםעולם".
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].3, "שלוםעולם");
    }

    #[test]
    fn test_block_tag_separates() {
        let words = collect("<p>שלום</p><p>עולם</p>");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].3, "שלום");
        assert_eq!(words[1].3, "עולם");
    }

    #[test]
    fn test_maqaf_separates() {
        let words = collect("שלום\u{05BE}עולם");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].3, "שלום");
        assert_eq!(words[1].3, "עולם");
    }

    #[test]
    fn test_nbsp_separates() {
        let words = collect("שלום&nbsp;עולם");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].3, "שלום");
        assert_eq!(words[1].3, "עולם");
    }

    #[test]
    fn test_lowercases_ascii() {
        let words = collect("Hello World");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].3, "hello");
        assert_eq!(words[1].3, "world");
    }

    #[test]
    fn test_short_word_filtered() {
        // FtsLib filters words with len < 2 or >= 30
        let words = collect("a שלום");
        assert_eq!(words.len(), 1); // "a" filtered
        assert_eq!(words[0].3, "שלום");
    }

    #[test]
    fn test_long_word_filtered() {
        let long_word: String = "א".repeat(35);
        let text = format!("{} שלום", long_word);
        let words = collect(&text);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].3, "שלום");
    }

    #[test]
    fn test_append_stripped_basic() {
        let mut out = String::new();
        append_stripped("<p>hello</p>", 0, 12, &mut out);
        assert_eq!(out, "hello");
    }

    #[test]
    fn test_append_stripped_amp_escape() {
        let mut out = String::new();
        append_stripped("a & b", 0, 5, &mut out);
        assert_eq!(out, "a &amp; b");
    }

    #[test]
    fn test_append_stripped_mid_tag() {
        let mut out = String::new();
        // from=2 lands inside "<p>" — backward scan should find '<'
        append_stripped("<p>hello", 2, 8, &mut out);
        assert_eq!(out, "hello");
    }

    #[test]
    fn test_visible_start_increments() {
        let words = collect("שלום עולם");
        assert_eq!(words[0].2, 0); // first word at 0
        assert_eq!(words[1].2, 5); // 4 chars + 1 space
    }
}
