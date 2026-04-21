//! Language-oblivious tokenizer for Tier A detection.
//!
//! Splits source text into [`Token`]s of four kinds (identifier, number, string,
//! punctuation) while stripping whitespace and comments. Comment heuristics
//! cover the pragmatic set of languages used in most real-world codebases:
//! `//` line comments, `/* ... */` block comments (C / JS / TS / Rust / Go /
//! Swift / Kotlin), and `#` line comments (Python / shell / Ruby / TOML / YAML).
//! String literals use `"..."` or `'...'` delimiters with a single-character
//! `\` escape. The tokenizer is intentionally permissive: Tier A is a
//! heuristic pass, and false positives / false negatives on pathological
//! input (e.g. a `#` inside a Rust attribute) are acceptable per the PRD.

/// Kind of a lexical token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// An identifier: `[A-Za-z_][A-Za-z0-9_]*` (plus non-ASCII letters).
    Identifier,
    /// A numeric literal: `[0-9]` followed by any `[A-Za-z0-9_.]` run.
    Number,
    /// A string literal, delimiter-included, quotes preserved verbatim.
    String,
    /// Any other non-whitespace byte (operator, bracket, comma, ...).
    Punct,
}

/// A single lexical token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// What sort of token this is.
    pub kind: TokenKind,
    /// Start byte offset (inclusive) into the source.
    pub start: usize,
    /// End byte offset (exclusive) into the source.
    pub end: usize,
    /// 1-based line number the token starts on.
    pub line: usize,
    /// Lexeme text, verbatim (including quotes for strings).
    pub text: String,
}

