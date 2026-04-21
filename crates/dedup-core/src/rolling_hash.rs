//! Rabin-Karp-style rolling hash over token n-grams.
//!
//! Given a slice of [`Token`]s and a window size `n`, emits one [`Hash`] per
//! length-`n` window, paired with the [`Span`] covered by that window. The
//! hash is a 64-bit polynomial rolling hash computed over each token's
//! canonical content (`kind` tag + normalized text). Two windows that hash
//! equal are almost certainly the same token sequence; downstream code
//! should still do an exact token-stream compare for confirmed matches
//! (Tier B / #6 handles that; Tier A as specified in the PRD tolerates
//! the false-positive risk).
//!
//! Pure and deterministic: no randomness, no global state.

use crate::tokenizer::{Token, TokenKind};

/// 64-bit window hash.
pub type Hash = u64;

/// The source region covered by a rolling-hash window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// 1-based line of the first token in the window.
    pub start_line: usize,
    /// 1-based line of the last token in the window.
    pub end_line: usize,
    /// Byte offset (inclusive) of the first token.
    pub start_byte: usize,
    /// Byte offset (exclusive) of the last token.
    pub end_byte: usize,
}

/// Mixing constants for the polynomial rolling hash. Chosen as a large odd
/// multiplier and a non-trivial seed so empty windows still hash distinctly.
const MUL: u64 = 0x100000001b3; // FNV-1a prime; works fine as a polynomial base.
const SEED: u64 = 0xcbf29ce484222325;

/// Compute the canonical per-token hash contribution.
#[inline]
fn token_hash(tok: &Token) -> u64 {
    // Kind tag so `identifier x` and `string x` don't collide.
    let kind_tag: u64 = match tok.kind {
        TokenKind::Identifier => 1,
        TokenKind::Number => 2,
        TokenKind::String => 3,
        TokenKind::Punct => 4,
    };
    let mut h = SEED ^ kind_tag;
    for &b in tok.text.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(MUL);
    }
    h
}

