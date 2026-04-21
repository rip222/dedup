//! Tier B alpha-rename diff tinting (issue #25).
//!
//! The Tier B normaliser in `dedup_lang` assigns a 1-based placeholder
//! index (`v1`, `v2`, ...) to each alpha-renamed local. Within a match
//! group, every occurrence shares the same canonical token stream, so
//! the same placeholder index refers to the same logical local across
//! every file in the group. That's the correspondence key: paint
//! `placeholder_idx == 1` the same color in every occurrence and the
//! eye immediately sees which identifier in file A maps to which in
//! file B.
//!
//! Tier A groups never alpha-rename and carry no span data, so this
//! module's painting path is skipped for them entirely — the data layer
//! guarantees `alpha_rename_spans` is empty for Tier A occurrences
//! (see `dedup_core::Occurrence`).
//!
//! # Palette
//!
//! Ten pastel fills, picked to:
//!
//! 1. Be visually distinguishable from each other at glance-level, even
//!    after a couple of minutes of staring at a duplicate group.
//! 2. Be readable as **backgrounds** behind `highlight::Highlight`
//!    foregrounds in both the dark sidebar theme (current default) and
//!    in a future light theme (TODO: light-palette variant).
//! 3. Avoid clashing with the existing highlight foreground colors —
//!    the foreground palette is saturated / mid-tone; these tints are
//!    low-saturation / low-value so the syntax color still reads.
//!
//! Colors are stored as 0xAARRGGBB so the alpha component is part of
//! the literal. In GPUI we layer them as container backgrounds; the
//! alpha controls how much of the dark sidebar bleeds through.
//!
//! The palette wraps — `placeholder_idx > PALETTE.len()` is reduced
//! modulo the palette length. With the normaliser's 1-based indices
//! this means `v1 → PALETTE[0]`, `v2 → PALETTE[1]`, ..., which is
//! stable and deterministic.

/// Number of distinct tint slots. Chosen so a typical function's worth
/// of locals (<10) each get a unique color; wrap past that is a minor
/// visual degradation, not correctness.
pub const PALETTE_LEN: usize = 10;

/// Dark-theme pastel palette. Stored as 0xRRGGBB (no alpha — alpha is
/// applied at render time via `tint_argb`). Hand-picked; each entry's
/// HSL lightness sits in ~0.35-0.50 so the gray-on-color contrast is
/// readable with the existing foreground palette.
///
/// TODO (#25 follow-up): a companion light-theme palette will land
/// when we wire theme switching in the GUI. The sidebar is dark by
/// default (#24), so a single palette is enough at this milestone.
const PALETTE_DARK_RGB: [u32; PALETTE_LEN] = [
    0x4a3340, // muted rose
    0x3a4a2f, // olive green
    0x2f3a52, // slate blue
    0x523a2f, // warm umber
    0x3a2f52, // violet
    0x2f5252, // teal
    0x524a2f, // ochre
    0x2f5240, // jade
    0x4a2f52, // plum
    0x403a52, // dusk indigo
];

/// Return the RGB tint color (0xRRGGBB) for a 1-based placeholder
/// index. Deterministic: the same `idx` always returns the same color.
///
/// `idx == 0` is treated as "no tint" and returns the default fill
/// `PALETTE_DARK_RGB[0]` — callers should filter zeros out before
/// calling, but the function never panics.
pub fn tint_for_placeholder(idx: u32) -> u32 {
    if idx == 0 {
        return PALETTE_DARK_RGB[0];
    }
    let slot = ((idx - 1) as usize) % PALETTE_LEN;
    PALETTE_DARK_RGB[slot]
}

/// Iterate the dark palette, oldest slot first. Test helper only — the
/// renderer calls [`tint_for_placeholder`] directly.
#[cfg(test)]
pub fn palette_dark() -> &'static [u32; PALETTE_LEN] {
    &PALETTE_DARK_RGB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tint_is_deterministic() {
        for idx in 1u32..50 {
            assert_eq!(tint_for_placeholder(idx), tint_for_placeholder(idx));
        }
    }

    #[test]
    fn tint_wraps_palette() {
        // idx=1 and idx=1+PALETTE_LEN must produce the same color.
        assert_eq!(
            tint_for_placeholder(1),
            tint_for_placeholder(1 + PALETTE_LEN as u32),
        );
        assert_eq!(
            tint_for_placeholder(2),
            tint_for_placeholder(2 + PALETTE_LEN as u32),
        );
    }

    #[test]
    fn distinct_adjacent_placeholders_get_distinct_colors() {
        // The painter relies on adjacent placeholder indices rendering
        // visibly differently. We assert uniqueness across the full
        // palette — a stronger property than strict adjacency — so a
        // future palette tweak can't accidentally collapse entries.
        let colors: std::collections::HashSet<u32> =
            (1..=PALETTE_LEN as u32).map(tint_for_placeholder).collect();
        assert_eq!(
            colors.len(),
            PALETTE_LEN,
            "palette entries must all be distinct"
        );
    }

    #[test]
    fn palette_avoids_pure_black_and_pure_white() {
        // Pure black (0x000000) / pure white (0xffffff) backgrounds would
        // either swallow or wash out the foreground highlighter colors
        // in the detail view. Sanity-check the palette isn't one of
        // them — the AC asks for colors readable against the existing
        // highlight palette.
        for (i, &c) in palette_dark().iter().enumerate() {
            assert_ne!(c, 0x000000, "palette[{i}] is pure black");
            assert_ne!(c, 0xffffff, "palette[{i}] is pure white");
            // Loose contrast: sum of R+G+B > a trivial threshold so
            // the tint has at least some chroma.
            let r = (c >> 16) & 0xff;
            let g = (c >> 8) & 0xff;
            let b = c & 0xff;
            assert!(
                r + g + b >= 0x40,
                "palette[{i}] = {c:06x} is too close to black",
            );
        }
    }

    #[test]
    fn tint_for_zero_does_not_panic() {
        // Defensive: `placeholder_idx == 0` should never reach the tint
        // function (alpha-rename indices are 1-based), but we still
        // want a non-panicking fallback.
        let _ = tint_for_placeholder(0);
    }
}