/// Tokenize `source` into a whitespace-and-comment-stripped token stream.
///
/// Lines are 1-based. Byte offsets are relative to the start of `source`.
pub fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1usize;

    while i < bytes.len() {
        let b = bytes[i];

        // Newline: advance the line counter.
        if b == b'\n' {
            line += 1;
            i += 1;
            continue;
        }

        // Other whitespace: skip.
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // `//` line comment.
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // `/* ... */` block comment.
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            // Consume the closing `*/` if present (unterminated comments eat to EOF).
            if i + 1 < bytes.len() {
                i += 2;
            } else {
                i = bytes.len();
            }
            continue;
        }

        // `#` line comment (Python / shell / Ruby / TOML / YAML).
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // String literal `"..."` or `'...'` with `\` escapes.
        if b == b'"' || b == b'\'' {
            let quote = b;
            let start = i;
            let start_line = line;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    if bytes[i + 1] == b'\n' {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            // Consume the closing quote if present.
            if i < bytes.len() {
                i += 1;
            }
            out.push(Token {
                kind: TokenKind::String,
                start,
                end: i,
                line: start_line,
                text: source[start..i].to_string(),
            });
            continue;
        }

        // Numeric literal: ASCII digit run, plus identifier-continuation bytes
        // so things like `0x1F`, `1_000`, `3.14`, `1e-3` stay one token. The
        // exponent-sign case (`1e-3`) requires a tiny lookahead.
        if b.is_ascii_digit() {
            let start = i;
            let start_line = line;
            while i < bytes.len() {
                let c = bytes[i];
                let is_cont = c.is_ascii_alphanumeric() || c == b'_' || c == b'.';
                let is_exp_sign = (c == b'+' || c == b'-')
                    && i > start
                    && (bytes[i - 1] == b'e' || bytes[i - 1] == b'E');
                if is_cont || is_exp_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(Token {
                kind: TokenKind::Number,
                start,
                end: i,
                line: start_line,
                text: source[start..i].to_string(),
            });
            continue;
        }

        // Identifier: leading ASCII letter or `_`, or any non-ASCII UTF-8 lead byte.
        if is_ident_start(b) {
            let start = i;
            let start_line = line;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            out.push(Token {
                kind: TokenKind::Identifier,
                start,
                end: i,
                line: start_line,
                text: source[start..i].to_string(),
            });
            continue;
        }

        // Anything else is a one-byte punctuation token. UTF-8 continuation
        // bytes (high bit set, but not an identifier start per our check)
        // are rare in punctuation and safe to treat as a single byte here.
        let start = i;
        let start_line = line;
        // Advance by a full UTF-8 code point to avoid slicing inside one.
        let step = utf8_char_len(b);
        i += step;
        let end = i.min(bytes.len());
        out.push(Token {
            kind: TokenKind::Punct,
            start,
            end,
            line: start_line,
            text: source[start..end].to_string(),
        });
    }

    out
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

#[inline]
fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

#[inline]
fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        // Continuation byte in isolation; treat as 1 to make progress.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src).into_iter().map(|t| t.kind).collect()
    }

    fn texts(src: &str) -> Vec<String> {
        tokenize(src).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn strips_whitespace() {
        let toks = tokenize("  foo   bar\n\tbaz   ");
        assert_eq!(
            toks.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["foo", "bar", "baz"]
        );
    }

    #[test]
    fn tracks_line_numbers() {
        let toks = tokenize("a\nb\n\nc");
        assert_eq!(toks[0].line, 1);
        assert_eq!(toks[1].line, 2);
        assert_eq!(toks[2].line, 4);
    }

    #[test]
    fn line_comment_slash() {
        let toks = tokenize("a // comment\nb");
        assert_eq!(texts("a // comment\nb"), vec!["a", "b"]);
        assert_eq!(toks[1].line, 2);
    }

    #[test]
    fn block_comment() {
        let toks = tokenize("a /* block\ncomment */ b");
        assert_eq!(
            toks.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(toks[1].line, 2, "line counter must span the block comment");
    }

    #[test]
    fn line_comment_hash() {
        assert_eq!(texts("a # python-style\nb"), vec!["a", "b"]);
    }

    #[test]
    fn string_literal_preserved_verbatim() {
        let toks = tokenize(r#"let s = "hello world";"#);
        let s = toks
            .iter()
            .find(|t| t.kind == TokenKind::String)
            .expect("string token");
        assert_eq!(s.text, "\"hello world\"");
    }

    #[test]
    fn string_with_escaped_quote() {
        let toks = tokenize(r#""a\"b""#);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::String);
        assert_eq!(toks[0].text, r#""a\"b""#);
    }

    #[test]
    fn single_quoted_string() {
        let toks = tokenize(r#"'hi'"#);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::String);
    }

    #[test]
    fn comment_markers_inside_strings_are_not_comments() {
        let toks = tokenize(r#""// not a comment" x"#);
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].kind, TokenKind::String);
        assert_eq!(toks[1].kind, TokenKind::Identifier);
    }

    #[test]
    fn numbers_and_decimals() {
        assert_eq!(
            kinds("42 3.14 0x1F 1_000 1e-3"),
            vec![
                TokenKind::Number,
                TokenKind::Number,
                TokenKind::Number,
                TokenKind::Number,
                TokenKind::Number
            ]
        );
    }

    #[test]
    fn punctuation_tokens() {
        let toks = tokenize("a+b;");
        assert_eq!(
            toks.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![
                TokenKind::Identifier,
                TokenKind::Punct,
                TokenKind::Identifier,
                TokenKind::Punct
            ]
        );
    }

    #[test]
    fn byte_spans_round_trip() {
        let src = "foo  bar";
        let toks = tokenize(src);
        assert_eq!(&src[toks[0].start..toks[0].end], "foo");
        assert_eq!(&src[toks[1].start..toks[1].end], "bar");
    }

    #[test]
    fn non_ascii_identifier() {
        let toks = tokenize("αβγ+δ");
        // Expect two identifier tokens separated by punctuation.
        assert_eq!(
            toks.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![
                TokenKind::Identifier,
                TokenKind::Punct,
                TokenKind::Identifier
            ]
        );
    }

    #[test]
    fn comment_stripping_preserves_non_comment_token_count() {
        let no_comments = "a b c d e";
        let with_comments = "a /* x y z */ b // noise\nc # also\nd e";
        assert_eq!(tokenize(no_comments).len(), tokenize(with_comments).len());
    }

    // --- proptest invariants -------------------------------------------------

    use proptest::prelude::*;

    fn strip_trailing_ws(s: &str) -> String {
        s.lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Restrict to simple ASCII letters + spaces + newlines to keep the
    /// whitespace-invariance property crisp. Comment markers, quotes, and
    /// escapes are excluded so we can't accidentally generate a string
    /// literal or comment mid-shuffle.
    fn simple_source() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a".to_string()),
                Just("b".to_string()),
                Just("c".to_string()),
                Just("ident".to_string()),
                Just("xyz".to_string()),
                Just(" ".to_string()),
                Just("  ".to_string()),
                Just("\t".to_string()),
                Just("\n".to_string()),
            ],
            0..40,
        )
        .prop_map(|v| v.concat())
    }

    proptest! {
        /// Whitespace-only edits must not change the token kinds or texts.
        #[test]
        fn whitespace_edits_preserve_tokens(src in simple_source()) {
            let original = tokenize(&src);
            let stripped = tokenize(&strip_trailing_ws(&src));
            let orig_texts: Vec<_> = original.iter().map(|t| (t.kind, t.text.clone())).collect();
            let strip_texts: Vec<_> = stripped.iter().map(|t| (t.kind, t.text.clone())).collect();
            prop_assert_eq!(orig_texts, strip_texts);
        }

        /// Tokenization is deterministic.
        #[test]
        fn deterministic(src in simple_source()) {
            prop_assert_eq!(tokenize(&src), tokenize(&src));
        }

        /// Collapsing runs of interior whitespace to a single space must not
        /// change the token stream (kinds + texts).
        #[test]
        fn collapsing_runs_of_whitespace_preserves_tokens(src in simple_source()) {
            let collapsed: String = {
                let mut out = String::new();
                let mut prev_ws = false;
                for ch in src.chars() {
                    if ch.is_ascii_whitespace() {
                        if !prev_ws { out.push(' '); }
                        prev_ws = true;
                    } else {
                        out.push(ch);
                        prev_ws = false;
                    }
                }
                out
            };
            let a: Vec<_> = tokenize(&src).into_iter().map(|t| (t.kind, t.text)).collect();
            let b: Vec<_> = tokenize(&collapsed).into_iter().map(|t| (t.kind, t.text)).collect();
            prop_assert_eq!(a, b);
        }
    }
}
