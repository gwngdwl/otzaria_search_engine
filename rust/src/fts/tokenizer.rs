/// Custom tantivy tokenizer that mirrors FtsLib's Tokenizer (HtmlTextScanner).
///
/// At index time it strips HTML tags, nikud/cantillation, and emits normalized
/// Hebrew + lowercase ASCII tokens. The same normalization is applied to query
/// terms by `fts_query::normalise`, so index and query terms align.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

use crate::fts::html::{scan, ScannedWord};

#[derive(Clone, Default)]
pub struct HebrewTokenizer;

pub struct HebrewTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
}

impl Tokenizer for HebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let mut tokens = Vec::new();
        let mut position = 0usize;
        scan(text, |w: ScannedWord| {
            tokens.push(Token {
                offset_from: w.raw_start,
                offset_to: w.raw_end,
                position,
                text: w.normalized.to_string(),
                position_length: 1,
            });
            position += 1;
        });
        HebrewTokenStream { tokens, cursor: 0 }
    }
}

impl TokenStream for HebrewTokenStream {
    fn advance(&mut self) -> bool {
        if self.cursor >= self.tokens.len() {
            self.cursor = self.tokens.len() + 1;
            return false;
        }
        self.cursor += 1;
        true
    }

    fn token(&self) -> &Token {
        // After advance() returned true, cursor points one past the current token.
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        let idx = self.cursor - 1;
        &mut self.tokens[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_tokens(text: &str) -> Vec<String> {
        let mut tk = HebrewTokenizer;
        let mut stream = tk.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn test_basic() {
        let tokens = collect_tokens("שלום עולם");
        assert_eq!(tokens, vec!["שלום", "עולם"]);
    }

    #[test]
    fn test_strips_nikud() {
        let tokens = collect_tokens("שָׁלוֹם");
        assert_eq!(tokens, vec!["שלום"]);
    }

    #[test]
    fn test_strips_html() {
        let tokens = collect_tokens("<p>שלום</p> <p>עולם</p>");
        assert_eq!(tokens, vec!["שלום", "עולם"]);
    }

    #[test]
    fn test_lowercase_ascii() {
        let tokens = collect_tokens("Hello World");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_positions_increment() {
        let mut tk = HebrewTokenizer;
        let mut stream = tk.token_stream("שלום עולם חבר");
        let mut positions = Vec::new();
        while stream.advance() {
            positions.push(stream.token().position);
        }
        assert_eq!(positions, vec![0, 1, 2]);
    }
}