/// Roll a window of size `n` across `tokens` and emit `(hash, span)` pairs.
///
/// Returns an empty vector if `n == 0` or `tokens.len() < n`.
pub fn rolling_hash(tokens: &[Token], n: usize) -> Vec<(Hash, Span)> {
    if n == 0 || tokens.len() < n {
        return Vec::new();
    }

    // Per-token hash table.
    let hashes: Vec<u64> = tokens.iter().map(token_hash).collect();

    // The multiplier raised to the window length, used to subtract the
    // departing token's contribution when sliding.
    let mut mul_n: u64 = 1;
    for _ in 0..n {
        mul_n = mul_n.wrapping_mul(MUL);
    }

    let mut out = Vec::with_capacity(tokens.len() - n + 1);

    // Seed the first window.
    let mut acc: u64 = 0;
    for h in hashes.iter().take(n) {
        acc = acc.wrapping_mul(MUL).wrapping_add(*h);
    }

    let push = |out: &mut Vec<(Hash, Span)>, acc: u64, start_idx: usize| {
        let first = &tokens[start_idx];
        let last = &tokens[start_idx + n - 1];
        out.push((
            acc,
            Span {
                start_line: first.line,
                end_line: last.line,
                start_byte: first.start,
                end_byte: last.end,
            },
        ));
    };

    push(&mut out, acc, 0);

    // Slide the window: for each new end index, drop the departing token
    // and mix in the new one. `acc_{i+1} = acc_i * MUL - out * MUL^n + in`.
    for i in 1..=tokens.len() - n {
        let out_hash = hashes[i - 1];
        let in_hash = hashes[i + n - 1];
        acc = acc
            .wrapping_mul(MUL)
            .wrapping_sub(out_hash.wrapping_mul(mul_n))
            .wrapping_add(in_hash);
        push(&mut out, acc, i);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::tokenize;

    #[test]
    fn empty_when_window_larger_than_input() {
        let toks = tokenize("a b c");
        assert!(rolling_hash(&toks, 4).is_empty());
    }

    #[test]
    fn empty_when_n_zero() {
        let toks = tokenize("a b c");
        assert!(rolling_hash(&toks, 0).is_empty());
    }

    #[test]
    fn exact_one_window_when_equal_length() {
        let toks = tokenize("a b c");
        let hs = rolling_hash(&toks, 3);
        assert_eq!(hs.len(), 1);
    }

    #[test]
    fn same_tokens_hash_equal() {
        let a = tokenize("fn foo(x: i32) { return x + 1; }");
        let b = tokenize("fn foo(x: i32) { return x + 1; }");
        let ha = rolling_hash(&a, 4);
        let hb = rolling_hash(&b, 4);
        assert_eq!(
            ha.iter().map(|(h, _)| *h).collect::<Vec<_>>(),
            hb.iter().map(|(h, _)| *h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn different_tokens_hash_distinct() {
        let a = rolling_hash(&tokenize("a b c d"), 3);
        let b = rolling_hash(&tokenize("a b c e"), 3);
        // At least one of the overlapping windows must differ.
        assert_ne!(a, b);
    }

    #[test]
    fn span_byte_ranges_cover_input() {
        let src = "alpha beta gamma";
        let toks = tokenize(src);
        let hs = rolling_hash(&toks, 3);
        assert_eq!(hs.len(), 1);
        let span = hs[0].1;
        assert_eq!(&src[span.start_byte..span.end_byte], "alpha beta gamma");
    }

    #[test]
    fn sliding_hash_matches_naive_recompute() {
        // For each window position, the rolling accumulator should equal
        // the polynomial hash recomputed from scratch over the same window.
        let src = "a b c d e f g h i j";
        let toks = tokenize(src);
        let rolled = rolling_hash(&toks, 4);
        for (i, (rolled_h, _)) in rolled.iter().enumerate() {
            let mut acc: u64 = 0;
            for tok in &toks[i..i + 4] {
                acc = acc.wrapping_mul(MUL).wrapping_add(token_hash(tok));
            }
            assert_eq!(*rolled_h, acc, "mismatch at window {i}");
        }
    }

    // --- proptest invariants -------------------------------------------------

    use proptest::prelude::*;

    fn ident_source() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a"),
                Just("b"),
                Just("c"),
                Just("d"),
                Just("e"),
                Just("+"),
                Just(";"),
                Just("("),
                Just(")"),
                Just(" "),
                Just("\n"),
            ],
            3..60,
        )
        .prop_map(|v| v.into_iter().collect::<String>())
    }

    proptest! {
        /// Determinism: same input → same output, byte-for-byte.
        #[test]
        fn stable_under_identical_input(src in ident_source(), n in 1usize..6) {
            let toks = tokenize(&src);
            prop_assert_eq!(rolling_hash(&toks, n), rolling_hash(&toks, n));
        }

        /// Each emitted span must cover exactly n tokens and the recovered
        /// byte range must start at tokens[i].start and end at tokens[i+n-1].end.
        #[test]
        fn span_covers_exactly_n_tokens(src in ident_source(), n in 1usize..6) {
            let toks = tokenize(&src);
            let hs = rolling_hash(&toks, n);
            if toks.len() < n {
                prop_assert!(hs.is_empty());
                return Ok(());
            }
            prop_assert_eq!(hs.len(), toks.len() - n + 1);
            for (i, (_, span)) in hs.iter().enumerate() {
                prop_assert_eq!(span.start_byte, toks[i].start);
                prop_assert_eq!(span.end_byte, toks[i + n - 1].end);
                prop_assert_eq!(span.start_line, toks[i].line);
                prop_assert_eq!(span.end_line, toks[i + n - 1].line);
            }
        }

        /// Rolling the window one step at a time must agree with recomputing
        /// the polynomial hash from scratch at each position.
        #[test]
        fn rolling_matches_direct_computation(src in ident_source(), n in 1usize..6) {
            let toks = tokenize(&src);
            let hs = rolling_hash(&toks, n);
            for (i, (rolled, _)) in hs.iter().enumerate() {
                let mut acc: u64 = 0;
                for tok in &toks[i..i + n] {
                    acc = acc.wrapping_mul(MUL).wrapping_add(token_hash(tok));
                }
                prop_assert_eq!(*rolled, acc);
            }
        }
    }
}
