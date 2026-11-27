//! Fonts, shaping and line breaking for Fagus.
//!
//! A [`FontDb`] holds the fonts of an engine for its whole lifetime. Faces are parsed once with
//! read-fonts (lazily, without copying the data), shaped with harfrust, and shaping results are
//! cached in font units so they can be reused for every size and document. A document's own
//! fonts (`@font-face`) go into a clone of the engine's database, which shares its faces and its
//! shaping cache.

#![forbid(unsafe_code)]

mod cascade;
mod emoji;
mod hvgl;
mod hyphenation;
mod linebreak;
mod math;
mod palette;
mod phrase;
mod platform;
mod system;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harfrust::font::name::NameId;
use harfrust::{Buffer, Direction, Feature, Font, ShapeOptions, ShapePlan, ShaperFont, Tag};
use rustc_hash::{FxHashMap, FxHashSet};

use cascade::Cascade;
use platform::{Generic, LangScript};
use system::SystemFonts;

pub use emoji::{Presentation, is_variation_selector, presentation, sequence_len};
pub use hyphenation::{hyphenate_word, hyphenation_points_limited};
pub use linebreak::{
    BreakOpportunity, Strictness, break_all_opportunities, break_opportunities, break_opportunities_with,
    breaks_all_at, keeps_all_at, phrase_opportunities_with,
};
pub use math::{GlyphPart, MathConstant};
pub use palette::{BasePalette, FontPalette, PaletteColor, PaletteValues, with_palette};
pub use phrase::{is_phrase_letter, phrase_breaks};

/// The fonts built into Fagus: Inter (SIL Open Font License 1.1, see `fonts/Inter-LICENSE.txt`)
/// in regular and bold, without hinting, which PDF viewers do not use.
const DEFAULT_FONTS: [&[u8]; 2] = [
    include_bytes!("../fonts/Inter-Regular.ttf"),
    include_bytes!("../fonts/Inter-Bold.ttf"),
];

/// Index of a face in a [`FontDb`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FaceId(u32);

/// The bit of a [`FaceId`] that marks a face of the system's fonts; the other bits number it
/// among them.
const SYSTEM_FACE: u32 = 1 << 31;

impl FaceId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Where the outlines of a face's glyphs come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outlines {
    /// A `glyf`, `CFF` or `CFF2` table, which PDF fonts embed as it is.
    Standard,
    /// Apple's `hvgl` table (PingFang), which only Fagus reads: a PDF embeds the outlines
    /// Fagus draws from it.
    Hvgl,
    /// None, but glyphs in color or as images (`COLR`, `sbix`, `CBDT`, `EBDT`, `SVG `), as
    /// emoji fonts have them.
    Color,
    /// Nothing that Fagus can draw, as in bitmap fonts of Apple's own format (`bdat`).
    None,
}

impl Outlines {
    /// Where the glyphs of a font come from.
    fn of(font: &skrifa::FontRef<'_>) -> Self {
        use skrifa::raw::TableProvider;
        let has = |tag: &[u8; 4]| font.table_data(Tag::new(tag)).is_some();
        if font.glyf().is_ok() || font.cff().is_ok() || font.cff2().is_ok() {
            Self::Standard
        } else if font
            .table_data(Tag::new(b"hvgl"))
            .is_some_and(|table| hvgl::Hvgl::new(table.as_bytes()).is_some())
        {
            Self::Hvgl
        } else if [b"COLR", b"sbix", b"CBDT", b"EBDT", b"SVG "].into_iter().any(has) {
            Self::Color
        } else {
            Self::None
        }
    }
}

/// The slant of a face, as used for font matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    /// Slanted by an angle in tenths of a degree, clockwise as CSS turns it (`oblique` alone is
    /// 14 degrees, [`FontStyle::OBLIQUE`]).
    Oblique(i16),
}

impl FontStyle {
    /// `oblique` without an angle: 14 degrees.
    pub const OBLIQUE: Self = Self::Oblique(140);

    /// `oblique` by `degrees`, within the -90 to 90 degrees that CSS allows.
    pub fn oblique(degrees: f32) -> Self {
        let tenths = (degrees.clamp(-90.0, 90.0) * 10.0).round();
        Self::Oblique(if tenths.is_finite() { tenths as i16 } else { 140 })
    }

    /// The slant in degrees as font matching sees it (Blink's slope): 0 for normal, 14 for
    /// italic and `oblique`, else the angle of `oblique`.
    pub fn slope(self) -> f32 {
        match self {
            Self::Normal => 0.0,
            Self::Italic => ITALIC_SLOPE,
            Self::Oblique(tenths) => f32::from(tenths) / 10.0,
        }
    }

    /// Whether the style slants text at all.
    pub fn is_slanted(self) -> bool {
        self != Self::Normal
    }
}

/// The slope of italic text and the slope from which on a face counts as italic (Blink's
/// `kItalicSlopeValue` and `kItalicThreshold`), in degrees.
const ITALIC_SLOPE: f32 = 14.0;

/// Vertical metrics of a face, in font units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceMetrics {
    pub units_per_em: f32,
    /// Distance from the baseline to the top of the line box (positive).
    pub ascender: f32,
    /// Distance from the baseline to the bottom of the line box (positive).
    pub descender: f32,
    pub line_gap: f32,
    /// The typographic ascender and descender of the OS/2 table (the descender positive), 0
    /// where the face has none.
    pub typo_ascender: f32,
    pub typo_descender: f32,
    pub x_height: f32,
    pub cap_height: f32,
    /// The average width of a character of the OS/2 table (`xAvgCharWidth`), 0 where the face
    /// has none.
    pub average_width: f32,
    /// The width of the box around all glyphs of the `head` table (`xMax - xMin`).
    pub bounding_width: f32,
    /// Position of the underline below the baseline (positive = below).
    pub underline_position: f32,
    pub underline_thickness: f32,
    /// Position of the strike-through line above the baseline.
    pub strikeout_position: f32,
    pub strikeout_thickness: f32,
}

/// A variation axis of a variable font, in its user coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Axis {
    pub tag: [u8; 4],
    pub min: f32,
    pub default: f32,
    pub max: f32,
}

/// Coordinates on variation axes: tags and values.
pub type Variations = [([u8; 4], f32)];

/// A font face: one font of a file, with the properties used for matching.
#[allow(clippy::struct_excessive_bools, reason = "independent properties of the face")]
pub struct Face {
    pub family: String,
    pub weight: f32,
    /// The weights the face covers: one weight, or a range for variable fonts and `@font-face`
    /// rules with a range.
    pub weight_range: (f32, f32),
    pub style: FontStyle,
    /// The slants the face covers in degrees (Blink's slope): 0 for normal faces, 14 for italic
    /// and oblique ones, or the range of an `@font-face` rule's `font-style: oblique`.
    pub slope: (f32, f32),
    /// Whether the face's weights are those of `font-weight: auto` (an `@font-face` rule
    /// without one): a variable face then takes any weight of its `wght` axis without
    /// synthetic bold.
    pub weight_auto: bool,
    /// Whether the face's slants are those of `font-style: auto` (an `@font-face` rule without
    /// one): a face with a `slnt` axis then takes any slant of the axis without synthetic
    /// italics.
    pub slope_auto: bool,
    /// Whether the font itself is bold (weight 600 or more) and italic, whatever an
    /// `@font-face` rule says: such a font is not made bolder or slanted.
    pub own_bold: bool,
    pub own_italic: bool,
    /// How much larger than the font size the face is set: its `@font-face` rule's
    /// `size-adjust`, else 1. [`Face::scale`] includes it.
    pub size_adjust: f32,
    /// The OpenType features of its `@font-face` rule's `font-feature-settings`, which come
    /// before those of the style (CSS Fonts 4, §7.2).
    pub features: Vec<([u8; 4], u32)>,
    /// Whether the face comes from a document (`@font-face`). Such faces hide installed faces of
    /// the same family.
    pub document: bool,
    /// Whether the face is one of the fonts built into Fagus, which any other face of the same
    /// family hides.
    pub builtin: bool,
    /// Unique among all faces of the process: the key of shared caches.
    key: u64,
    /// The widths the face covers in percent of normal: one width, or a range for variable
    /// fonts and `@font-face` rules with a range.
    pub stretch: (f32, f32),
    /// The variation axes of a variable font; empty for others.
    pub axes: Vec<Axis>,
    /// The weights and widths that the `wght` and `wdth` axes of a variable face may take: the
    /// ranges of its `@font-face` rule, or of the axes where the rule gives none.
    variable: ((f32, f32), (f32, f32)),
    /// The characters the face is used for (`unicode-range`), as inclusive ranges; empty for all.
    unicode_range: Vec<(u32, u32)>,
    /// The glyph substitution features of the face (GSUB), sorted.
    substitutions: Vec<[u8; 4]>,
    /// The glyph positioning features of the face (GPOS), sorted.
    positionings: Vec<[u8; 4]>,
    /// Whether the face has features for particular languages (language systems in its GSUB or
    /// GPOS table): only then does the language of text change how it is shaped.
    languages: bool,
    /// Whether the face has metrics for vertical text (a `vmtx` table).
    pub vertical_metrics: bool,
    /// Whether the face gives the vertical origins of its glyphs (a `VORG` table); without one
    /// they come from the tops of the glyphs and their top side bearings.
    pub vertical_origins: bool,
    /// Whether the face draws glyphs in color (has an `sbix`, `CBDT` or `COLR` table), as emoji
    /// fonts do.
    pub color: bool,
    /// Where the outlines of the face's glyphs come from.
    pub outlines: Outlines,
    /// Which fonts fallback goes to for the characters the face lacks.
    cascade: Cascade,
    /// The weight that fallback beyond the families of the text matches, where the system gives
    /// the face another weight than its own (Hiragino's W3 is regular to Core Text).
    cascade_weight: Option<f32>,
    /// The normal track of the face's `trak` table, which shaping applies where the face has a
    /// `STAT` table too: sizes in points and the tracking there in font units, by size.
    trak: Vec<(f32, f32)>,
    pub data: Arc<Vec<u8>>,
    /// Index of the face in a font collection.
    pub index: u32,
    pub metrics: FaceMetrics,
    font: Font,
    plan: ShapePlan,
    /// Which characters from U+0000 to U+00FF the face has, looked up once: most text checks
    /// coverage there.
    latin: [u64; 4],
    /// The boxes [`Face::glyph_bounds`] found, by glyph and the bits of the size the outline is
    /// drawn at (with `size_adjust`, which an `@font-face` rule sets after the face is read):
    /// drawing an outline to measure it is slow, and text repeats its glyphs.
    bounds: Mutex<GlyphBounds>,
    /// The extents [`Face::intercept`] found without variations, by glyph and the bits of the
    /// size and of the band: underlined text crosses the same glyphs at the same heights again
    /// and again.
    intercepts: Mutex<Intercepts>,
    /// The ink boxes of glyphs as Chrome on Linux measures them for math (see
    /// [`Face::glyph_ink_box`]).
    ink_boxes: Mutex<math::InkBoxes>,
    /// The heights that `FreeType`'s auto-hinter fits to the pixel grid where Chrome on Linux
    /// measures the x-height and the cap height from glyphs (see [`Face::x_height_at`]).
    grid_fit: GridFit,
}

/// The tops of a TrueType face's "x" and "H" (font units) where Chrome on Linux takes its x-height
/// and cap height from them, because the face's OS/2 table gives none (Skia's
/// `getCBoxForLetter`), and the overshoot of its round small letters, by which the auto-hinter
/// scales the face vertically.
#[derive(Clone, Copy, Debug, Default)]
struct GridFit {
    x: Option<i64>,
    cap: Option<i64>,
    overshoot: Option<i64>,
}

/// The boxes of a face's glyphs, by glyph and the bits of a size (see [`Face::glyph_bounds`]).
type GlyphBounds = FxHashMap<(u32, u32), Option<[f32; 4]>>;

/// The extents of a face's glyphs in bands, by glyph and the bits of a size and a band (see
/// [`Face::intercept`]).
type Intercepts = FxHashMap<(u32, u32, u32, u32), Option<(f32, f32)>>;

/// How many extents a face keeps at most; then it starts over.
const MAX_INTERCEPTS: usize = 1 << 16;

impl std::fmt::Debug for Face {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Face")
            .field("family", &self.family)
            .field("weight", &self.weight)
            .field("style", &self.style)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Face {
    /// The glyph for a character, if the face has one. A face without the hyphen (U+2010) or
    /// the non-breaking hyphen (U+2011) gives its hyphen-minus for them, as Core Text
    /// synthesizes them and Chrome on macOS takes them from it (see [`synthesized_hyphen`]).
    /// Line and paragraph separators are drawn as spaces (see [`drawn_as`]).
    pub fn glyph(&self, character: char) -> Option<u32> {
        let charmap = self.font.charmap();
        charmap
            .map_unicode(drawn_as(character))
            .or_else(|| {
                synthesized_hyphen(character)
                    .then(|| charmap.map_unicode('-'))
                    .flatten()
            })
            .map(harfrust::GlyphId::to_u32)
            .filter(|glyph| *glyph < self.font.num_glyphs())
    }

    /// Whether the face has a glyph for a character. A space separator it lacks (an em, thin or
    /// figure space …) it draws with its space, as wide as the separator should be, where it has
    /// one: shaping falls back so (`HarfBuzz`'s space fallback), and so Chrome does not look for
    /// another font for it.
    pub fn has_glyph(&self, character: char) -> bool {
        self.has_own_glyph(character) || has_space_fallback(character) && self.has_own_glyph(' ')
    }

    fn has_own_glyph(&self, character: char) -> bool {
        let code = character as u32;
        if code < 256 {
            self.latin[(code / 64) as usize] & (1 << (code % 64)) != 0
        } else {
            self.covers(character) && self.glyph(character).is_some()
        }
    }

    /// Whether the face draws a grapheme cluster (a base character and the combining marks
    /// after it) without missing glyphs, as shaping draws it: a mark the face lacks may be
    /// composed with the character before it into one the face has (`s` and a combining acute
    /// into `ś`), as `HarfBuzz` composes them. Variation selectors and other characters that
    /// shaping hides (see [`is_ignorable`]) need no glyph.
    pub fn has_cluster(&self, cluster: &str) -> bool {
        let mut characters = cluster
            .chars()
            .filter(|character| !is_variation_selector(*character) && !is_ignorable(*character));
        let Some(mut current) = characters.next() else {
            return true;
        };
        // `current` is the character drawn last, which the next mark may compose with.
        for mark in characters {
            let composed = harfrust::unicode::compose(u32::from(current), u32::from(mark))
                .and_then(char::from_u32)
                .filter(|composed| self.has_glyph(*composed));
            match composed {
                Some(composed) => current = composed,
                None if self.has_glyph(mark) => {
                    if !self.has_glyph(current) {
                        return false;
                    }
                    current = mark;
                }
                None => return false,
            }
        }
        self.has_glyph(current)
    }

    /// Whether the face substitutes glyphs for a feature (has it in its GSUB table), such as
    /// `smcp` for small capitals.
    pub fn substitutes(&self, feature: [u8; 4]) -> bool {
        self.substitutions.binary_search(&feature).is_ok()
    }

    /// Whether the face positions glyphs for a feature (has it in its GPOS table), such as
    /// `halt` for alternate half widths.
    pub fn positions(&self, feature: [u8; 4]) -> bool {
        self.positionings.binary_search(&feature).is_ok()
    }

    /// Whether the face is used for a character: whether its `@font-face` rule's
    /// `unicode-range` includes it (a face without one is used for all characters).
    pub fn covers(&self, character: char) -> bool {
        let code = character as u32;
        self.unicode_range.is_empty()
            || self
                .unicode_range
                .iter()
                .any(|(start, end)| (*start..=*end).contains(&code))
    }

    /// How much wider than shaped each grapheme of the face is at `size` pixels, in pixels: the
    /// face's `trak` table (Apple's tracking by size, which SF and PingFang have) at the size,
    /// less the tracking that shaping applied, which is that at 12 points. Chrome on macOS
    /// tracks by the size in CSS pixels. 0 for faces without tracking.
    pub fn tracking(&self, size: f32) -> f32 {
        if self.trak.is_empty() {
            return 0.0;
        }
        let at = |ptem: f32| {
            let index = self.trak.partition_point(|(entry, _)| *entry < ptem);
            // Below the first size its value, above the last the last one's, between two sizes
            // in proportion.
            match (
                index.checked_sub(1).and_then(|i| self.trak.get(i)),
                self.trak.get(index),
            ) {
                (Some((s0, v0)), Some((s1, v1))) if s1 > s0 => v0 + (ptem - s0) / (s1 - s0) * (v1 - v0),
                (_, Some((_, value))) | (Some((_, value)), None) => *value,
                (None, None) => 0.0,
            }
        };
        (at(size) - at(12.0)) * self.scale(size)
    }

    /// Whether the face, added to the installed fonts, stands in for `other` of the same
    /// family: it has the same style and overlaps it in weight and width.
    fn replaces(&self, other: &Face) -> bool {
        let overlap = |(low, high): (f32, f32), (other_low, other_high): (f32, f32)| {
            low <= other_high && other_low <= high
        };
        self.style == other.style
            && overlap(self.weight_range, other.weight_range)
            && overlap(self.stretch, other.stretch)
    }

    /// Whether text of `weight` in the face is made bold, as Chrome decides it: the text asks
    /// for 600 or more, the face covers no such weight, the font itself is not bold, and it is
    /// not a variable face of `font-weight: auto` with heavier instances on its `wght` axis.
    pub fn synthetic_bold(&self, weight: f32) -> bool {
        weight >= 600.0
            && self.weight_range.1 < 600.0
            && !self.own_bold
            && !(self.weight_auto && self.axis(*b"wght").is_some_and(|axis| axis.max > 400.0))
    }

    /// Whether text of `style` in the face is slanted, as Chrome decides it: the style slants
    /// 14 degrees or more (`italic`, `oblique`), the face covers no such slant, the font itself
    /// is not italic, an `italic` face has no `ital` axis to set, and it is not a face of
    /// `font-style: auto` whose `slnt` axis leans right.
    pub fn synthetic_italic(&self, style: FontStyle) -> bool {
        style.slope() >= ITALIC_SLOPE
            && self.slope.1 < ITALIC_SLOPE
            && !self.own_italic
            && !(style == FontStyle::Italic
                && self
                    .axis(*b"ital")
                    .is_some_and(|axis| axis.min <= 1.0 && axis.max >= 1.0))
            && !(self.slope_auto && self.axis(*b"slnt").is_some_and(|axis| axis.min < 0.0))
    }

    /// The variation axis of a tag, if the face has one.
    pub fn axis(&self, tag: [u8; 4]) -> Option<&Axis> {
        self.axes.iter().find(|axis| axis.tag == tag)
    }

    /// Scale factor from font units to pixels at a font size of `size` pixels: its em is
    /// `size` pixels, times the face's `size-adjust`.
    pub fn scale(&self, size: f32) -> f32 {
        size * self.size_adjust / self.metrics.units_per_em
    }

    /// The advance of a shaped glyph along a horizontal line at a font size of `size` pixels,
    /// as Chrome on Linux has it: the glyph's own advance as `FreeType` scales it (its linear
    /// advance in 1/65536 pixel) at the size Skia sets, cut to 1/64 pixel (`SkScalarToFDot6`),
    /// which Skia hands to the shaper (`SkiaScalarToHarfBuzzPosition`), plus what positioning
    /// (kerning) adds, which the shaper scales to 1/65536 pixel at the size cut to that
    /// precision (`em_mult`). At 11pt (14.67px, set as 14.66px) a line is 0.07 % narrower than
    /// the font's advances times the size, which decides whether a word still fits.
    ///
    /// A glyph the shaper sets without advance (a default ignorable character such as a
    /// direction mark, or a mark of zero width) has none: the shaper sets it to zero, whatever
    /// the advance Skia gave.
    pub fn layout_advance(&self, glyph: &ShapedGlyph, size: f32) -> f32 {
        if glyph.x_advance == 0.0 {
            return 0.0;
        }
        let size = size * self.size_adjust;
        #[allow(clippy::cast_possible_truncation, reason = "font units of an em")]
        let units_per_em = (self.metrics.units_per_em as i128).max(1);
        // `FreeType`'s rounding fixed-point arithmetic (`FT_MulDiv`, `FT_DivFix`), in integers
        // wide enough for any font size: at `font-size: 1e30px` the products are past 64 bits.
        let mul_div = |a: i128, b: i128, c: i128| {
            let product = a * b;
            let half = c / 2;
            if product < 0 {
                -((-product + half) / c)
            } else {
                (product + half) / c
            }
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the size in 26.6 fixed point, glyph advances in font units"
        )]
        let own = {
            let size = (f64::from(size) * 64.0) as i64;
            let x_scale = mul_div(i128::from(size), 0x10000, units_per_em);
            mul_div(f64::from(glyph.own_advance).round() as i128, x_scale, 64)
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "HarfBuzz positions, 16.16 fixed point"
        )]
        let positioned = {
            let scale = (f64::from(size) * 65536.0).trunc() as i64;
            let multiplier = (i128::from(scale) << 16) / units_per_em;
            let delta = f64::from(glyph.x_advance - glyph.own_advance).round() as i128;
            (delta * multiplier + 32768) >> 16
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "a glyph's advance in pixels"
        )]
        let advance = ((own + positioned) as f64 / 65536.0) as f32;
        advance
    }

    /// The ascent and descent of the face at `size` pixels, rounded to whole pixels as browsers
    /// lay out lines, with the ascent made larger by [`Face::ascent_adjustment`].
    pub fn rounded_ascent_descent(&self, size: f32) -> (f32, f32) {
        let scale = self.scale(size);
        let ascent = (self.metrics.ascender * scale).round();
        let descent = (self.metrics.descender * scale).round();
        (ascent + self.ascent_adjustment(ascent, descent), descent)
    }

    /// The x-height of the face at a font size of `size` pixels, in pixels, as Chrome on Linux
    /// takes it for `ex`, `vertical-align: middle` and `font-size-adjust`: the OS/2 table's, or
    /// for a face without one the top of its "x", which in a TrueType face is where `FreeType` puts
    /// it (see [`grid_fitted`]): 8 pixels at 14 pixels for `DejaVu` Sans Mono, whose "x" is 7.66
    /// pixels high.
    pub fn x_height_at(&self, size: f32) -> f32 {
        self.fitted(self.grid_fit.x, size)
            .unwrap_or_else(|| self.metrics.x_height * self.scale(size))
    }

    /// The cap height of the face at `size` pixels, as Chrome on Linux takes it for `cap`: the
    /// OS/2 table's, or the top of the "H" as for [`Face::x_height_at`].
    pub fn cap_height_at(&self, size: f32) -> f32 {
        self.fitted(self.grid_fit.cap, size)
            .unwrap_or_else(|| self.metrics.cap_height * self.scale(size))
    }

    fn fitted(&self, top: Option<i64>, size: f32) -> Option<f32> {
        let pixels = size * self.size_adjust;
        if !(pixels > 0.0 && pixels.is_finite()) {
            return None;
        }
        Some(grid_fitted(
            top?,
            self.grid_fit.overshoot,
            pixels,
            self.metrics.units_per_em as i64,
        ))
    }

    /// The highest point of the outline of `glyph` (curves' control points included), in font
    /// units: what `FreeType` takes as a glyph's top.
    fn point_top(&self, glyph: u32) -> Option<f32> {
        /// Keeps the highest point.
        struct Top(Option<f32>);
        impl Top {
            fn add(&mut self, y: f32) {
                self.0 = Some(self.0.map_or(y, |top| top.max(y)));
            }
        }
        impl skrifa::outline::OutlinePen for Top {
            fn move_to(&mut self, _: f32, y: f32) {
                self.add(y);
            }
            fn line_to(&mut self, _: f32, y: f32) {
                self.add(y);
            }
            fn quad_to(&mut self, _: f32, cy: f32, _: f32, y: f32) {
                self.add(cy);
                self.add(y);
            }
            fn curve_to(&mut self, _: f32, cy0: f32, _: f32, cy1: f32, _: f32, y: f32) {
                self.add(cy0);
                self.add(cy1);
                self.add(y);
            }
            fn close(&mut self) {}
        }
        let mut top = Top(None);
        let units = self.metrics.units_per_em / self.size_adjust;
        self.draw_glyph(glyph, units, &[], &mut top)
            .then_some(top.0)
            .flatten()
    }

    /// What Chrome adds to the rounded `ascent` of the face, with the rounded `descent`: 15 % of
    /// their sum, rounded, for the installed Times, Helvetica and Courier of macOS, so that they
    /// take the room of their Windows counterparts (Blink's `AscentDescentWithHacks`, after
    /// Safari); nothing for other faces. Fagus does it on every system, so that a document
    /// lays out the same with the same fonts.
    pub fn ascent_adjustment(&self, ascent: f32, descent: f32) -> f32 {
        if self.document {
            0.0
        } else {
            ascent_adjustment(&self.family, ascent, descent)
        }
    }

    /// The box around the outline of `glyph` at a font size of `size` pixels (see
    /// [`Face::draw_glyph`]), from the glyph's origin with y down: left, top, right and bottom.
    /// `None` for a glyph without outline.
    pub fn glyph_bounds(&self, glyph: u32, size: f32) -> Option<[f32; 4]> {
        let key = (glyph, (size * self.size_adjust).to_bits());
        if let Some(bounds) = self.bounds.lock().ok().and_then(|cache| cache.get(&key).copied()) {
            return bounds;
        }
        let bounds = self.measure_glyph(glyph, size);
        if let Ok(mut cache) = self.bounds.lock() {
            cache.insert(key, bounds);
        }
        bounds
    }

    /// The box around the outline of `glyph` at `size` pixels, measured from the outline (see
    /// [`Face::glyph_bounds`]).
    fn measure_glyph(&self, glyph: u32, size: f32) -> Option<[f32; 4]> {
        use skrifa::outline::OutlinePen;

        /// Keeps the box around the points of an outline, curves followed in steps.
        struct Bounds {
            last: (f32, f32),
            bounds: Option<[f32; 4]>,
        }
        impl Bounds {
            fn add(&mut self, (x, y): (f32, f32)) {
                self.last = (x, y);
                let [left, top, right, bottom] = self.bounds.unwrap_or([x, y, x, y]);
                self.bounds = Some([left.min(x), top.min(y), right.max(x), bottom.max(y)]);
            }
            fn curve(&mut self, points: &[(f32, f32)]) {
                const STEPS: u16 = 16;
                let start = self.last;
                for step in 1..=STEPS {
                    let t = f32::from(step) / f32::from(STEPS);
                    let mut level: smallvec::SmallVec<[(f32, f32); 4]> =
                        std::iter::once(start).chain(points.iter().copied()).collect();
                    while level.len() > 1 {
                        for index in 0..level.len() - 1 {
                            let (a, b) = (level[index], level[index + 1]);
                            level[index] = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                        }
                        level.pop();
                    }
                    self.add(level[0]);
                }
            }
        }
        impl OutlinePen for Bounds {
            fn move_to(&mut self, x: f32, y: f32) {
                self.add((x, -y));
            }
            fn line_to(&mut self, x: f32, y: f32) {
                self.add((x, -y));
            }
            fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
                self.curve(&[(cx, -cy), (x, -y)]);
            }
            fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
                self.curve(&[(cx0, -cy0), (cx1, -cy1), (x, -y)]);
            }
            fn close(&mut self) {}
        }

        let mut bounds = Bounds {
            last: (0.0, 0.0),
            bounds: None,
        };
        if !self.draw_glyph(glyph, size, &[], &mut bounds) {
            return None;
        }
        bounds.bounds
    }

    /// Where the outline of `glyph` at `size` pixels (at `variations`) lies between two heights
    /// `band` (pixels below the baseline, top first): the leftmost and rightmost of it there,
    /// from the glyph's origin, as Skia's text intercepts find them. `None` where the outline
    /// does not reach into the band.
    pub fn intercept(
        &self,
        glyph: u32,
        size: f32,
        variations: &Variations,
        band: (f32, f32),
    ) -> Option<(f32, f32)> {
        if !variations.is_empty() {
            return self.measure_intercept(glyph, size, variations, band);
        }
        let key = (
            glyph,
            (size * self.size_adjust).to_bits(),
            band.0.to_bits(),
            band.1.to_bits(),
        );
        if let Some(extent) = self
            .intercepts
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).copied())
        {
            return extent;
        }
        let extent = self.measure_intercept(glyph, size, variations, band);
        if let Ok(mut cache) = self.intercepts.lock() {
            if cache.len() >= MAX_INTERCEPTS {
                cache.clear();
            }
            cache.insert(key, extent);
        }
        extent
    }

    /// The extent of `glyph` in `band`, found from its outline (see [`Face::intercept`]).
    fn measure_intercept(
        &self,
        glyph: u32,
        size: f32,
        variations: &Variations,
        band: (f32, f32),
    ) -> Option<(f32, f32)> {
        use skrifa::outline::OutlinePen;

        /// Keeps the horizontal extent of the parts of an outline between two heights, with
        /// curves cut into lines.
        struct Extent {
            band: (f32, f32),
            last: (f32, f32),
            start: (f32, f32),
            range: Option<(f32, f32)>,
        }
        impl Extent {
            fn add(&mut self, x: f32) {
                self.range = Some(
                    self.range
                        .map_or((x, x), |(left, right)| (left.min(x), right.max(x))),
                );
            }
            /// A line from the last point, in pixels with y down.
            fn line(&mut self, to: (f32, f32)) {
                let (from, (top, bottom)) = (self.last, self.band);
                self.last = to;
                let (low, high) = (from.1.min(to.1), from.1.max(to.1));
                if high < top || low > bottom {
                    return;
                }
                for point in [from, to] {
                    if (top..=bottom).contains(&point.1) {
                        self.add(point.0);
                    }
                }
                // A level line meets the band's edges in its end points, if at all.
                if high > low {
                    for y in [top, bottom] {
                        if (low..=high).contains(&y) {
                            self.add(from.0 + (to.0 - from.0) * (y - from.1) / (to.1 - from.1));
                        }
                    }
                }
            }
            fn curve(&mut self, points: &[(f32, f32)]) {
                const STEPS: u16 = 16;
                let start = self.last;
                for step in 1..=STEPS {
                    let t = f32::from(step) / f32::from(STEPS);
                    // De Casteljau over the start and the control points.
                    let mut level: smallvec::SmallVec<[(f32, f32); 4]> =
                        std::iter::once(start).chain(points.iter().copied()).collect();
                    while level.len() > 1 {
                        for index in 0..level.len() - 1 {
                            let (a, b) = (level[index], level[index + 1]);
                            level[index] = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                        }
                        level.pop();
                    }
                    self.line(level[0]);
                }
            }
        }
        impl OutlinePen for Extent {
            fn move_to(&mut self, x: f32, y: f32) {
                self.last = (x, -y);
                self.start = self.last;
            }
            fn line_to(&mut self, x: f32, y: f32) {
                self.line((x, -y));
            }
            fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
                self.curve(&[(cx, -cy), (x, -y)]);
            }
            fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
                self.curve(&[(cx0, -cy0), (cx1, -cy1), (x, -y)]);
            }
            fn close(&mut self) {
                self.line(self.start);
            }
        }

        let mut extent = Extent {
            band,
            last: (0.0, 0.0),
            start: (0.0, 0.0),
            range: None,
        };
        if !self.draw_glyph(glyph, size, variations, &mut extent) {
            return None;
        }
        extent.range
    }

    /// Draws the outline of `glyph` at a font size of `size` pixels (an em of `size` times the
    /// face's `size-adjust`) and at `variations` into `pen`, in pixels from the glyph's origin
    /// with y up: from the face's `glyf`, `CFF` or `CFF2` table, or from its `hvgl` table. False
    /// for a glyph without outline.
    pub fn draw_glyph(
        &self,
        glyph: u32,
        size: f32,
        variations: &Variations,
        pen: &mut impl skrifa::outline::OutlinePen,
    ) -> bool {
        use skrifa::MetadataProvider;
        use skrifa::outline::DrawSettings;

        let Ok(font) = skrifa::FontRef::from_index(&self.data, self.index) else {
            return false;
        };
        let location = font
            .axes()
            .location(variations.iter().map(|(tag, value)| (Tag::new(tag), *value)));
        match self.outlines {
            Outlines::Hvgl => {
                let Some(table) = font.table_data(Tag::new(b"hvgl")) else {
                    return false;
                };
                let Some(table) = hvgl::Hvgl::new(table.as_bytes()) else {
                    return false;
                };
                let coordinates: Vec<f64> = location
                    .coords()
                    .iter()
                    .map(|coordinate| f64::from(coordinate.to_f32()))
                    .collect();
                table.draw(glyph, &coordinates, f64::from(self.scale(size)), pen)
            }
            _ => font
                .outline_glyphs()
                .get(skrifa::GlyphId::new(glyph))
                .is_some_and(|outline| {
                    outline
                        .draw(
                            DrawSettings::unhinted(
                                skrifa::instance::Size::new(size * self.size_adjust),
                                &location,
                            ),
                            pen,
                        )
                        .is_ok()
                }),
        }
    }
}

/// A shaped glyph, positioned in font units: its advance along the line and its offsets along
/// and across it (in vertical lines as [`FontDb::shape_vertical`] says).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    pub id: u32,
    /// Byte offset of the cluster in the shaped text.
    pub cluster: u32,
    pub x_advance: f32,
    /// The glyph's own advance along a horizontal line (its `hmtx` advance, varied), before
    /// shaping positioned it (kerning): `x_advance` less the adjustments of positioning.
    pub own_advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
    /// Whether the text may be broken before this glyph without reshaping.
    pub safe_to_break: bool,
}

/// The result of shaping a run of text with one face.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedText {
    pub glyphs: Vec<ShapedGlyph>,
    /// Sum of all advances, in font units.
    pub advance: f32,
}

/// The faces that characters of a run of text fell back to, in the order they were found, for
/// [`FontDb::fallback_in_run`]; it starts over where the script of the text changes.
#[derive(Clone, Debug, Default)]
pub struct FallbackRun {
    script: Option<harfrust::Script>,
    faces: smallvec::SmallVec<[FaceId; 4]>,
    /// Whether the last character noted that has a script of its own is of a cursive one.
    cursive: Option<bool>,
}

impl FallbackRun {
    /// Notes a character of the run, in logical order, for [`Self::cursive`].
    pub fn note(&mut self, character: char) {
        if let Some(cursive) = cursive_script(character) {
            self.cursive = Some(cursive);
        }
    }

    /// Starts the run before `text`, whose characters will be noted: until one with a script
    /// of its own is, those without one take the script of the first that has one, as Blink
    /// segments text by script.
    pub fn start(&mut self, text: &str) {
        if self.cursive.is_none() {
            self.cursive = text.chars().find_map(cursive_script);
        }
    }

    /// Starts the run after text whose last character with a script of its own is of a cursive
    /// script or not (`before`), or before text whose first one is, where nothing comes before:
    /// the text of the paragraph around `text`, which Blink segments by script as a whole.
    pub fn start_in_context(&mut self, text: &str, context: Option<bool>) {
        if self.cursive.is_none() {
            self.cursive = context;
        }
        self.start(text);
    }

    /// Whether the last character noted that has a script of its own is of a cursive script
    /// (see [`cursive_script`]); `None` before any.
    pub fn cursive(&self) -> Option<bool> {
        self.cursive
    }

    /// Adds a face that a character of the run took otherwise, as an emoji shown as text does
    /// (see [`FontDb::fallback_sequence_in`]).
    pub fn add(&mut self, face: FaceId) {
        if !self.faces.contains(&face) {
            self.faces.push(face);
        }
    }
}

/// Default font families for the generic family names of CSS.
#[derive(Clone, Debug, Default)]
pub struct GenericFamilies {
    pub sans_serif: Option<String>,
    pub serif: Option<String>,
    pub monospace: Option<String>,
}

/// The direction in which text is shaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    LeftToRight,
    RightToLeft,
    /// Top to bottom, for a vertical line (see [`FontDb::shape_vertical`]).
    TopToBottom,
}

/// OpenType features to shape text with (`font-feature-settings`, `font-variant-numeric`):
/// tags and values.
pub type Features = [([u8; 4], u32)];

/// A face (by its unique key) with features and the coordinates of its instance (as bits), for
/// text in a direction, a script and a language.
type PlanKey = (
    u64,
    Box<Features>,
    CoordinateBits,
    Direction,
    harfrust::Script,
    Option<Box<str>>,
);

/// The coordinates of an instance with their values as bits, for keys.
type CoordinateBits = Box<[([u8; 4], u32)]>;

fn coordinate_bits(variations: &Variations) -> CoordinateBits {
    variations
        .iter()
        .map(|(tag, value)| (*tag, value.to_bits()))
        .collect()
}

/// The fonts available to an engine.
#[derive(Clone, Default)]
pub struct FontDb {
    faces: Vec<Arc<Face>>,
    generic: GenericFamilies,
    shape_cache: Arc<Mutex<ShapeCache>>,
    /// Shaping plans for faces with features other than the default ones, or at coordinates.
    plans: Arc<Mutex<FxHashMap<PlanKey, Arc<ShapePlan>>>>,
    instances: Arc<Mutex<Instances>>,
    /// The fonts installed on the system, where they were added.
    system: Option<Arc<SystemFonts>>,
    queries: QueryCache,
    /// For each face, whether a later `@font-face` rule with the same family and descriptors
    /// limits itself to a `unicode-range`: such a rule comes first for its characters, so a
    /// face's own glyph does not settle which face a character takes.
    overlaid: Vec<bool>,
    /// The language that text without one is shaped in ([`FontDb::set_default_language`]).
    default_language: Option<Arc<str>>,
    /// For each face, whether it is a face of a document that is not loaded (see
    /// [`FontDb::with_unloaded`]).
    unloaded: Vec<bool>,
    /// The document's `@font-palette-values` rules, in cascade order.
    palette_values: Arc<Vec<PaletteValues>>,
}

/// Instances of variable faces (by their key) at coordinates.
type Instances = FxHashMap<(u64, CoordinateBits), Arc<Font>>;

/// The faces that queries found, by a hash of the families, the weight, the style and the
/// width, and those that fallbacks found for characters. It holds for one set of faces: a copy of
/// the database starts empty, and adding faces empties it.
#[derive(Default)]
struct QueryCache {
    faces: Mutex<FxHashMap<QueryKey, Option<FaceId>>>,
    /// The face of each fallback, with whether a family of the text or the standard font gave
    /// it (see [`FontDb::fallback_in_run`]).
    fallbacks: Mutex<FxHashMap<(QueryKey, char), Fallback>>,
    sequences: Mutex<FxHashMap<SequenceKey, Option<FaceId>>>,
    /// The colors of the palettes of color faces (see [`FontDb::palette`]), by face and a hash
    /// of the palette.
    palettes: Mutex<PaletteCache>,
}

/// The colors of palettes by face and a hash of the palette (see [`FontDb::palette`]).
type PaletteCache = FxHashMap<(FaceId, u64), Vec<(FontPalette, Option<Arc<[PaletteColor]>>)>>;

/// The most palettes of color faces other than their own that a document has: each is a copy
/// of the face in the PDF, and a palette may have 65535 colors.
const MAX_PALETTES: usize = 256;

/// The face of a fallback, with whether a family of the text or the standard font gave it.
type Fallback = (Option<FaceId>, bool);

/// A hash of the families and the language, then the weight, the style and the width (numbers as
/// bits).
type QueryKey = (u64, u32, u32, u32);

/// A query (see [`QueryKey`]) for an emoji sequence.
type SequenceKey = (QueryKey, Box<str>);

impl Clone for QueryCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// Shaped text per face (by its unique key) and string, in two generations: when the newer one
/// is full, it becomes the older one and the oldest is dropped. An engine renders document after
/// document, so the cache has to stay bounded; text that is still in use moves back to the newer
/// generation and stays.
#[derive(Default)]
struct ShapeCache {
    newer: FxHashMap<ShapeKey, Arc<ShapedText>>,
    older: FxHashMap<ShapeKey, Arc<ShapedText>>,
    /// Text shaped as part of a longer text, with the text before and after it that shaping
    /// looks at (see [`FontDb::shape_in_context`]): only text of scripts that join, kept apart
    /// so that the keys of other text stay as small. Dropped whole when full.
    joined: FxHashMap<(ShapeKey, Box<str>, Box<str>), Arc<ShapedText>>,
}

/// Text shaped with a face (by its unique key), features and coordinates (as bits), in a
/// direction and a language.
type ShapeKey = (
    u64,
    Box<str>,
    Box<Features>,
    CoordinateBits,
    Direction,
    Option<Box<str>>,
);

impl ShapeCache {
    /// Entries per generation.
    const GENERATION: usize = 1 << 15;

    fn get(&mut self, key: &ShapeKey) -> Option<Arc<ShapedText>> {
        if let Some(shaped) = self.newer.get(key) {
            return Some(Arc::clone(shaped));
        }
        let (key, shaped) = self.older.remove_entry(key)?;
        self.insert(key, Arc::clone(&shaped));
        Some(shaped)
    }

    fn insert(&mut self, key: ShapeKey, shaped: Arc<ShapedText>) {
        if self.newer.len() >= Self::GENERATION {
            self.older = std::mem::take(&mut self.newer);
        }
        self.newer.insert(key, shaped);
    }

    /// Keeps text shaped with the text around it.
    fn insert_joined(&mut self, key: (ShapeKey, Box<str>, Box<str>), shaped: Arc<ShapedText>) {
        if self.joined.len() >= Self::GENERATION {
            self.joined.clear();
        }
        self.joined.insert(key, shaped);
    }
}

impl std::fmt::Debug for FontDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontDb")
            .field("faces", &self.faces)
            .finish_non_exhaustive()
    }
}

/// An error while loading fonts.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FontError {
    #[error("the data is not a supported font")]
    InvalidFont,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Where the words of `text` may be hyphenated, as byte offsets, for the language of the BCP 47
/// tag `lang` (`de`, `de-DE`, `en-GB` …), as Chrome hyphenates them on Linux: see
/// [`hyphenation_points_limited`] with the default limits.
pub fn hyphenation_points(text: &str, lang: &str) -> Vec<usize> {
    hyphenation_points_limited(text, lang, [0; 3])
}

/// The largest font that is unpacked from WOFF or WOFF2.
const MAX_UNPACKED: u32 = 64 << 20;

/// The OpenType data of a font: WOFF and WOFF2 fonts are unpacked, others stay as they are. The
/// size the font declares is checked first, so that a small file cannot unpack to gigabytes; for
/// WOFF, whose tables are unpacked to the sizes they declare each, those too.
pub fn unpack(data: Vec<u8>) -> Result<Vec<u8>, FontError> {
    let woff1 = data.starts_with(b"wOFF");
    if !woff1 && !data.starts_with(b"wOF2") {
        return Ok(data);
    }
    let declared = data
        .get(16..20)
        .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or(FontError::InvalidFont)?;
    if declared > MAX_UNPACKED
        || woff1 && woff1_tables(&data).is_none_or(|tables| tables > u64::from(MAX_UNPACKED))
    {
        return Err(FontError::InvalidFont);
    }
    let unpacked = if woff1 {
        wuff::decompress_woff1(&data)
    } else {
        wuff::decompress_woff2(&data)
    };
    unpacked.map_err(|_| FontError::InvalidFont)
}

/// The sum of the sizes that the tables of a WOFF font declare unpacked (`origLength` in its
/// table directory); `None` if the directory is cut short.
fn woff1_tables(data: &[u8]) -> Option<u64> {
    let count = u16::from_be_bytes(data.get(12..14)?.try_into().ok()?);
    (0..usize::from(count))
        .map(|table| {
            // Each entry: tag, offset, compLength, origLength, origChecksum (four bytes each).
            let at = 44 + table * 20 + 12;
            Some(u64::from(u32::from_be_bytes(
                data.get(at..at + 4)?.try_into().ok()?,
            )))
        })
        .sum()
}

impl FontDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds all faces of a font file (a single font or a collection, also as WOFF or WOFF2).
    /// Returns the new faces.
    pub fn add_font_data(&mut self, data: Vec<u8>) -> Result<Vec<FaceId>, FontError> {
        let data = Arc::new(unpack(data)?);
        let mut added = Vec::new();
        for index in 0.. {
            let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(&data) as _;
            let Some(font) = Font::new(blob, index) else { break };
            if face_sound(&data, index) {
                added.push(self.push(Arc::new(make_face(font, Arc::clone(&data), index))));
            }
        }
        if added.is_empty() {
            Err(FontError::InvalidFont)
        } else {
            Ok(added)
        }
    }

    /// Adds a font file.
    pub fn add_font_file(&mut self, path: &Path) -> Result<Vec<FaceId>, FontError> {
        self.add_font_data(std::fs::read(path)?)
    }

    /// Adds all font files (`.ttf`, `.otf`, `.ttc`, `.otc`, `.woff`, `.woff2`) in a directory and
    /// its subdirectories. Returns the number of faces added. Directories and files that symbolic
    /// links lead to more than once are read once: a link to a parent directory would otherwise
    /// load its fonts again at every level until the system refuses to follow more links.
    pub fn add_font_dir(&mut self, directory: &Path) -> Result<usize, FontError> {
        self.add_font_dir_once(directory, &mut FxHashSet::default())
    }

    /// Adds the fonts of a directory and its subdirectories, but not those of the directories and
    /// files in `seen` (by their canonical paths), which it adds to.
    fn add_font_dir_once(
        &mut self,
        directory: &Path,
        seen: &mut FxHashSet<PathBuf>,
    ) -> Result<usize, FontError> {
        if !seen.insert(std::fs::canonicalize(directory)?) {
            return Ok(0);
        }
        let mut count = 0;
        let mut entries: Vec<_> = std::fs::read_dir(directory)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                count += self.add_font_dir_once(&path, seen)?;
            } else if std::fs::canonicalize(&path).is_ok_and(|real| !seen.insert(real)) {
                // The same file again, through a link.
            } else if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2"
                    )
                })
            {
                count += self.add_font_file(&path).map_or(0, |faces| faces.len());
            }
        }
        Ok(count)
    }

    /// Adds the fonts built into Fagus: Inter in regular and bold. They stand in where no other
    /// face fits: for text in families and generic families without faces of their own, and for
    /// characters the other faces lack. Faces of the family Inter that are added hide them.
    pub fn add_default_fonts(&mut self) {
        for data in DEFAULT_FONTS {
            let data = Arc::new(data.to_vec());
            let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(&data) as _;
            let Some(font) = Font::new(blob, 0) else {
                continue;
            };
            let mut face = make_face(font, data, 0);
            face.builtin = true;
            self.push(Arc::new(face));
        }
    }

    /// Adds the fonts installed on the system (on macOS those in `/System/Library/Fonts`,
    /// `/Library/Fonts` and `~/Library/Fonts`; on Linux and FreeBSD those fontconfig lists; on
    /// Windows those in `%WINDIR%\Fonts` and the user's), which then serve as in Chrome: for
    /// families that documents name, for the generic families (Times for `serif`, Helvetica for
    /// `sans-serif`, Menlo for `monospace` … and, by the language of the text, Hiragino, PingFang
    /// or Apple SD Gothic Neo) and for fallback. Fonts added otherwise and those of documents come first for
    /// their families. The faces are only indexed now and parsed when used. Returns the number
    /// of faces found.
    pub fn add_system_fonts(&mut self) -> usize {
        self.set_system_fonts(SystemFonts::scan())
    }

    fn set_system_fonts(&mut self, fonts: SystemFonts) -> usize {
        let count = fonts.len();
        self.system = Some(Arc::new(fonts));
        self.queries = QueryCache::default();
        count
    }

    /// The directories of the installed fonts that [`Self::add_system_fonts`] found, which are
    /// read when a document uses one of their faces.
    pub fn system_font_directories(&self) -> Vec<PathBuf> {
        self.system
            .as_ref()
            .map(|system| system.directories())
            .unwrap_or_default()
    }

    /// Whether the fonts of the system were added.
    pub fn has_system_fonts(&self) -> bool {
        self.system.is_some()
    }

    /// The families of installed faces that matching or fallback passed over since the last
    /// call because Fagus cannot draw their glyphs (they have no outlines, color glyphs or
    /// images that it reads), so that the next face drew the text instead. Each family is
    /// reported once.
    pub fn take_skipped_faces(&self) -> Vec<String> {
        self.system
            .as_ref()
            .map(|system| system.take_skipped())
            .unwrap_or_default()
    }

    fn push(&mut self, face: Arc<Face>) -> FaceId {
        self.queries = QueryCache::default();
        let id = FaceId(u32::try_from(self.faces.len()).expect("fewer than u32::MAX faces"));
        if face.document && !face.unicode_range.is_empty() {
            for (earlier, overlaid) in self.faces.iter().zip(&mut self.overlaid) {
                *overlaid |= earlier.document
                    && earlier.family.eq_ignore_ascii_case(&face.family)
                    && earlier.weight_range == face.weight_range
                    && earlier.style == face.style
                    && earlier.stretch == face.stretch;
            }
        }
        self.faces.push(face);
        self.overlaid.push(false);
        id
    }

    /// The data of the installed face (added, not built in, or of the system) whose PostScript
    /// name or full name is `name`, in any case, with the face's index in it: what `local()`
    /// in `@font-face` finds, as in Chrome.
    pub fn local(&self, name: &str) -> Option<(Arc<Vec<u8>>, u32)> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        self.faces
            .iter()
            .find(|face| !face.builtin && !face.document && full_names(&face.data, face.index, name))
            .map(|face| (Arc::clone(&face.data), face.index))
            .or_else(|| self.system.as_ref()?.unique(name))
    }

    /// Adds a face that a document defines with `@font-face` (see [`Face::from_rule`]).
    pub fn add_document_face(&mut self, face: Arc<Face>) -> FaceId {
        self.push(face)
    }

    /// How many faces the database holds (installed, built in and of documents; the fonts of
    /// the system aside): the id the next face added gets.
    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// The faces added after the first `count`, in order.
    pub fn faces_after(&self, count: usize) -> &[Arc<Face>] {
        self.faces.get(count..).unwrap_or_default()
    }

    /// The database without the faces of documents (`@font-face` rules): the fonts that an SVG
    /// image sees, which is a document of its own in Chrome.
    pub fn without_document_faces(&self) -> std::borrow::Cow<'_, Self> {
        if !self.faces.iter().any(|face| face.document) {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut installed = Self {
            faces: Vec::new(),
            overlaid: Vec::new(),
            queries: QueryCache::default(),
            ..self.clone()
        };
        for face in self.faces.iter().filter(|face| !face.document) {
            installed.push(Arc::clone(face));
        }
        std::borrow::Cow::Owned(installed)
    }

    /// The database in which the faces of documents that `used` leaves out are not loaded, as
    /// Chrome loads the faces of `@font-face` rules only as the content of a document needs them:
    /// a family whose face for a text is not loaded gives it no face, and the text goes on to
    /// the next family. The faces keep their ids. `None` where all are used.
    pub fn with_unloaded(&self, used: impl Fn(FaceId) -> bool) -> Option<Self> {
        let unloaded: Vec<bool> = self
            .faces
            .iter()
            .enumerate()
            .map(|(index, face)| face.document && !used(FaceId(index as u32)))
            .collect();
        unloaded.contains(&true).then(|| Self {
            unloaded,
            queries: QueryCache::default(),
            ..self.clone()
        })
    }

    /// Whether `id` is a face of a document that is not loaded (see [`Self::with_unloaded`]).
    fn is_unloaded(&self, id: FaceId) -> bool {
        id.0 & SYSTEM_FACE == 0 && self.unloaded.get(id.index()).copied().unwrap_or(false)
    }

    /// Sets the families used for `sans-serif`, `serif` and `monospace`.
    pub fn set_generic_families(&mut self, generic: GenericFamilies) {
        self.generic = generic;
        self.queries = QueryCache::default();
    }

    /// Sets the document's `@font-palette-values` rules, in cascade order.
    pub fn set_palette_values(&mut self, rules: Vec<PaletteValues>) {
        self.palette_values = Arc::new(rules);
        self.queries.palettes = Mutex::default();
    }

    /// The colors that `palette` gives the palette of a color face, by index: `None` where the
    /// face draws with its own first palette (`normal`, a face without palettes, a palette name
    /// without an `@font-palette-values` rule for the face's family), as in Chrome. Each is
    /// resolved once; past [`MAX_PALETTES`] palettes, faces draw with their own.
    pub fn palette(&self, id: FaceId, palette: &FontPalette) -> Option<Arc<[PaletteColor]>> {
        if *palette == FontPalette::Normal || !self.face(id).color {
            return None;
        }
        let key = (id, palette::palette_hash(palette));
        let mut cache = self
            .queries
            .palettes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, colors)) = cache
            .get(&key)
            .and_then(|known| known.iter().find(|(known, _)| known == palette))
        {
            return colors.clone();
        }
        let count: usize = cache.values().map(Vec::len).sum();
        let colors = if count < MAX_PALETTES {
            self.resolve_palette(id, palette)
        } else {
            None
        };
        cache
            .entry(key)
            .or_default()
            .push((palette.clone(), colors.clone()));
        colors
    }

    /// The colors of [`FontDb::palette`], resolved.
    fn resolve_palette(&self, id: FaceId, palette: &FontPalette) -> Option<Arc<[PaletteColor]>> {
        let face = self.face(id);
        let font = skrifa::FontRef::from_index(&face.data, face.index).ok()?;
        let palettes = palette::Palettes::read(&font)?;
        let colors = palettes.resolve(palette, &face.family, &self.palette_values)?;
        (colors != palettes.colors(0)).then(|| colors.into())
    }

    pub fn face(&self, id: FaceId) -> &Face {
        if id.0 & SYSTEM_FACE != 0 {
            return self
                .system
                .as_ref()
                .and_then(|system| system.loaded_face(id.0 & !SYSTEM_FACE))
                .expect("faces of the system are parsed before their ids are given out");
        }
        &self.faces[id.index()]
    }

    /// Whether a later `@font-face` rule with a `unicode-range` overlays the face (see
    /// [`Self::overlaid`]).
    fn is_overlaid(&self, id: FaceId) -> bool {
        self.overlaid.get(id.index()).copied().unwrap_or(false)
    }

    /// Whether a family is installed: added other than built in, or on the system.
    fn has_family(&self, family: &str) -> bool {
        self.faces
            .iter()
            .any(|face| !face.builtin && face.family.eq_ignore_ascii_case(family))
            || self
                .system
                .as_ref()
                .is_some_and(|system| system.has_family(family, true))
    }

    pub fn len(&self) -> usize {
        self.faces.len()
    }

    /// All faces, installed and from the document.
    pub fn faces(&self) -> impl Iterator<Item = &Face> {
        self.faces.iter().map(|face| &**face)
    }

    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// The face that CSS font matching selects for a family list, weight and style, of normal
    /// width. Falls back to the default sans-serif family and then to the first face, so every
    /// query with at least one face returns one.
    pub fn query(&self, families: &[&str], weight: f32, style: FontStyle) -> Option<FaceId> {
        self.query_stretch(families, weight, style, 100.0)
    }

    /// The face that CSS font matching selects for a family list, weight, style and width
    /// (`font-stretch`, in percent of normal); see [`Self::query`].
    pub fn query_stretch(
        &self,
        families: &[&str],
        weight: f32,
        style: FontStyle,
        stretch: f32,
    ) -> Option<FaceId> {
        self.query_in(families, (weight, style, stretch), None)
    }

    /// The face that CSS font matching selects for a family list, weight, style and width in
    /// text of a language (a BCP 47 tag): with the system's fonts, the generic families stand
    /// for the families Chrome takes for the language, and a list without an installed family
    /// gets the language's standard font; see [`Self::query`].
    pub fn query_in(
        &self,
        families: &[&str],
        (weight, style, stretch): (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> Option<FaceId> {
        let key = query_key(families, weight, style, stretch, lang);
        if let Some(found) = self
            .queries
            .faces
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).copied())
        {
            return found;
        }
        let found = self.query_uncached(families, (weight, style, stretch), lang);
        if let Ok(mut cache) = self.queries.faces.lock() {
            cache.insert(key, found);
        }
        found
    }

    /// The first available font of a family list (CSS Fonts 4, §5): as [`Self::query_in`], but
    /// past a family whose faces leave out the space with their `unicode-range`, as Blink skips
    /// it (`FontFallbackList::DeterminePrimarySimpleFontData`). Font-relative lengths refer to
    /// its metrics.
    pub fn first_available(
        &self,
        families: &[&str],
        request: (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> Option<FaceId> {
        let found = self.query_in(families, request, lang)?;
        if self.face(found).covers(' ') {
            return Some(found);
        }
        families
            .iter()
            .filter_map(|family| self.match_listed(family, lang, request, None))
            .find(|face| self.face(*face).covers(' '))
            .or(Some(found))
    }

    fn query_uncached(
        &self,
        families: &[&str],
        (weight, style, stretch): (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> Option<FaceId> {
        for family in families {
            if let Some(face) = self.match_listed(family, lang, (weight, style, stretch), None) {
                return Some(face);
            }
        }
        if self.system.is_some()
            && let Some(face) = self.match_listed(STANDARD, lang, (weight, style, stretch), None)
        {
            return Some(face);
        }
        let fallback = self.generic.sans_serif.as_deref().or_else(|| self.first_family());
        fallback
            .and_then(|family| self.match_family(family, weight, style, stretch, None))
            .or_else(|| {
                // Where the face of that family is not loaded, the first family whose face is,
                // and else that face after all rather than none.
                self.faces
                    .iter()
                    .filter(|face| !face.builtin)
                    .chain(self.faces.iter().filter(|face| face.builtin))
                    .find_map(|face| self.match_family(&face.family, weight, style, stretch, None))
                    .or_else(|| {
                        fallback.and_then(|family| {
                            self.match_loaded(family, false, (weight, style, stretch), None, false)
                        })
                    })
            })
    }

    /// The family of the first face added, before the fonts built into Fagus.
    fn first_family(&self) -> Option<&str> {
        self.faces
            .iter()
            .find(|face| !face.builtin)
            .or_else(|| self.faces.first())
            .map(|face| face.family.as_str())
    }

    /// A face that has a glyph for `character`: `preferred` (the face of the query for
    /// `families`), then the closest match for weight, style and width among the faces of
    /// `families` in their order, then of the other families, the fonts built into Fagus last.
    pub fn fallback(
        &self,
        families: &[&str],
        preferred: FaceId,
        character: char,
        request: (f32, FontStyle, f32),
    ) -> Option<FaceId> {
        self.fallback_in(families, preferred, character, request, None)
    }

    /// [`Self::fallback`] for text of a language (a BCP 47 tag), as Chrome falls back: after
    /// the families of the text, with the system's fonts the language's standard font (Times,
    /// or Hiragino for Japanese, PingFang for Chinese …), then the fonts that Core Text cascades
    /// to from `preferred` for the character's script (see [`cascade`]), then the other
    /// families, those of the system by the order of their files, the fonts built into Fagus
    /// last.
    pub fn fallback_in(
        &self,
        families: &[&str],
        preferred: FaceId,
        character: char,
        request: (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> Option<FaceId> {
        self.fallback_listed(families, preferred, character, request, lang)
            .0
    }

    /// [`Self::fallback_in`] for a character of a run of text, as Chrome shapes a run: each
    /// fallback face beyond the families of the text and the standard font serves all the
    /// characters of the run in the same script that it has, so a later character takes a face
    /// that an earlier one fell back to (`✔ ✓` both in Zapf Dingbats, where `✓` alone would take
    /// Lucida Grande) before one of its own. `run` keeps those faces from character to character.
    pub fn fallback_in_run(
        &self,
        run: &mut FallbackRun,
        families: &[&str],
        preferred: FaceId,
        character: char,
        request: (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> Option<FaceId> {
        let script = harfrust::unicode::script(character as u32);
        if ![
            harfrust::Script::COMMON,
            harfrust::Script::INHERITED,
            harfrust::Script::UNKNOWN,
        ]
        .contains(&script)
            && run.script != Some(script)
        {
            run.script = Some(script);
            run.faces.clear();
        }
        let (found, listed) = self.fallback_listed(families, preferred, character, request, lang);
        if listed || found == Some(preferred) {
            return found;
        }
        if let Some(earlier) = run
            .faces
            .iter()
            .copied()
            .find(|id| self.face(*id).has_glyph(character))
        {
            return Some(earlier);
        }
        if let Some(found) = found {
            run.faces.push(found);
        }
        found
    }

    /// [`Self::fallback_in`], with whether a family of the text or the standard font gave the
    /// face (which comes before the faces of other fallbacks in a run).
    fn fallback_listed(
        &self,
        families: &[&str],
        preferred: FaceId,
        character: char,
        (weight, style, stretch): (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> (Option<FaceId>, bool) {
        if !self.is_overlaid(preferred) && self.face(preferred).has_glyph(character) {
            return (Some(preferred), true);
        }
        let key = (query_key(families, weight, style, stretch, lang), character);
        if let Some(found) = self
            .queries
            .fallbacks
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).copied())
        {
            return found;
        }
        let request = (weight, style, stretch);
        let standard = || {
            self.system.as_ref()?;
            self.match_listed(STANDARD, lang, request, Some(character))
        };
        let listed = families
            .iter()
            .find_map(|family| self.match_listed(family, lang, request, Some(character)))
            .or_else(standard);
        let found = if listed.is_some() {
            (listed, true)
        } else {
            let has = |face: &Face| face.has_glyph(character);
            let rank = platform_rank(character);
            (
                self.beyond(preferred, character, lang, request, &has, &rank),
                false,
            )
        };
        if let Ok(mut cache) = self.queries.fallbacks.lock() {
            cache.insert(key, found);
        }
        found
    }

    /// A face beyond the families of the text for `character` that `has` accepts: with the
    /// system's fonts the standard font of the language first; then the families that Core Text
    /// cascades to from `preferred`; then the other families as [`Self::other_families`] orders
    /// them, with the system's fonts before the fonts built into Fagus.
    fn beyond(
        &self,
        preferred: FaceId,
        character: char,
        lang: Option<&str>,
        request: (f32, FontStyle, f32),
        has: &dyn Fn(&Face) -> bool,
        rank: &dyn Fn(&Face) -> u8,
    ) -> Option<FaceId> {
        let accepted = |id: FaceId| has(self.face(id)).then_some(id);
        // The standard font of the language, then the other families Chrome has for its script
        // (Times New Roman after Devanagari MT, which has no Latin letters).
        let standard = || {
            self.system.as_ref()?;
            self.match_listed(STANDARD, lang, request, Some(character))
                .and_then(accepted)
                .or_else(|| {
                    let script = lang.and_then(LangScript::of);
                    platform::script_families(Generic::Standard, script)
                        .iter()
                        .skip(1)
                        .find_map(|family| {
                            self.match_with(family, true, request, Some(character), true)
                                .and_then(accepted)
                        })
                })
        };
        let cascaded = || {
            cascade::families(character, self.face(preferred).cascade)
                .iter()
                .find_map(|family| {
                    self.match_with(family, true, request, Some(character), true)
                        .and_then(accepted)
                })
        };
        let others = self.other_families(has, rank);
        let in_family = |family: &str| {
            self.match_with(family, false, request, Some(character), true)
                .and_then(accepted)
        };
        // Chrome on Linux asks fontconfig for a font with the character, which ranks the fonts
        // for the text's language first (Noto Sans CJK KR for Korean), then the families of
        // `sans-serif` (DejaVu Sans before DejaVu Sans Mono), not the order of the files. It
        // asks without the weight and style of the text, so that fontconfig gives the regular
        // face, which Chrome makes bold or slanted where the text is (Blink's
        // `PlatformFallbackFontForCharacter`); only for a character of the script of a language
        // it has fonts of its own for (Japanese, Korean, Chinese, Hindi) does it find the face
        // of the text's weight and style (measured with Chrome 157 on Linux: bold Persian and
        // Thai text is DejaVu Sans and Noto Sans Thai made bold, bold Hindi text in `lang=hi` is
        // Noto Sans Devanagari Bold).
        let script = lang.and_then(LangScript::of);
        let styled = script.is_some_and(|script| script.writes(character));
        let fontconfig = |family: &str| {
            let request = if styled {
                request
            } else {
                (400.0, FontStyle::Normal, 100.0)
            };
            self.match_with(family, false, request, Some(character), true)
                .and_then(accepted)
        };
        let sans_serif = || {
            self.system.as_ref()?;
            platform::fallback_families(script)
                .iter()
                .copied()
                .chain(platform::generic_families(Generic::SansSerif, None))
                .find_map(fontconfig)
        };
        let system = || {
            let (_, face) = self.system.as_ref()?.covering(character)?;
            fontconfig(&face.family)
        };
        standard()
            .or_else(cascaded)
            .or_else(|| {
                others
                    .iter()
                    .filter(|(_, builtin)| !builtin)
                    .find_map(|(family, _)| in_family(family))
            })
            .or_else(sans_serif)
            .or_else(system)
            .or_else(|| {
                others
                    .iter()
                    .filter(|(_, builtin)| *builtin)
                    .find_map(|(family, _)| in_family(family))
            })
    }

    /// The families of the faces that `has` accepts, installed ones before the fonts built into
    /// Fagus, each in the order of its first face, those that `rank` puts first before the rest,
    /// with whether they are built in.
    fn other_families(&self, has: &dyn Fn(&Face) -> bool, rank: &dyn Fn(&Face) -> u8) -> Vec<(&str, bool)> {
        let mut others: Vec<(u8, &str, bool)> = Vec::new();
        for face in self
            .faces
            .iter()
            .filter(|face| !face.builtin)
            .chain(self.faces.iter().filter(|face| face.builtin))
        {
            if !others.iter().any(|(_, family, _)| *family == face.family) && has(face) {
                others.push((rank(face), &face.family, face.builtin));
            }
        }
        // A stable sort keeps the order of the faces among families of the same rank.
        others.sort_by_key(|(rank, _, _)| *rank);
        others
            .into_iter()
            .map(|(_, family, builtin)| (family, builtin))
            .collect()
    }

    /// A face for an emoji sequence (see [`sequence_len`]) that has glyphs for all its
    /// characters, as Chrome picks it: one in color first where variation selector 16 asks for
    /// color, else as [`Self::fallback`] does, but among the other families those in color
    /// first for emoji shown in color by default and those without color first for emoji shown
    /// as text. A face for its first character where none has them all.
    pub fn fallback_sequence(
        &self,
        families: &[&str],
        preferred: FaceId,
        sequence: &str,
        request: (f32, FontStyle, f32),
    ) -> Option<FaceId> {
        self.fallback_sequence_in(families, preferred, sequence, request, None)
    }

    /// [`Self::fallback_sequence`] for text of a language (a BCP 47 tag), as
    /// [`Self::fallback_in`] falls back; with the system's fonts, its emoji font (Apple Color
    /// Emoji, Segoe UI Emoji or Noto Color Emoji) comes first for emoji shown in color.
    pub fn fallback_sequence_in(
        &self,
        families: &[&str],
        preferred: FaceId,
        sequence: &str,
        (weight, style, stretch): (f32, FontStyle, f32),
        lang: Option<&str>,
    ) -> Option<FaceId> {
        /// The emoji fonts of macOS, Windows and Linux.
        const EMOJI: [&str; 3] = ["Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji"];
        let needed: Vec<char> = sequence.chars().filter(|c| !is_variation_selector(*c)).collect();
        let first = *needed.first()?;
        let covers = |face: &Face| needed.iter().all(|character| face.has_glyph(*character));
        let presentation = presentation(sequence);
        if presentation != Presentation::Emoji && !self.is_overlaid(preferred) && covers(self.face(preferred))
        {
            return Some(preferred);
        }
        let key = (
            query_key(families, weight, style, stretch, lang),
            Box::from(sequence),
        );
        if let Some(found) = self
            .queries
            .sequences
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).copied())
        {
            return found;
        }
        let request = (weight, style, stretch);
        // The families of the text, with whether they are generic ones resolved (which may be
        // hidden families of the system).
        let listed: Vec<(&str, bool)> = families
            .iter()
            .map(|family| {
                self.resolve_generic(family, lang)
                    .map_or((*family, false), |resolved| (resolved, true))
            })
            .collect();
        let in_family = |(family, hidden): (&str, bool)| {
            self.match_named(family, hidden, request, Some(first))
                .filter(|id| covers(self.face(*id)))
        };
        let color_first = |face: &Face| u8::from(!face.color);
        let in_color = |face: &Face| face.color && covers(face);
        let system_emoji = || {
            self.system.as_ref()?;
            EMOJI
                .iter()
                .find_map(|family| in_family((family, false)).filter(|id| self.face(*id).color))
        };
        let found = match presentation {
            Presentation::Emoji => listed
                .iter()
                .copied()
                .chain(
                    self.other_families(&in_color, &color_first)
                        .into_iter()
                        .map(|(family, _)| (family, false)),
                )
                .find_map(|family| in_family(family).filter(|id| self.face(*id).color))
                .or_else(system_emoji),
            _ => None,
        }
        .or_else(|| listed.iter().find_map(|family| in_family(*family)))
        .or_else(|| {
            let rank = |face: &Face| match presentation {
                Presentation::Text => u8::from(face.color),
                _ => u8::from(!face.color),
            };
            if self.system.is_some() {
                let default_emoji = || match presentation {
                    Presentation::DefaultEmoji => system_emoji(),
                    _ => None,
                };
                return default_emoji()
                    .or_else(|| self.beyond(preferred, first, lang, request, &covers, &rank));
            }
            self.other_families(&covers, &rank)
                .into_iter()
                .find_map(|(family, _)| in_family((family, false)))
        })
        .or_else(|| self.fallback_in(families, preferred, first, request, lang));
        if let Ok(mut cache) = self.queries.sequences.lock() {
            cache.insert(key, found);
        }
        found
    }

    /// The face for a family of the text: for a generic family the face of the family it stands
    /// for (see [`Self::resolve_generic`]), which may be a hidden family of the system. With the
    /// system's fonts, the fonts built into Fagus do not answer to their name (Inter) unless it
    /// is installed: Chrome goes on to the next family of the list where the system lacks it.
    fn match_listed(
        &self,
        family: &str,
        lang: Option<&str>,
        request: (f32, FontStyle, f32),
        character: Option<char>,
    ) -> Option<FaceId> {
        match self.resolve_generic(family, lang) {
            Some(resolved) => self.match_named(resolved, true, request, character),
            // A family the system lacks takes an installed one with the same metrics, as
            // fontconfig substitutes it on Linux and Chrome accepts it (Liberation Sans for
            // Arial, Liberation Mono for Courier New).
            None if self.system.is_some() && !self.has_family(family) => platform::metric_equivalents(family)
                .iter()
                .find(|equivalent| self.has_family(equivalent))
                .and_then(|equivalent| self.match_named(equivalent, false, request, character)),
            None => self.match_named(family, false, request, character),
        }
    }

    /// The family a generic family (or the standard font, [`STANDARD`]) stands for: the family
    /// set for it; with the system's fonts the first installed one that Chrome takes for it in
    /// text of `lang`; else for `serif` and `sans-serif` the first family added, for `monospace`
    /// a fixed-pitch one. `None` for other names, and for `cursive` and `fantasy` without the
    /// system's fonts.
    fn resolve_generic(&self, family: &str, lang: Option<&str>) -> Option<&str> {
        let generic = Generic::of(family)?;
        // Chrome knows `BlinkMacSystemFont`, but not `-apple-system`, nor `ui-serif`,
        // `ui-sans-serif`, `ui-monospace` and `ui-rounded` (only Safari does): a list such as
        // Tailwind's `ui-sans-serif, system-ui, sans-serif` gets the system font, not
        // Helvetica.
        if self.system.is_some() && (family.eq_ignore_ascii_case("-apple-system") || is_ui_family(family)) {
            return None;
        }
        let set = match generic {
            Generic::Standard | Generic::Serif => self.generic.serif.as_deref(),
            Generic::SansSerif => self.generic.sans_serif.as_deref(),
            Generic::SystemUi if self.system.is_none() => self.generic.sans_serif.as_deref(),
            Generic::Monospace => self.generic.monospace.as_deref(),
            _ => None,
        };
        if set.is_some() {
            return set;
        }
        // `math` takes the math font Chrome takes where it is there, installed or not, else
        // any face with a `MATH` table, else the standard font.
        if generic == Generic::Math {
            return platform::generic_families(generic, None)
                .into_iter()
                .find(|name| self.has_family(name))
                .or_else(|| {
                    self.faces
                        .iter()
                        .find(|face| face.has_math())
                        .map(|face| face.family.as_str())
                })
                .or_else(|| self.resolve_generic(STANDARD, lang));
        }
        if self.system.is_some() {
            let script = lang.and_then(LangScript::of);
            if let Some(found) = platform::generic_families(generic, script)
                .into_iter()
                .find(|name| self.has_family(name))
            {
                return Some(found);
            }
            if generic == Generic::SystemUi {
                return self.resolve_generic("sans-serif", lang);
            }
        }
        match generic {
            Generic::Cursive | Generic::Fantasy => None,
            Generic::Monospace => self.fixed_pitch_family().or_else(|| self.first_family()),
            _ => self.first_family(),
        }
    }

    /// What `monospace` stands for where no family is set for it, as browsers map it to a
    /// fixed-pitch system font: the family Chrome takes on macOS, Windows or Linux (`Menlo`,
    /// `Consolas`, `DejaVu Sans Mono`, `Liberation Mono`) if it is installed, else the first
    /// installed face whose glyphs are all equally wide, as its `post` table says.
    fn fixed_pitch_family(&self) -> Option<&str> {
        use skrifa::raw::TableProvider;
        const DEFAULTS: [&str; 4] = ["Menlo", "Consolas", "DejaVu Sans Mono", "Liberation Mono"];
        let installed = || self.faces.iter().filter(|face| !face.builtin && !face.document);
        DEFAULTS
            .iter()
            .find_map(|name| installed().find(|face| face.family.eq_ignore_ascii_case(name)))
            .or_else(|| {
                installed().find(|face| {
                    skrifa::FontRef::from_index(&face.data, face.index)
                        .ok()
                        .and_then(|font| font.post().ok())
                        .is_some_and(|post| post.is_fixed_pitch() != 0)
                })
            })
            .map(|face| face.family.as_str())
    }

    /// CSS Fonts 4, §5.2: among the faces of a family, pick the closest width, then style, then
    /// weight. The faces of `@font-face` rules with the same descriptors are one face whose
    /// rules cover characters by their `unicode-range`, the last rule first: for `character` the
    /// first rule that covers it and has its glyph (none if no rule does), without one the first
    /// that covers a space (the first available font). For `character`, a face of other fonts
    /// only if it has the glyph.
    fn match_family(
        &self,
        family: &str,
        weight: f32,
        style: FontStyle,
        stretch: f32,
        character: Option<char>,
    ) -> Option<FaceId> {
        self.match_named(family, false, (weight, style, stretch), character)
    }

    /// [`Self::match_family`], also among the hidden families of the system where `hidden`
    /// allows them. The faces of the system's family are parsed now if they are needed: if the
    /// document defines no face of the family.
    fn match_named(
        &self,
        family: &str,
        hidden: bool,
        request: (f32, FontStyle, f32),
        character: Option<char>,
    ) -> Option<FaceId> {
        self.match_with(family, hidden, request, character, false)
    }

    /// [`Self::match_named`], with the weights the system gives its faces for fallback where
    /// `cascaded` says so (see [`Face::cascade_weight`]). A face that is not loaded gives none.
    fn match_with(
        &self,
        family: &str,
        hidden: bool,
        request: (f32, FontStyle, f32),
        character: Option<char>,
        cascaded: bool,
    ) -> Option<FaceId> {
        self.match_loaded(family, hidden, request, character, cascaded)
            .filter(|id| !self.is_unloaded(*id))
    }

    /// [`Self::match_with`], whether the face is loaded or not.
    fn match_loaded(
        &self,
        family: &str,
        hidden: bool,
        (weight, style, stretch): (f32, FontStyle, f32),
        character: Option<char>,
        cascaded: bool,
    ) -> Option<FaceId> {
        let mut candidates: Vec<(FaceId, &Face)> = self
            .faces
            .iter()
            .enumerate()
            .filter(|(_, face)| face.family.eq_ignore_ascii_case(family))
            .map(|(index, face)| (FaceId(index as u32), &**face))
            .collect();
        // The system's faces of a family come after those added, as more installed faces of
        // the family: an added face hides only the system's faces of its style that overlap it
        // in weight and width (Arial Regular in a font directory leaves the system's Arial Bold
        // to bold text, where Chrome would find it). A family that the document defines hides
        // them all, so they are not parsed then.
        if !candidates.iter().any(|(_, face)| face.document)
            && let Some(system) = &self.system
        {
            let added: Vec<&Face> = candidates
                .iter()
                .filter(|(_, face)| !face.builtin)
                .map(|(_, face)| *face)
                .collect();
            candidates.extend(
                system
                    .family(family, hidden)
                    .into_iter()
                    .filter(|(_, face)| !added.iter().any(|added| added.replaces(face)))
                    .map(|(number, face)| (FaceId(number | SYSTEM_FACE), face)),
            );
        }
        if candidates.is_empty() {
            return None;
        }
        // Installed faces replace the built-in ones of the same family, and a family defined by
        // the document replaces an installed family of the same name.
        if candidates.iter().any(|(_, face)| !face.builtin) {
            candidates.retain(|(_, face)| !face.builtin);
        }
        if candidates.iter().any(|(_, face)| face.document) {
            candidates.retain(|(_, face)| face.document);
        }
        let closest = candidates
            .iter()
            .map(|(_, face)| stretch_distance(stretch, face.stretch))
            .fold(f32::INFINITY, f32::min);
        // The slants of all faces of the family bound the search for one (Blink's
        // `capabilities_bounds_`).
        let bounds = candidates
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), (_, face)| {
                (low.min(face.slope.0), high.max(face.slope.1))
            });
        candidates.retain(|(_, face)| stretch_distance(stretch, face.stretch) <= closest);
        // The closest slant, and among faces as close an italic face for italic text and an
        // oblique one for oblique text.
        let desired = style.slope();
        let category =
            |face: &Face| u8::from(std::mem::discriminant(&face.style) != std::mem::discriminant(&style));
        let distance = |face: &Face| (slope_distance(desired, face.slope, bounds), category(face));
        let nearest = candidates
            .iter()
            .map(|(_, face)| distance(face))
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let styled: Vec<(FaceId, &Face)> = candidates
            .iter()
            .filter(|(_, face)| Some(distance(face)) == nearest)
            .copied()
            .collect();
        let weights = |face: &Face| match face.cascade_weight {
            Some(weight) if cascaded => (weight, weight),
            _ => face.weight_range,
        };
        let &(id, best) = styled.iter().min_by(|(_, a), (_, b)| {
            range_distance(weight, weights(a)).total_cmp(&range_distance(weight, weights(b)))
        })?;
        if !best.document {
            return match character {
                Some(character) => best.has_glyph(character).then_some(id),
                None => Some(id),
            };
        }
        let mut rules = styled.iter().rev().filter(|(_, face)| {
            face.weight_range == best.weight_range && face.slope == best.slope && face.stretch == best.stretch
        });
        match character {
            Some(character) => rules
                .find(|(_, face)| face.has_glyph(character))
                .map(|(id, _)| *id),
            None => Some(rules.find(|(_, face)| face.covers(' ')).map_or(id, |(id, _)| *id)),
        }
    }

    /// Shapes `text` left to right with the default features of `face`. Results are cached.
    pub fn shape(&self, face: FaceId, text: &str) -> Arc<ShapedText> {
        self.shape_with(face, text, &[])
    }

    /// Shapes `text` left to right with `features` on top of the default features of `face`.
    /// Results are cached.
    pub fn shape_with(&self, face: FaceId, text: &str, features: &Features) -> Arc<ShapedText> {
        self.shape_variable(face, text, features, &[])
    }

    /// Shapes `text` as [`Self::shape_with`] does, with a variable face at `variations` (from
    /// [`Self::variations`]).
    pub fn shape_variable(
        &self,
        face: FaceId,
        text: &str,
        features: &Features,
        variations: &Variations,
    ) -> Arc<ShapedText> {
        self.shape_directed(face, text, features, variations, false)
    }

    /// Shapes `text` as [`Self::shape_variable`] does, in the script of its letters, right to
    /// left where `rtl` says: the glyphs then come in the order they stand on the page, the
    /// last character's first.
    pub fn shape_directed(
        &self,
        face: FaceId,
        text: &str,
        features: &Features,
        variations: &Variations,
        rtl: bool,
    ) -> Arc<ShapedText> {
        let flow = if rtl { Flow::RightToLeft } else { Flow::LeftToRight };
        self.shape_in_language(face, text, features, variations, flow, None)
    }

    /// Shapes `text` as [`Self::shape_variable`] does, top to bottom for a vertical line, with
    /// the face's vertical forms (`vert`, `vrt2`) and metrics. The glyphs advance down the line:
    /// `x_advance` is how far, and `x_offset` and `y_offset` how far down and how far to the right
    /// of the pen (the glyph's vertical origin, at the middle of its top edge) its horizontal
    /// origin lies, in font units.
    pub fn shape_vertical(
        &self,
        face: FaceId,
        text: &str,
        features: &Features,
        variations: &Variations,
    ) -> Arc<ShapedText> {
        self.shape_in_language(face, text, features, variations, Flow::TopToBottom, None)
    }

    /// Sets the language (a BCP 47 tag) that text without a language of its own is shaped in:
    /// Chrome shapes it in the language of its user interface, so that a face's German forms
    /// apply to text without `lang` in a German Chrome. Without one, such text is shaped in no
    /// language.
    pub fn set_default_language(&mut self, lang: Option<&str>) {
        self.default_language = lang.map(str::trim).filter(|lang| !lang.is_empty()).map(Arc::from);
    }

    /// Shapes `text` as [`Self::shape_directed`] and [`Self::shape_vertical`] do, in the
    /// direction of `flow`, for text in the language of the BCP 47 tag `lang` (else in the
    /// default language, [`Self::set_default_language`]): a face's forms for the language
    /// (`locl`, and what its language systems turn on or off) apply, as in Chrome, which shapes
    /// text in the language of its `lang`.
    pub fn shape_in_language(
        &self,
        face: FaceId,
        text: &str,
        features: &Features,
        variations: &Variations,
        flow: Flow,
        lang: Option<&str>,
    ) -> Arc<ShapedText> {
        self.shape_in_context(face, text, features, variations, (flow, lang), ("", ""))
    }

    /// Shapes `text` as [`Self::shape_in_language`] does (in the direction and language of
    /// `flow_lang`), as part of a longer text: `context` is the text before and after it, of
    /// which shaping looks at the five characters next to it for the forms of letters that join
    /// across its edges (`HarfBuzz`'s pre- and post-context), as Blink shapes the text of an
    /// inline box with the text of the whole paragraph around it. Context that cannot change
    /// the result (where neither it nor the text is of a joining script) is left out, so that
    /// such text is shaped and cached once.
    pub fn shape_in_context(
        &self,
        face: FaceId,
        text: &str,
        features: &Features,
        variations: &Variations,
        (flow, lang): (Flow, Option<&str>),
        context: (&str, &str),
    ) -> Arc<ShapedText> {
        let context = joining_context(text, context);
        let direction = match flow {
            Flow::LeftToRight => Direction::LeftToRight,
            Flow::RightToLeft => Direction::RightToLeft,
            Flow::TopToBottom => Direction::TopToBottom,
        };
        let face = self.face(face);
        // Only faces with forms for languages shape text of one apart.
        let lang: Option<Box<str>> = lang
            .map(str::trim)
            .filter(|lang| !lang.is_empty())
            .or(self.default_language.as_deref())
            .filter(|_| face.languages)
            .map(|lang| lang.to_ascii_lowercase().into_boxed_str());
        let key = (
            face.key,
            Box::<str>::from(text),
            Box::<Features>::from(features),
            coordinate_bits(variations),
            direction,
            lang,
        );
        let cache = || {
            self.shape_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        // Text with context is kept by it too.
        let joined =
            (context != ("", "")).then(|| (Box::<str>::from(context.0), Box::<str>::from(context.1)));
        let key = match joined {
            None => {
                if let Some(cached) = cache().get(&key) {
                    return cached;
                }
                (key, None)
            }
            Some((before, after)) => {
                let key = (key, before, after);
                if let Some(cached) = cache().joined.get(&key) {
                    return Arc::clone(cached);
                }
                let (key, before, after) = key;
                (key, Some((before, after)))
            }
        };
        let (key, joined) = key;
        let script = script_of(text);
        let language = key
            .5
            .as_deref()
            .and_then(|lang| lang.parse::<harfrust::Language>().ok());
        let shaped = if features.is_empty()
            && variations.is_empty()
            && direction == Direction::LeftToRight
            && script == harfrust::Script::LATIN
            && language.is_none()
        {
            Arc::new(shape_uncached(
                &face.font,
                text,
                &face.plan,
                &[],
                (direction, script, None),
                context,
            ))
        } else {
            let features: Vec<Feature> = features
                .iter()
                .map(|(tag, value)| Feature::new(Tag::new(tag), *value, ..))
                .collect();
            let font = self.instance(face, variations, &key.3);
            let plan = self.plan(
                face,
                &font,
                (&key.2, &key.3),
                &features,
                (direction, script, language.as_ref()),
            );
            Arc::new(shape_uncached(
                &font,
                text,
                &plan,
                &features,
                (direction, script, language.as_ref()),
                context,
            ))
        };
        match joined {
            None => cache().insert(key, Arc::clone(&shaped)),
            Some((before, after)) => cache().insert_joined((key, before, after), Arc::clone(&shaped)),
        }
        shaped
    }

    /// The coordinates at which text in `face` is set, as in Chrome: on its `wght` axis the
    /// weight, on `wdth` the width in percent, both within the ranges of the face's `@font-face`
    /// rule; for faces of `@font-face` rules on `slnt` the slant of `style` within the rule's
    /// (leaning the other way, as OpenType counts it), on `ital` 1 for `italic`; on `opsz` the
    /// size in CSS pixels where `optical_size` gives it (`font-optical-sizing: auto`); then
    /// `settings` (`font-variation-settings`). All within the axes; empty for a face without
    /// axes.
    pub fn variations(
        &self,
        face: FaceId,
        (weight, style, stretch): (f32, FontStyle, f32),
        optical_size: Option<f32>,
        settings: &Variations,
    ) -> Vec<([u8; 4], f32)> {
        let face = self.face(face);
        let mut variations = Vec::new();
        let ((lightest, heaviest), (narrowest, widest)) = face.variable;
        let italic_axis = style == FontStyle::Italic
            && face
                .axes
                .iter()
                .any(|axis| &axis.tag == b"ital" && axis.min <= 1.0 && axis.max >= 1.0);
        for axis in &face.axes {
            let value = match &axis.tag {
                b"wght" => Some(weight.clamp(lightest, heaviest)),
                b"wdth" => Some(stretch.clamp(narrowest, widest)),
                // Chrome sets these axes of web fonts only.
                b"ital" | b"slnt" if !face.document => None,
                b"ital" => Some(if italic_axis { 1.0 } else { 0.0 }),
                b"slnt" if italic_axis => Some(axis.default),
                b"slnt" if face.slope_auto => Some(-style.slope()),
                b"slnt" => Some(-style.slope().clamp(face.slope.0, face.slope.1)),
                b"opsz" => optical_size,
                _ => None,
            };
            let value = settings
                .iter()
                .rev()
                .find(|(tag, _)| *tag == axis.tag)
                .map(|(_, value)| *value)
                .or(value);
            if let Some(value) = value.filter(|value| value.is_finite()) {
                variations.push((axis.tag, value.clamp(axis.min, axis.max)));
            }
        }
        variations
    }

    /// The instance of a variable face at `variations`, made once.
    fn instance(&self, face: &Face, variations: &Variations, bits: &[([u8; 4], u32)]) -> Arc<Font> {
        let mut instances = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(font) = instances.get(&(face.key, Box::from(bits))) {
            return Arc::clone(font);
        }
        if instances.len() >= 256 {
            instances.clear();
        }
        let font = Arc::new(if variations.is_empty() {
            face.font.clone()
        } else {
            face.font
                .instance_builder()
                .variations(
                    variations
                        .iter()
                        .map(|(tag, value)| harfrust::font::Variation::new(Tag::new(tag), *value)),
                )
                .build()
        });
        instances.insert((face.key, Box::from(bits)), Arc::clone(&font));
        font
    }

    /// The shaping plan of a face or one of its instances with features, for text of a
    /// script in a direction, made once.
    fn plan(
        &self,
        face: &Face,
        font: &Font,
        (tags, bits): (&Features, &[([u8; 4], u32)]),
        features: &[Feature],
        (direction, script, language): (Direction, harfrust::Script, Option<&harfrust::Language>),
    ) -> Arc<ShapePlan> {
        let mut plans = self
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (
            face.key,
            Box::from(tags),
            Box::from(bits),
            direction,
            script,
            language.map(|language| Box::from(language.as_str())),
        );
        if let Some(plan) = plans.get(&key) {
            return Arc::clone(plan);
        }
        // A document uses few sets of features; an engine that renders many documents with
        // many faces starts over now and then.
        if plans.len() >= 1024 {
            plans.clear();
        }
        let plan = Arc::new(ShapePlan::new(font, direction, Some(script), language, features));
        plans.insert(key, Arc::clone(&plan));
        plan
    }
}

/// Whether a family is one of the `ui-` generic families of CSS Fonts 4 (`ui-serif`,
/// `ui-sans-serif`, `ui-monospace`, `ui-rounded`), in any case.
fn is_ui_family(family: &str) -> bool {
    family
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ui-"))
        && Generic::of(family).is_some()
}

/// [`weight_distance`] for a face that covers a range of weights.
/// The key of a query: a hash of the families, then the weight, the style and the width.
/// How early fallback tries a face for a character beyond the families of the text (lower is
/// earlier): Chrome on macOS takes the system's font for the script (Core Text's cascade list),
/// which for Hangul is Apple SD Gothic Neo; the fonts other systems use come next.
fn platform_rank(character: char) -> impl Fn(&Face) -> u8 {
    const HANGUL: [&str; 4] = [
        "Apple SD Gothic Neo",
        "Malgun Gothic",
        "Noto Sans CJK KR",
        "Noto Sans KR",
    ];
    let hangul = matches!(character,
        '\u{1100}'..='\u{11FF}'
        | '\u{3130}'..='\u{318F}'
        | '\u{A960}'..='\u{A97F}'
        | '\u{AC00}'..='\u{D7FF}');
    move |face: &Face| {
        if hangul {
            HANGUL
                .iter()
                .position(|name| face.family.eq_ignore_ascii_case(name))
                .map_or(u8::MAX, |index| index as u8)
        } else {
            0
        }
    }
}

fn query_key(families: &[&str], weight: f32, style: FontStyle, stretch: f32, lang: Option<&str>) -> QueryKey {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    families.hash(&mut hasher);
    lang.hash(&mut hasher);
    let style = match style {
        FontStyle::Normal => 0,
        FontStyle::Italic => 1,
        FontStyle::Oblique(tenths) => 0x1_0000 | u32::from(tenths.cast_unsigned()),
    };
    (hasher.finish(), weight.to_bits(), style, stretch.to_bits())
}

/// Ordering key for the style matching rules of CSS Fonts 4 (§5.2, step 4b) by slant in degrees,
/// as Blink orders faces (`FontSelectionAlgorithm::StyleDistance`, from `WebKit`, BSD-licensed; see
/// `CHROMIUM-LICENSE`): a slant the face covers first; for italic slants (14 degrees and more)
/// steeper faces, then shallower ones; for other positive slants shallower faces down to
/// upright, then steeper ones, then those leaning back; negative slants mirror that. `bounds`
/// are the lowest and highest slants of the family's faces. Smaller is better.
fn slope_distance(desired: f32, (low, high): (f32, f32), bounds: (f32, f32)) -> f32 {
    if (low..=high).contains(&desired) {
        return 0.0;
    }
    if desired >= ITALIC_SLOPE {
        if low > desired {
            return low - desired;
        }
        return desired.max(bounds.1) - high;
    }
    if desired >= 0.0 {
        if high >= 0.0 && high < desired {
            return desired - high;
        }
        if low > desired {
            return low;
        }
        return desired.max(bounds.1) - high;
    }
    if desired > -ITALIC_SLOPE {
        if low > desired && low <= 0.0 {
            return low - desired;
        }
        if high < desired {
            return -high;
        }
        return low - desired.min(bounds.0);
    }
    if high < desired {
        return desired - high;
    }
    low - desired.min(bounds.0)
}

/// Ordering key for the width matching rules of CSS Fonts 4 (§5.2, step 4a): a width the face
/// covers first; for a desired width up to normal narrower widths, closest first, then wider
/// ones; for a wider one the other way round. Smaller is better.
fn stretch_distance(desired: f32, (low, high): (f32, f32)) -> f32 {
    if (low..=high).contains(&desired) {
        0.0
    } else if (high < desired) == (desired <= 100.0) {
        (desired - high).abs().min((desired - low).abs())
    } else {
        1000.0 + (desired - high).abs().min((desired - low).abs())
    }
}

fn range_distance(desired: f32, (low, high): (f32, f32)) -> f32 {
    if (low..=high).contains(&desired) {
        0.0
    } else if desired < low {
        weight_distance(desired, low)
    } else {
        weight_distance(desired, high)
    }
}

/// Ordering key for the weight matching rules of CSS Fonts 4: smaller is better.
fn weight_distance(desired: f32, available: f32) -> f32 {
    if (available - desired).abs() < f32::EPSILON {
        return 0.0;
    }
    let below = available < desired;
    if (400.0..=500.0).contains(&desired) {
        if !below && available <= 500.0 {
            available - desired
        } else if below {
            1000.0 + (desired - available)
        } else {
            2000.0 + (available - desired)
        }
    } else if desired < 400.0 {
        if below {
            desired - available
        } else {
            1000.0 + (available - desired)
        }
    } else if below {
        1000.0 + (desired - available)
    } else {
        available - desired
    }
}

/// The name that stands for the standard font: what text is set in without `font-family`, and
/// what Chrome tries after the families of the text.
pub const STANDARD: &str = "-webkit-standard";

static NEXT_FACE_KEY: AtomicU64 = AtomicU64::new(0);

/// What an `@font-face` rule says about its face (CSS Fonts 4, §4).
#[derive(Clone, Debug, PartialEq)]
pub struct Descriptors {
    pub family: String,
    /// `font-weight`: a weight or a range; `None` for `auto`, where faces match as normal
    /// weight and the `wght` axis of a variable face spans the axis.
    pub weight: Option<(f32, f32)>,
    pub style: FontStyle,
    /// The slants of `font-style: oblique <angle> <angle>` in degrees, where the rule gives a
    /// range; `None` for a single style, which `style` says.
    pub slope: Option<(f32, f32)>,
    /// Whether the rule gives no `font-style` (`auto`): `style` is then normal for matching.
    pub style_auto: bool,
    /// `font-stretch` in percent: a width or a range; `None` for `auto`, as with `weight`.
    pub stretch: Option<(f32, f32)>,
    /// `unicode-range`: the characters the face is used for, as inclusive ranges; empty for all.
    pub unicode_range: Vec<(u32, u32)>,
    /// `size-adjust`: how much larger than the font size the face is set (1 for `100%`).
    pub size_adjust: f32,
    /// `font-feature-settings`: the OpenType features the face is set with, before those of
    /// the style (CSS Fonts 4, §7.2).
    pub features: Vec<([u8; 4], u32)>,
    /// `ascent-override`, `descent-override` and `line-gap-override` as fractions of the font
    /// size; `None` for `normal`, where the font's own metrics hold.
    pub ascent_override: Option<f32>,
    pub descent_override: Option<f32>,
    pub line_gap_override: Option<f32>,
}

impl Default for Descriptors {
    fn default() -> Self {
        Self {
            family: String::new(),
            weight: None,
            style: FontStyle::Normal,
            slope: None,
            style_auto: true,
            stretch: None,
            unicode_range: Vec::new(),
            size_adjust: 1.0,
            features: Vec::new(),
            ascent_override: None,
            descent_override: None,
            line_gap_override: None,
        }
    }
}

impl Face {
    /// Face `index` of font data, with the family, weights, style and widths that an
    /// `@font-face` rule gives it: they replace what the font itself says. The rule's
    /// `size-adjust` scales the face, and its overrides replace the font's ascent, descent and
    /// line gap (in proportion to the adjusted size, as in Chrome).
    pub fn from_rule(data: Arc<Vec<u8>>, index: u32, rule: &Descriptors) -> Option<Self> {
        if !face_sound(&data, index) {
            return None;
        }
        let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(&data) as _;
        let font = Font::new(blob, index)?;
        let mut face = make_face(font, data, index);
        rule.family.clone_into(&mut face.family);
        let weight = rule.weight.unwrap_or((400.0, 400.0));
        face.weight = weight.0;
        face.weight_range = weight;
        face.weight_auto = rule.weight.is_none();
        face.stretch = rule.stretch.unwrap_or((100.0, 100.0));
        face.variable = (
            rule.weight.unwrap_or(face.variable.0),
            rule.stretch.unwrap_or(face.variable.1),
        );
        face.style = rule.style;
        face.slope = rule.slope.unwrap_or_else(|| {
            let slope = rule.style.slope();
            (slope, slope)
        });
        face.slope_auto = rule.style_auto;
        face.document = true;
        face.unicode_range.clone_from(&rule.unicode_range);
        for code in 0..256u32 {
            if !char::from_u32(code).is_some_and(|character| face.covers(character)) {
                face.latin[(code / 64) as usize] &= !(1 << (code % 64));
            }
        }
        if rule.size_adjust.is_finite() && rule.size_adjust >= 0.0 {
            face.size_adjust = rule.size_adjust;
        }
        face.features.clone_from(&rule.features);
        // The overrides are fractions of the (adjusted) font size: of an em in font units.
        let units = |fraction: Option<f32>| {
            fraction
                .filter(|fraction| fraction.is_finite() && *fraction >= 0.0)
                .map(|fraction| fraction * face.metrics.units_per_em)
        };
        if let Some(ascent) = units(rule.ascent_override) {
            face.metrics.ascender = ascent;
        }
        if let Some(descent) = units(rule.descent_override) {
            face.metrics.descender = descent;
        }
        if let Some(gap) = units(rule.line_gap_override) {
            face.metrics.line_gap = gap;
        }
        Some(face)
    }
}

/// Whether face `index` of font data is sound enough to use, as Chrome's font sanitizer
/// requires: its size of the em square is within the 16 to 16,384 units that OpenType allows
/// (glyphs are scaled by it, so 0 made every size infinite), its color tables are sound
/// (see [`palette::color_tables_sound`]) and its `post` table is (see [`post_sound`]).
fn face_sound(data: &[u8], index: u32) -> bool {
    use skrifa::raw::TableProvider;
    skrifa::FontRef::from_index(data, index).is_ok_and(|font| {
        font.head()
            .is_ok_and(|head| (16..=16_384).contains(&head.units_per_em()))
            && palette::color_tables_sound(&font)
            && font
                .table_data(Tag::new(b"post"))
                .is_none_or(|post| post_sound(post.as_bytes()))
    })
}

/// Whether a `post` table holds the number of glyphs where its version says it does: one of
/// version 2.0 has it right after the 32 bytes of its header. krilla counts the glyph names of
/// a font when it writes it, and the read-fonts it reads fonts with unwraps that number (a
/// table shorter than the header it does not read at all). The bytes are looked at rather than
/// parsed, so that what is found does not depend on a version of read-fonts.
fn post_sound(post: &[u8]) -> bool {
    !(post.starts_with(&0x0002_0000u32.to_be_bytes()) && (32..34).contains(&post.len()))
}

fn make_face(font: Font, data: Arc<Vec<u8>>, index: u32) -> Face {
    let names = font.names();
    let family = [NameId::TYPOGRAPHIC_FAMILY_NAME, NameId::FAMILY_NAME]
        .into_iter()
        .find_map(|id| {
            names
                .get(id)
                .and_then(|name| name.english_or_first())
                .map(|name| name.name().chars().collect::<String>())
        })
        .unwrap_or_default();
    let attributes = font.default_attributes();
    let style = match attributes.style {
        harfrust::font::attribute::Style::Normal => FontStyle::Normal,
        harfrust::font::attribute::Style::Italic => FontStyle::Italic,
        harfrust::font::attribute::Style::Oblique(_) => FontStyle::OBLIQUE,
    };
    let metrics = face_metrics(&font);
    let plan = ShapePlan::new(
        &font,
        Direction::LeftToRight,
        Some(harfrust::Script::LATIN),
        None,
        &[],
    );
    let weight = attributes.weight.value();
    let axes: Vec<Axis> = font
        .axes()
        .iter()
        .map(|axis| Axis {
            tag: axis.tag().to_be_bytes(),
            min: axis.min_value(),
            default: axis.default_value(),
            max: axis.max_value(),
        })
        .collect();
    // A variable font covers the weights of its `wght` axis and the widths of its `wdth` axis.
    let axis_range = |tag: &[u8; 4], value: f32| {
        axes.iter()
            .find(|axis| &axis.tag == tag)
            .map_or((value, value), |axis| (axis.min, axis.max))
    };
    let weight_range = axis_range(b"wght", weight);
    let stretch = axis_range(b"wdth", attributes.width.percentage());
    let (substitutions, positionings) = {
        use skrifa::raw::TableProvider;
        let tags = |list: Option<skrifa::raw::tables::layout::FeatureList<'_>>| {
            let mut tags: Vec<[u8; 4]> = list
                .map(|list| {
                    list.feature_records()
                        .iter()
                        .map(|record| record.feature_tag().to_be_bytes())
                        .collect()
                })
                .unwrap_or_default();
            tags.sort_unstable();
            tags.dedup();
            tags
        };
        let tables = font.tables();
        (
            tags(tables.gsub().ok().and_then(|gsub| gsub.feature_list().ok())),
            tags(tables.gpos().ok().and_then(|gpos| gpos.feature_list().ok())),
        )
    };
    let languages = {
        use skrifa::raw::TableProvider;
        let any = |list: Option<skrifa::raw::tables::layout::ScriptList<'_>>| {
            list.is_some_and(|list| {
                list.script_records().iter().any(|record| {
                    record
                        .script(list.offset_data())
                        .is_ok_and(|script| !script.lang_sys_records().is_empty())
                })
            })
        };
        let tables = font.tables();
        any(tables.gsub().ok().and_then(|gsub| gsub.script_list().ok()))
            || any(tables.gpos().ok().and_then(|gpos| gpos.script_list().ok()))
    };
    let (vertical_metrics, vertical_origins, color) = {
        use skrifa::raw::TableProvider;
        let tables = font.tables();
        (
            tables.vmtx().is_ok(),
            tables.vorg().is_ok(),
            tables.sbix().is_ok() || tables.cbdt().is_ok() || tables.colr().is_ok(),
        )
    };
    let outlines =
        skrifa::FontRef::from_index(&data, index).map_or(Outlines::None, |font| Outlines::of(&font));
    let cascade = Cascade::of(&family);
    let trak = tracking_table(&font);
    let mut latin = [0u64; 4];
    let charmap = font.charmap();
    for code in 0..256u32 {
        if char::from_u32(code)
            .and_then(|character| charmap.map_unicode(character))
            .is_some_and(|glyph| glyph.to_u32() < font.num_glyphs())
        {
            latin[(code / 64) as usize] |= 1 << (code % 64);
        }
    }
    let slope = style.slope();
    let mut face = Face {
        family,
        weight,
        weight_range,
        style,
        slope: (slope, slope),
        weight_auto: false,
        slope_auto: false,
        own_bold: weight >= 600.0,
        own_italic: style.is_slanted(),
        size_adjust: 1.0,
        features: Vec::new(),
        document: false,
        builtin: false,
        key: NEXT_FACE_KEY.fetch_add(1, Ordering::Relaxed),
        stretch,
        variable: (weight_range, stretch),
        unicode_range: Vec::new(),
        substitutions,
        positionings,
        languages,
        vertical_metrics,
        vertical_origins,
        color,
        outlines,
        cascade,
        cascade_weight: None,
        trak,
        axes,
        data,
        index,
        metrics,
        font,
        plan,
        latin,
        bounds: Mutex::default(),
        intercepts: Mutex::default(),
        ink_boxes: Mutex::default(),
        grid_fit: GridFit::default(),
    };
    // Where the OS/2 table gives no x-height or cap height (before its version 2, or 0), Chrome
    // on Linux takes the top of the "x" and of the "H" (Skia's `getCBoxForLetter`), which
    // FreeType fits to the pixel grid in TrueType faces (see `Face::x_height_at`).
    let (own_x_height, own_cap_height) = {
        use skrifa::raw::TableProvider;
        face.font.tables().os2().ok().map_or((false, false), |os2| {
            (
                os2.sx_height().is_some_and(|height| height > 0),
                os2.s_cap_height().is_some_and(|height| height > 0),
            )
        })
    };
    let truetype = {
        use skrifa::raw::TableProvider;
        face.font.tables().glyf().is_ok()
    };
    let top = |character: char| {
        face.glyph(character)
            .and_then(|glyph| face.point_top(glyph))
            .filter(|top| *top > 0.0)
    };
    let x = top('x').filter(|_| !own_x_height);
    let cap = top('H').filter(|_| !own_cap_height);
    // The auto-hinter's blue zone of the small letters: the median top of the round ones.
    let mut rounds: Vec<f32> = ['o', 'e', 's', 'c'].into_iter().filter_map(top).collect();
    rounds.sort_by(f32::total_cmp);
    let overshoot = rounds.get(rounds.len() / 2).map(|top| top.round() as i64);
    if let Some(x) = x {
        face.metrics.x_height = x;
    }
    if let Some(cap) = cap {
        face.metrics.cap_height = cap;
    }
    if truetype {
        face.grid_fit = GridFit {
            x: x.map(|top| top.round() as i64),
            cap: cap.map(|top| top.round() as i64),
            overshoot,
        };
    }
    face
}

/// Whether face `index` of font data has `name` (in any case) as its PostScript name or its full
/// name, in any language.
fn full_names(data: &[u8], index: u32, name: &str) -> bool {
    use skrifa::MetadataProvider;
    use skrifa::string::StringId;
    let Ok(font) = skrifa::FontRef::from_index(data, index) else {
        return false;
    };
    [StringId::POSTSCRIPT_NAME, StringId::FULL_NAME]
        .into_iter()
        .any(|id| {
            font.localized_strings(id)
                .any(|string| string.to_string().eq_ignore_ascii_case(name))
        })
}

/// The normal track (or the first) of a face's horizontal `trak` table as sizes and values,
/// if shaping applies it: where the face has a `STAT` table too. Empty otherwise.
fn tracking_table(font: &Font) -> Vec<(f32, f32)> {
    use skrifa::raw::TableProvider;
    let tables = font.tables();
    if tables.stat().is_err() {
        return Vec::new();
    }
    let Ok(trak) = tables.trak() else {
        return Vec::new();
    };
    let data = trak.offset_data();
    let Some(Ok(horizontal)) = trak.horiz().transpose().ok().flatten().map(Ok::<_, ()>) else {
        return Vec::new();
    };
    let Some(sizes) = horizontal.size_table(data) else {
        return Vec::new();
    };
    let tracks = horizontal.track_table();
    let Some(track) = tracks
        .iter()
        .find(|track| track.track().to_f32() == 0.0)
        .or_else(|| tracks.first())
    else {
        return Vec::new();
    };
    let Some(values) = track.per_size_values(data, sizes.len() as u16) else {
        return Vec::new();
    };
    sizes
        .iter()
        .zip(values)
        .map(|(size, value)| (size.get().to_f32(), f32::from(value.get())))
        .collect()
}

/// The height `top` (font units) of a TrueType face at `pixels` as `FreeType` gives it for Chrome on
/// Linux, in `FreeType`'s fixed-point arithmetic: in 1/64 pixels below 8 pixels, which Chrome does
/// not hint (measured); from 8 pixels on as the auto-hinter fits it, for Chrome hints lightly
/// (fontconfig's `hintslight`): the vertical scale stretched so that the `overshoot` of the round
/// small letters ends on a whole pixel, rounded up from 3/8 of one (`af_latin_metrics_scale_dim`),
/// then the height rounded to whole pixels (its blue zone's fitted reference). Chrome 157 gives
/// exactly this for the x-height and cap height of the `DejaVu` faces up to 52 pixels (measured in
/// steps of 1/4 pixel); above that it is up to 0.3 pixels more.
#[allow(clippy::cast_possible_truncation, reason = "a font size in 26.6 fixed point")]
fn grid_fitted(top: i64, overshoot: Option<i64>, pixels: f32, units_per_em: i64) -> f32 {
    let sign = |value: i64| if value < 0 { -1 } else { 1 };
    let mul_div = |a: i64, b: i64, c: i64| {
        if c == 0 {
            return 0;
        }
        sign(a) * sign(b) * sign(c) * ((a.abs() * b.abs() + c.abs() / 2) / c.abs())
    };
    let mul_fix = |a: i64, b: i64| mul_div(a, b, 0x10000);
    // Skia sets the size in 26.6 points, cut (`SkScalarToFDot6`).
    let size = (pixels * 64.0) as i64;
    let mut scale = mul_div(size, 0x10000, units_per_em.max(1));
    if pixels < 8.0 {
        return mul_fix(top, scale) as f32 / 64.0;
    }
    if let Some(overshoot) = overshoot {
        let scaled = mul_fix(overshoot, scale);
        let fitted = (scaled + 40) & !63;
        if scaled != fitted && scaled > 0 {
            scale = mul_div(scale, fitted, scaled);
        }
    }
    ((mul_fix(top, scale) + 32) & !63) as f32 / 64.0
}

/// What Chrome adds to the rounded `ascent` of an installed face of `family` with the rounded
/// `descent` (see [`Face::ascent_adjustment`]).
fn ascent_adjustment(family: &str, ascent: f32, descent: f32) -> f32 {
    if matches!(family, "Times" | "Helvetica" | "Courier") {
        ((ascent + descent) * 0.15 + 0.5).floor()
    } else {
        0.0
    }
}

fn face_metrics(font: &Font) -> FaceMetrics {
    let metrics = font.metrics();
    let style = font.style_metrics();
    let units_per_em = f32::from(metrics.units_per_em.max(1));
    let line = if metrics.use_typo_metrics {
        metrics.typo_line.or(metrics.hhea_line)
    } else {
        metrics.hhea_line.or(metrics.typo_line)
    };
    let (ascender, descender, line_gap) =
        line.map_or((units_per_em * 0.8, units_per_em * 0.2, 0.0), |line| {
            (
                line.ascender.to_f64() as f32,
                -(line.descender.to_f64() as f32),
                line.line_gap.to_f64() as f32,
            )
        });
    let (typo_ascender, typo_descender) = metrics.typo_line.map_or((0.0, 0.0), |line| {
        (line.ascender.to_f64() as f32, -(line.descender.to_f64() as f32))
    });
    let x_height = metrics
        .x_height
        .map(|value| value.to_f64() as f32)
        .filter(|value| *value > 0.0)
        .unwrap_or(units_per_em * 0.5);
    let cap_height = metrics
        .cap_height
        .map(|value| value.to_f64() as f32)
        .filter(|value| *value > 0.0)
        .unwrap_or(units_per_em * 0.7);
    let underline = style.underline;
    let strike = style.strike_through;
    let (average_width, bounding_width) = {
        use skrifa::raw::TableProvider;
        let tables = font.tables();
        (
            tables
                .os2()
                .map_or(0.0, |os2| f32::from(os2.x_avg_char_width().max(0))),
            tables
                .head()
                .map_or(0.0, |head| f32::from(head.x_max()) - f32::from(head.x_min())),
        )
    };
    FaceMetrics {
        units_per_em,
        ascender,
        descender,
        line_gap,
        typo_ascender,
        typo_descender,
        x_height,
        cap_height,
        average_width,
        bounding_width,
        underline_position: underline.map_or(units_per_em * 0.1, |decoration| {
            -(decoration.position.to_f64() as f32)
        }),
        underline_thickness: underline.map_or(units_per_em * 0.05, |decoration| {
            decoration.thickness.to_f64() as f32
        }),
        strikeout_position: strike.map_or(x_height / 2.0, |decoration| decoration.position.to_f64() as f32),
        strikeout_thickness: strike.map_or(units_per_em * 0.05, |decoration| {
            decoration.thickness.to_f64() as f32
        }),
    }
}

/// Whether a character is of a cursive script, whose letters join and admit no gaps for
/// letter spacing (Blink's `IsCursiveScript`: Arabic, Hanifi Rohingya, Mandaic, Mongolian, N'Ko,
/// Phags-pa, Syriac); `None` for one of no script of its own (spaces, European digits,
/// punctuation, combining marks), which takes that of the text around it. The characters of the
/// Arabic blocks count as Arabic, as their script extensions make them in Blink's runs: the
/// tatweel, the Arabic comma and the Arabic-Indic digits.
pub fn cursive_script(character: char) -> Option<bool> {
    if matches!(character,
        '\u{0600}'..='\u{061B}' | '\u{061D}'..='\u{06FF}'
        | '\u{0750}'..='\u{077F}'
        | '\u{0870}'..='\u{08FF}'
        | '\u{FB50}'..='\u{FDFF}'
        | '\u{FE70}'..='\u{FEFE}')
    {
        return Some(true);
    }
    let script = harfrust::unicode::script(character as u32);
    if [
        harfrust::Script::COMMON,
        harfrust::Script::INHERITED,
        harfrust::Script::UNKNOWN,
    ]
    .contains(&script)
    {
        return None;
    }
    Some(
        [
            harfrust::Script::ARABIC,
            harfrust::Script::HANIFI_ROHINGYA,
            harfrust::Script::MANDAIC,
            harfrust::Script::MONGOLIAN,
            harfrust::Script::NKO,
            harfrust::Script::PHAGS_PA,
            harfrust::Script::SYRIAC,
        ]
        .contains(&script),
    )
}

/// How many characters before and after a text `HarfBuzz` keeps as its context.
const CONTEXT_CHARS: usize = 5;

/// Whether `character` joins with the letters next to it: a letter of a cursive script, or a
/// zero width joiner.
#[inline]
pub fn joins(character: char) -> bool {
    // No script before Arabic joins.
    character >= '\u{600}' && (character == '\u{200D}' || cursive_script(character) == Some(true))
}

/// Whether `text` has a character that [`joins`]. Text without characters from U+0600 on (whose
/// UTF-8 starts with a byte below 0xD8), as Latin text, is passed over byte by byte.
#[inline]
pub fn has_joining(text: &str) -> bool {
    // The largest byte of each chunk, which the compiler finds many bytes at a time.
    let high = |chunk: &[u8]| chunk.iter().fold(0, |high: u8, &byte| high.max(byte)) >= 0xD8;
    text.as_bytes().chunks(64).any(high) && text.chars().any(joins)
}

/// The part of `(before, after)` that shaping `text` looks at (the characters `HarfBuzz` keeps),
/// or nothing on a side where neither it nor the text has a letter that joins.
fn joining_context<'a>(text: &str, (before, after): (&'a str, &'a str)) -> (&'a str, &'a str) {
    if (before.is_empty() && after.is_empty()) || !has_joining(text) {
        return ("", "");
    }
    let start = before
        .char_indices()
        .rev()
        .nth(CONTEXT_CHARS - 1)
        .map_or(0, |(at, _)| at);
    let end = after
        .char_indices()
        .nth(CONTEXT_CHARS)
        .map_or(after.len(), |(at, _)| at);
    let (before, after) = (&before[start..], &after[..end]);
    (
        if has_joining(before) { before } else { "" },
        if has_joining(after) { after } else { "" },
    )
}

/// The script of the first letter of `text` that has one: Latin where none has.
fn script_of(text: &str) -> harfrust::Script {
    let mut buffer = Buffer::new();
    buffer.push_str(text);
    buffer.set_direction(Direction::LeftToRight);
    buffer.guess_segment_properties();
    buffer.script().unwrap_or(harfrust::Script::LATIN)
}

/// Whether Core Text synthesizes a glyph for a character that a face lacks, from the face's
/// hyphen-minus: the hyphen and the non-breaking hyphen. Chrome on macOS takes these glyphs
/// before it falls back to another font (`HarfBuzzGetGlyph` in Blink's `harfbuzz_face.cc`), so
/// that Arial, Georgia, Times and web fonts without them show their own hyphen.
fn synthesized_hyphen(character: char) -> bool {
    matches!(character, '\u{2010}' | '\u{2011}')
}

/// Whether shaping hides a character where the face has no glyph for it, so that it needs none
/// and no other font: `HarfBuzz`'s default ignorable characters (zero width spaces and joiners,
/// bidi controls, the word joiner and invisible operators, the combining grapheme joiner,
/// Mongolian free variation selectors, variation selectors, tags …), not the Hangul fillers,
/// which Chrome draws.
/// Whether shaping draws `character`, a space separator, with the space of a face that lacks
/// it, widened or narrowed to its width (`HarfBuzz`'s `space_fallback`). Not the ideographic
/// space, which Chrome takes from another font (PingFang SC after Menlo), measured.
fn has_space_fallback(character: char) -> bool {
    matches!(
        character,
        '\u{A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}'
    )
}

pub fn is_ignorable(character: char) -> bool {
    matches!(character,
        '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFF0}'..='\u{FFF8}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E0FFF}')
}

/// The character whose glyph a character is drawn with: a line separator (U+2028) or a
/// paragraph separator (U+2029) with the space, as Blink's `HarfBuzzGetGlyph` replaces them so
/// that fonts with symbols for them do not show those; the character itself otherwise.
pub fn drawn_as(character: char) -> char {
    match character {
        '\u{2028}' | '\u{2029}' => ' ',
        _ => character,
    }
}

/// Puts `text` into a buffer to shape with `font`, with clusters at the characters' byte
/// offsets; a hyphen the font lacks is shaped as its hyphen-minus (see [`synthesized_hyphen`]),
/// a line or paragraph separator as a space (see [`drawn_as`]).
fn push_text(buffer: &mut Buffer, font: &Font, text: &str) {
    if !text
        .chars()
        .any(|character| synthesized_hyphen(character) || drawn_as(character) != character)
    {
        buffer.push_str(text);
        return;
    }
    let charmap = font.charmap();
    let substitute = |character: char| {
        if synthesized_hyphen(character)
            && charmap.map_unicode(character).is_none()
            && charmap.map_unicode('-').is_some()
        {
            '-'
        } else {
            drawn_as(character)
        }
    };
    for (at, character) in text.char_indices() {
        buffer.push(
            u32::from(substitute(character)),
            u32::try_from(at).unwrap_or(u32::MAX),
        );
    }
}

fn shape_uncached(
    font: &Font,
    text: &str,
    plan: &ShapePlan,
    features: &[Feature],
    (direction, script, language): (Direction, harfrust::Script, Option<&harfrust::Language>),
    (before, after): (&str, &str),
) -> ShapedText {
    let shaper = ShaperFont::new(font);
    let mut buffer = Buffer::new();
    push_text(&mut buffer, font, text);
    buffer.set_pre_context(before);
    buffer.set_post_context(after);
    buffer.set_direction(direction);
    buffer.set_script(Some(script));
    buffer.set_language(language.cloned());
    let shaped = harfrust::shape(
        &shaper,
        &mut buffer,
        ShapeOptions::new().plan(Some(plan)).features(features),
    );
    if shaped.is_err() {
        return ShapedText {
            glyphs: Vec::new(),
            advance: 0.0,
        };
    }
    // Top to bottom the pen moves down (harfrust's advances are negative there, upward
    // positive); the glyphs keep their advance along the line in `x_advance`.
    let vertical = direction == Direction::TopToBottom;
    // A `cmap` may map characters to glyphs past those the font has (format 12 numbers them
    // with 32 bits): such a glyph is missing, drawn as `.notdef`.
    let count = font.num_glyphs();
    let glyphs: Vec<ShapedGlyph> = buffer
        .glyph_infos()
        .iter()
        .zip(buffer.glyph_positions())
        .map(|(info, position)| ShapedGlyph {
            id: if info.glyph_id < count { info.glyph_id } else { 0 },
            cluster: info.cluster,
            x_advance: if vertical {
                -position.y_advance as f32
            } else {
                position.x_advance as f32
            },
            own_advance: if vertical {
                -position.y_advance as f32
            } else {
                shaper.glyph_h_advance(info.glyph_id.into()) as f32
            },
            x_offset: if vertical {
                -position.y_offset as f32
            } else {
                position.x_offset as f32
            },
            y_offset: if vertical {
                position.x_offset as f32
            } else {
                position.y_offset as f32
            },
            safe_to_break: !info.unsafe_to_break(),
        })
        .collect();
    let advance = glyphs.iter().map(|glyph| glyph.x_advance).sum();
    ShapedText { glyphs, advance }
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_a_post_table_of_version_2_that_ends_before_its_number_of_glyphs_unsound() {
        use super::post_sound;
        let post = |version: u32, length: usize| {
            let mut table = version.to_be_bytes().to_vec();
            table.resize(length, 0);
            table
        };
        assert!(!post_sound(&post(0x0002_0000, 32)));
        assert!(!post_sound(&post(0x0002_0000, 33)));
        // With the number of glyphs, the names may be the standard ones and the table end there.
        assert!(post_sound(&post(0x0002_0000, 34)));
        // Other versions have no number of glyphs, and shorter tables are not read at all.
        assert!(post_sound(&post(0x0003_0000, 32)));
        assert!(post_sound(&post(0x0001_0000, 32)));
        assert!(post_sound(&post(0x0002_0000, 31)));
    }

    #[test]
    fn tells_cursive_scripts_from_others_and_from_characters_without_one() {
        use super::cursive_script;
        // Arabic letters, and the tatweel, comma and digits of the Arabic block.
        for character in ['ب', '\u{640}', '\u{60C}', '١', '۴', 'ﻻ'] {
            assert_eq!(cursive_script(character), Some(true), "{character}");
        }
        // Syriac and Mongolian join too; Hebrew and Latin do not.
        assert_eq!(cursive_script('ܐ'), Some(true));
        assert_eq!(cursive_script('ᠠ'), Some(true));
        assert_eq!(cursive_script('א'), Some(false));
        assert_eq!(cursive_script('a'), Some(false));
        // No script of their own.
        for character in [' ', '1', '.', '(', '\u{301}'] {
            assert_eq!(cursive_script(character), None, "{character:?}");
        }
    }

    #[test]
    fn keeps_context_only_where_letters_join() {
        use super::joining_context;
        // Five characters next to the text, where they and the text have letters that join.
        assert_eq!(joining_context("ع", ("abcdefع", "عxyzuvw")), ("cdefع", "عxyzu"));
        // Context without such letters, or for text without them, is left out.
        assert_eq!(joining_context("ع", ("abc ", "ع")), ("", "ع"));
        assert_eq!(joining_context("abc", ("ع", "ع")), ("", ""));
        // A zero width joiner joins.
        assert_eq!(joining_context("\u{200D}", ("ع", "")), ("ع", ""));
    }

    #[test]
    fn finds_letters_that_join_past_long_latin_text() {
        use super::has_joining;
        let latin = "Lorem ipsum dolor sit amet, ünd größer — “quoted” ".repeat(10);
        assert!(!has_joining(&latin));
        assert!(!has_joining(""));
        // Letters that join anywhere in the text, past the first chunks of bytes looked at.
        assert!(has_joining(&format!("{latin}ع")));
        assert!(has_joining(&format!("{latin}\u{200D}{latin}")));
        assert!(has_joining(&format!("ܐ{latin}")));
        // Characters from U+0600 on that do not join: Hebrew, CJK, emoji.
        assert!(!has_joining(&format!("{latin}שלום 日本語 😀")));
    }

    #[test]
    fn shapes_joining_forms_from_the_context() {
        let mut db = FontDb::new();
        let Ok(faces) = db.add_font_file(Path::new("/System/Library/Fonts/SFArabic.ttf")) else {
            return;
        };
        let face = faces[0];
        let shape = |context| {
            db.shape_in_context(face, "ع", &[], &[], (Flow::RightToLeft, None), context)
                .glyphs[0]
                .id
        };
        let alone = shape(("", ""));
        let medial = shape(("ع", "ع"));
        assert_ne!(alone, medial);
        assert_eq!(
            db.shape_directed(face, "ععع", &[], &[], true).glyphs[1].id,
            medial
        );
        // Latin context changes nothing.
        assert_eq!(shape(("a", "b")), alone);
    }

    use super::*;

    #[test]
    fn refuses_woff_tables_that_unpack_to_too_much() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/webfonts/Inter-Bold-Latin.woff");
        let woff = std::fs::read(path).expect("web font");
        assert!(unpack(woff.clone()).is_ok());
        // The header declares the font's real size; one table claims a gigabyte.
        let mut bomb = woff;
        bomb[44 + 12..44 + 16].copy_from_slice(&(1u32 << 30).to_be_bytes());
        assert!(matches!(unpack(bomb.clone()), Err(FontError::InvalidFont)));
        // A table directory cut short.
        bomb.truncate(50);
        assert!(matches!(unpack(bomb), Err(FontError::InvalidFont)));
    }

    #[test]
    fn shapes_with_features() {
        let Some(db) = test_db() else { return };
        let face = db
            .faces()
            .position(|face| face.family == "Inter")
            .map(|index| FaceId(index as u32));
        let Some(face) = face else { return };
        // Inter's figures are proportional by default; `tnum` makes them all as wide.
        let widths =
            |features: &Features| ["1", "8"].map(|digit| db.shape_with(face, digit, features).advance);
        let proportional = widths(&[]);
        let tabular = widths(&[(*b"tnum", 1)]);
        assert!(proportional[0] < proportional[1], "{proportional:?}");
        assert!((tabular[0] - tabular[1]).abs() < f32::EPSILON, "{tabular:?}");
        assert!(Arc::ptr_eq(
            &db.shape_with(face, "1", &[(*b"tnum", 1)]),
            &db.shape_with(face, "1", &[(*b"tnum", 1)])
        ));
    }

    #[test]
    fn shapes_top_to_bottom() {
        let Some(db) = test_db() else { return };
        let Some(face) = db
            .faces()
            .position(|face| face.family == "Inter")
            .map(|index| FaceId(index as u32))
        else {
            return;
        };
        let across = db.shape_variable(face, "AB", &[], &[]);
        let down = db.shape_vertical(face, "AB", &[], &[]);
        // The same glyphs, advancing down the line and hung from the middle of their tops.
        let ids = |shaped: &ShapedText| shaped.glyphs.iter().map(|glyph| glyph.id).collect::<Vec<_>>();
        assert_eq!(ids(&across), ids(&down));
        assert!(down.glyphs.iter().all(|glyph| glyph.x_advance > 0.0), "{down:?}");
        assert!(down.glyphs.iter().all(|glyph| glyph.x_offset > 0.0), "{down:?}");
        let first = (&across.glyphs[0], &down.glyphs[0]);
        assert!(
            (first.1.y_offset + first.0.x_advance / 2.0).abs() < 1.0,
            "{first:?}"
        );
        assert!(!db.face(face).vertical_metrics);
    }

    #[test]
    fn keeps_the_shaping_cache_bounded() {
        let mut cache = ShapeCache::default();
        let shaped = Arc::new(ShapedText {
            glyphs: Vec::new(),
            advance: 0.0,
        });
        let key = |index: usize| {
            (
                0,
                Box::<str>::from(index.to_string()),
                Box::<Features>::from([]),
                Box::<[([u8; 4], u32)]>::from([]),
                Direction::LeftToRight,
                None,
            )
        };
        cache.insert(key(0), Arc::clone(&shaped));
        for index in 1..ShapeCache::GENERATION * 3 {
            cache.insert(key(index), Arc::clone(&shaped));
            // Text in use stays.
            if index % 1000 == 0 {
                assert!(cache.get(&key(0)).is_some());
            }
        }
        assert!(cache.newer.len() + cache.older.len() <= ShapeCache::GENERATION * 2);
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1)).is_none(), "unused text goes");
    }

    /// The built-in Inter with a `cmap` of its own that maps only `characters` (to Inter's
    /// glyphs for them), as a font file.
    fn inter_with_only(characters: &[char]) -> Vec<u8> {
        let data = DEFAULT_FONTS[0];
        let font =
            Font::new(Arc::new(data.to_vec()) as Arc<dyn AsRef<[u8]> + Send + Sync>, 0).expect("Inter");
        let mut pairs: Vec<(u32, u32)> = characters
            .iter()
            .filter_map(|&c| Some((u32::from(c), font.charmap().map_unicode(c)?.to_u32())))
            .collect();
        pairs.sort_unstable();
        // A format 12 subtable for Windows Unicode (full repertoire), one group per character.
        let length = 16 + 12 * pairs.len();
        let mut cmap = vec![0, 0, 0, 1, 0, 3, 0, 10, 0, 0, 0, 12, 0, 12, 0, 0];
        cmap.extend_from_slice(&u32::try_from(length).unwrap().to_be_bytes());
        cmap.extend_from_slice(&[0, 0, 0, 0]);
        cmap.extend_from_slice(&u32::try_from(pairs.len()).unwrap().to_be_bytes());
        for (code, glyph) in pairs {
            for value in [code, code, glyph] {
                cmap.extend_from_slice(&value.to_be_bytes());
            }
        }
        // The same tables, with the new `cmap` in place of the old.
        let count = usize::from(u16::from_be_bytes([data[4], data[5]]));
        let mut tables: Vec<([u8; 4], Vec<u8>)> = (0..count)
            .map(|index| {
                let record = &data[12 + 16 * index..28 + 16 * index];
                let read = |at: usize| u32::from_be_bytes(record[at..at + 4].try_into().unwrap()) as usize;
                let tag: [u8; 4] = record[..4].try_into().unwrap();
                let table = if &tag == b"cmap" {
                    cmap.clone()
                } else {
                    data[read(8)..read(8) + read(12)].to_vec()
                };
                (tag, table)
            })
            .collect();
        tables.sort_by_key(|(tag, _)| *tag);
        let mut out = data[..12].to_vec();
        let mut offset = 12 + 16 * count;
        let mut bodies = Vec::new();
        for (tag, table) in &tables {
            out.extend_from_slice(tag);
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&u32::try_from(offset).unwrap().to_be_bytes());
            out.extend_from_slice(&u32::try_from(table.len()).unwrap().to_be_bytes());
            bodies.extend_from_slice(table);
            while bodies.len() % 4 != 0 {
                bodies.push(0);
            }
            offset = 12 + 16 * count + bodies.len();
        }
        out.extend_from_slice(&bodies);
        out
    }

    #[test]
    fn takes_the_hyphen_minus_for_hyphens_a_face_lacks() {
        let mut db = FontDb::new();
        let face = db.add_font_data(inter_with_only(&['-', 'a'])).expect("font")[0];
        let minus = db.face(face).glyph('-').expect("hyphen-minus");
        // As Core Text synthesizes them on macOS, where Chrome takes them from the face.
        for hyphen in ['\u{2010}', '\u{2011}'] {
            assert!(db.face(face).has_glyph(hyphen));
            let shaped = db.shape(face, &format!("a{hyphen}a"));
            assert_eq!(shaped.glyphs[1].id, minus);
            assert_eq!(
                shaped.glyphs[2].cluster, 4,
                "clusters stay at the characters' bytes"
            );
            assert!((shaped.advance - db.shape(face, "a-a").advance).abs() < f32::EPSILON);
        }
        assert!(!db.face(face).has_glyph('\u{2012}'));
    }

    #[test]
    fn draws_clusters_whose_marks_compose_into_characters_it_has() {
        let mut db = FontDb::new();
        let latin = db
            .add_font_data(inter_with_only(&['e', 's', 'é', 'ê', 'ễ', '\u{302}']))
            .expect("font")[0];
        let face = db.face(latin);
        // A combining acute composes with `e` into `é`; `ś` is missing, and so is the acute.
        assert!(face.has_cluster("e\u{301}"));
        assert!(!face.has_cluster("s\u{301}"));
        // Marks compose one after the other (`e` circumflex tilde into `ễ`), or stand alone.
        assert!(face.has_cluster("e\u{302}\u{303}"));
        assert!(face.has_cluster("s\u{302}"));
        // Variation selectors need no glyph, nor a combining grapheme joiner, which shaping
        // hides.
        assert!(face.has_cluster("e\u{FE0E}"));
        assert!(face.has_cluster("e\u{34F}"));
    }

    #[test]
    fn hides_default_ignorables_but_hangul_fillers() {
        for character in [
            '\u{34F}',
            '\u{180E}',
            '\u{200D}',
            '\u{2061}',
            '\u{FEFF}',
            '\u{E0041}',
        ] {
            assert!(is_ignorable(character), "{character:?}");
        }
        for character in ['\u{115F}', '\u{3164}', 'a', ' '] {
            assert!(!is_ignorable(character), "{character:?}");
        }
    }

    #[test]
    fn draws_line_and_paragraph_separators_as_spaces() {
        let mut db = FontDb::new();
        let latin = db.add_font_data(inter_with_only(&['a', ' '])).expect("font")[0];
        assert!(db.face(latin).has_glyph('\u{2028}'));
        let space = db.shape(latin, " ");
        let separators = db.shape(latin, "\u{2028}\u{2029}");
        assert_eq!(separators.glyphs[0].id, space.glyphs[0].id);
        assert_eq!(separators.glyphs[1].cluster, 3);
        assert!((separators.advance - 2.0 * space.advance).abs() < f32::EPSILON);
    }

    #[test]
    fn draws_space_separators_it_lacks_with_its_space() {
        // As HarfBuzz shapes them: an em space as wide as an em, a thin space as a fifth, so no
        // other font is looked for.
        let mut db = FontDb::new();
        let latin = db.add_font_data(inter_with_only(&['a', ' '])).expect("font")[0];
        for character in ['\u{2003}', '\u{2009}', '\u{202F}', '\u{205F}'] {
            assert!(db.face(latin).has_glyph(character), "{character:?}");
        }
        // The Ogham space mark is visible, the ideographic space Chrome takes from a CJK font.
        assert!(!db.face(latin).has_glyph('\u{1680}') && !db.face(latin).has_glyph('\u{3000}'));
        let em = db.shape(latin, "\u{2003}");
        let thin = db.shape(latin, "\u{2009}");
        // In font units: Inter's em is 2048 units, a fifth of it rounded.
        assert!(
            (em.advance - 2048.0).abs() < 0.01 && (thin.advance - 410.0).abs() < 0.01,
            "{em:?} {thin:?}"
        );
        let without = db.add_font_data(inter_with_only(&['a'])).expect("font")[0];
        assert!(!db.face(without).has_glyph('\u{2003}'));
    }

    fn test_db() -> Option<FontDb> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts");
        let mut db = FontDb::new();
        db.add_font_dir(&directory).ok()?;
        (!db.is_empty()).then_some(db)
    }

    /// A link to a parent directory (or to a file already read) adds no faces again.
    #[cfg(unix)]
    #[test]
    fn reads_linked_font_directories_once() {
        let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts");
        let directory = std::env::temp_dir().join(format!("fagus-font-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("sub")).unwrap();
        std::fs::copy(
            fonts.join("Inter-Regular.ttf"),
            directory.join("Inter-Regular.ttf"),
        )
        .unwrap();
        std::os::unix::fs::symlink("..", directory.join("sub/up")).unwrap();
        std::os::unix::fs::symlink("../Inter-Regular.ttf", directory.join("sub/again.ttf")).unwrap();
        let mut db = FontDb::new();
        let count = db.add_font_dir(&directory);
        std::fs::remove_dir_all(&directory).unwrap();
        assert_eq!(count.unwrap(), 1);
        assert_eq!(db.len(), 1);
    }

    #[test]
    fn covers_characters_by_unicode_range() {
        let mut db = FontDb::new();
        let mut add = |file: &str, unicode_range: &[(u32, u32)]| {
            let rule = Descriptors {
                family: "Ranged".to_owned(),
                weight: None,
                style: FontStyle::Normal,
                stretch: None,
                unicode_range: unicode_range.to_vec(),
                ..Descriptors::default()
            };
            db.add_document_face(Arc::new(web_face(file, &rule)))
        };
        // Everything but digits from the first rule, digits from the second; the third rule
        // claims capitals, but lacks Cyrillic ones.
        let letters = add("Inter-Latin.woff2", &[(0, 0x2f), (0x3a, 0x0010_ffff)]);
        let digits = add("Inter-Bold-Latin.woff", &[(0x30, 0x39)]);
        let capitals = add("Inter-Variable-Latin.woff", &[(0x41, 0x5a), (0x410, 0x42f)]);
        let request = (400.0, FontStyle::Normal, 100.0);
        let preferred = db.query(&["Ranged"], 400.0, FontStyle::Normal);
        // The first available font covers a space.
        assert_eq!(preferred, Some(letters));
        let face = |character: char| db.fallback(&["Ranged"], letters, character, request);
        assert_eq!(face('a'), Some(letters));
        assert_eq!(face('7'), Some(digits));
        // A later rule comes first for its characters.
        assert_eq!(face('R'), Some(capitals));
        assert!(!db.face(digits).covers('a') && !db.face(digits).has_glyph('a'));
        // A character in a rule's range that its font lacks goes to the rules before it, then
        // to other families.
        assert_ne!(face('Ж'), Some(capitals));
    }

    #[test]
    fn falls_back_to_the_families_of_the_text_first() {
        let mut db = FontDb::new();
        let mut add = |family: &str, file: &str, unicode_range: Vec<(u32, u32)>| {
            let rule = Descriptors {
                family: family.to_owned(),
                weight: None,
                style: FontStyle::Normal,
                stretch: None,
                unicode_range,
                ..Descriptors::default()
            };
            db.add_document_face(Arc::new(web_face(file, &rule)))
        };
        // The first family lacks an x; a family the text does not name comes before the second
        // one it names.
        let first = add("First", "Inter-Latin.woff2", vec![(0, 0x77), (0x79, 0x0010_ffff)]);
        add("Unnamed", "Inter-Variable-Latin.woff", Vec::new());
        let second = add("Second", "Inter-Bold-Latin.woff", Vec::new());
        let request = (400.0, FontStyle::Normal, 100.0);
        assert_eq!(
            db.fallback(&["First", "Second"], first, 'x', request),
            Some(second)
        );
        assert_eq!(
            db.fallback(&["First", "Second"], first, 'y', request),
            Some(first)
        );
    }

    #[test]
    fn lists_the_positioning_features_of_a_face() {
        let mut db = FontDb::new();
        db.add_default_fonts();
        let face = db.face(db.query(&["Inter"], 400.0, FontStyle::Normal).expect("Inter"));
        assert!(face.positions(*b"kern"));
        assert!(!face.positions(*b"halt") && !face.substitutes(*b"kern"));
    }

    #[test]
    fn falls_back_to_the_built_in_fonts() {
        let mut db = FontDb::new();
        db.add_default_fonts();
        // Without other faces, every family, generic or not, gets the built-in Inter.
        for family in ["Inter", "Helvetica", "serif", "monospace"] {
            let face = db.face(db.query(&[family], 700.0, FontStyle::Normal).expect(family));
            assert!(face.builtin && face.family == "Inter", "{family}");
            assert_eq!(face.weight, 700.0, "{family}");
        }
        // Another family comes first for unknown and generic families; the built-in faces still
        // have the characters it lacks.
        let latin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/webfonts/Inter-Latin.woff2");
        let data = Arc::new(unpack(std::fs::read(latin).expect("web font")).expect("WOFF2"));
        let rule = Descriptors {
            family: "Brand".to_owned(),
            weight: Some((400.0, 400.0)),
            style: FontStyle::Normal,
            stretch: None,
            unicode_range: Vec::new(),
            ..Descriptors::default()
        };
        let face = Face::from_rule(data, 0, &rule).expect("face");
        let brand = db.add_document_face(Arc::new(face));
        assert_eq!(db.query(&["Helvetica"], 400.0, FontStyle::Normal), Some(brand));
        assert_eq!(db.query(&["monospace"], 400.0, FontStyle::Normal), Some(brand));
        let cyrillic = db
            .fallback(&["Brand"], brand, 'Ж', (400.0, FontStyle::Normal, 100.0))
            .expect("fallback");
        assert!(db.face(cyrillic).builtin);
        // An installed Inter hides the built-in one.
        let Some(installed) = test_db() else { return };
        db = installed;
        db.add_default_fonts();
        let inter = db.query(&["Inter"], 700.0, FontStyle::Normal).expect("Inter");
        assert!(!db.face(inter).builtin);
    }

    /// A face of a web font in `tests/webfonts` with an `@font-face` rule.
    fn web_face(file: &str, rule: &Descriptors) -> Face {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/webfonts")
            .join(file);
        let data = Arc::new(unpack(std::fs::read(path).expect("web font")).expect("unpacked"));
        Face::from_rule(data, 0, rule).expect("face")
    }

    #[test]
    fn matches_widths_like_css() {
        let mut db = FontDb::new();
        let mut face = |stretch: (f32, f32)| {
            let rule = Descriptors {
                family: "Wide".to_owned(),
                weight: None,
                style: FontStyle::Normal,
                stretch: Some(stretch),
                unicode_range: Vec::new(),
                ..Descriptors::default()
            };
            db.add_document_face(Arc::new(web_face("Inter-Latin.woff2", &rule)))
        };
        let (condensed, normal, expanded) = (face((75.0, 75.0)), face((100.0, 100.0)), face((125.0, 150.0)));
        let query = |stretch: f32| db.query_stretch(&["Wide"], 400.0, FontStyle::Normal, stretch);
        assert_eq!(query(75.0), Some(condensed));
        assert_eq!(query(100.0), Some(normal));
        assert_eq!(query(140.0), Some(expanded), "within a range");
        // Up to normal width narrower faces come first, closest first, then wider ones; above
        // normal the other way round.
        assert_eq!(query(87.5), Some(condensed));
        assert_eq!(query(50.0), Some(condensed));
        assert_eq!(query(112.5), Some(expanded));
        assert_eq!(query(200.0), Some(expanded));
    }

    #[test]
    fn sets_variable_faces_at_their_coordinates() {
        let rule = |weight: Option<(f32, f32)>| Descriptors {
            family: "Variable".to_owned(),
            weight,
            style: FontStyle::Normal,
            stretch: None,
            unicode_range: Vec::new(),
            ..Descriptors::default()
        };
        let mut db = FontDb::new();
        let face = db.add_document_face(Arc::new(web_face("Inter-Variable-Latin.woff", &rule(None))));
        let ranged = db.add_document_face(Arc::new(web_face(
            "Inter-Variable-Latin.woff",
            &rule(Some((300.0, 500.0))),
        )));
        let tags = |variations: Vec<([u8; 4], f32)>| -> Vec<(String, f32)> {
            variations
                .into_iter()
                .map(|(tag, value)| (String::from_utf8_lossy(&tag).into_owned(), value))
                .collect()
        };
        // The weight on `wght`, the size on `opsz`, both within the axes.
        assert_eq!(
            tags(db.variations(face, (700.0, FontStyle::Normal, 100.0), Some(24.0), &[])),
            [("opsz".to_owned(), 24.0), ("wght".to_owned(), 700.0)]
        );
        assert_eq!(
            tags(db.variations(face, (1000.0, FontStyle::Normal, 100.0), Some(8.0), &[])),
            [("opsz".to_owned(), 14.0), ("wght".to_owned(), 900.0)]
        );
        // Without optical sizing the axis stays at its default; settings come last.
        assert_eq!(
            tags(db.variations(
                face,
                (400.0, FontStyle::Normal, 100.0),
                None,
                &[(*b"wght", 650.0)]
            )),
            [("wght".to_owned(), 650.0)]
        );
        // A rule with weights keeps the weight within them; without one the axis spans them all.
        assert_eq!(db.face(ranged).weight_range, (300.0, 500.0));
        assert_eq!(db.face(face).weight_range, (400.0, 400.0));
        assert_eq!(
            tags(db.variations(ranged, (800.0, FontStyle::Normal, 100.0), None, &[])),
            [("wght".to_owned(), 500.0)]
        );
        // Heavier instances have wider glyphs.
        let width = |weight: f32| {
            let variations = db.variations(face, (weight, FontStyle::Normal, 100.0), Some(14.0), &[]);
            db.shape_variable(face, "Hamburg", &[], &variations).advance
        };
        assert!(width(900.0) > width(400.0) && width(400.0) > width(100.0));
        assert!(
            (width(400.0) - db.shape(face, "Hamburg").advance).abs() < 0.5,
            "the default instance"
        );
    }

    #[test]
    fn scales_faces_by_size_adjust_and_overrides_their_metrics() {
        let plain = web_face("Inter-Latin.woff2", &Descriptors::default());
        let adjusted = web_face(
            "Inter-Latin.woff2",
            &Descriptors {
                size_adjust: 1.5,
                ascent_override: Some(1.2),
                descent_override: Some(0.4),
                line_gap_override: Some(0.3),
                ..Descriptors::default()
            },
        );
        // An em of the adjusted face is 1.5 times the font size: glyphs, advances and metrics.
        assert!((adjusted.scale(20.0) - 1.5 * plain.scale(20.0)).abs() < 1e-6);
        let x = plain.glyph('x').expect("an x");
        let width = |face: &Face| {
            let [left, _, right, _] = face.glyph_bounds(x, 20.0).expect("an outline");
            right - left
        };
        assert!((width(&adjusted) - 1.5 * width(&plain)).abs() < 0.01);
        // The overrides are fractions of the adjusted size: 1.2 × 30 and 0.4 × 30 pixels.
        assert_eq!(adjusted.rounded_ascent_descent(20.0), (36.0, 12.0));
        assert!((adjusted.metrics.line_gap * adjusted.scale(20.0) - 9.0).abs() < 1e-3);
    }

    #[test]
    fn keeps_the_boxes_of_glyphs_it_measured() {
        let plain = web_face("Inter-Latin.woff2", &Descriptors::default());
        let x = plain.glyph('x').expect("an x");
        let first = plain.glyph_bounds(x, 20.0);
        assert!(first.is_some());
        assert_eq!(plain.glyph_bounds(x, 20.0), first);
        assert_eq!(plain.measure_glyph(x, 20.0), first);
        assert_ne!(plain.glyph_bounds(x, 30.0), first);
        // A face with `size-adjust` draws its glyphs larger, measured or not before the rule's
        // descriptors applied.
        let adjusted = web_face(
            "Inter-Latin.woff2",
            &Descriptors {
                size_adjust: 2.0,
                ..Descriptors::default()
            },
        );
        assert_eq!(adjusted.glyph_bounds(x, 10.0), first);
        let em = adjusted.metrics.units_per_em;
        assert_eq!(adjusted.glyph_bounds(x, em), adjusted.measure_glyph(x, em));
    }

    #[test]
    fn measures_the_x_height_at_the_top_of_the_x() {
        let face = web_face("Inter-Latin.woff2", &Descriptors::default());
        let x = face.glyph('x').expect("an x");
        let [_, top, _, _] = face
            .glyph_bounds(x, face.metrics.units_per_em)
            .expect("an outline");
        assert!((face.metrics.x_height + top).abs() < 1e-3);
    }

    #[test]
    fn fits_heights_from_glyphs_to_the_pixel_grid_as_freetype_does_for_chrome_on_linux() {
        // DejaVu Sans, whose OS/2 table (version 1) has no x-height or cap height: an "x" 1120
        // units high, an "H" 1493, round small letters 1147, 2048 units per em. The x-heights and
        // cap heights Chrome 157 on Linux takes (measured with `ex` and `cap`): not hinted below
        // 8 pixels, in 1/64 pixels; whole pixels from 8 on, 3/8 of a pixel of overshoot rounded up.
        let x = |pixels| grid_fitted(1120, Some(1147), pixels, 2048);
        assert_eq!(x(7.5), 4.109_375);
        assert_eq!(
            [x(8.0), x(9.5), x(13.0), x(13.25), x(14.0), x(15.0)],
            [5.0, 5.0, 7.0, 8.0, 8.0, 9.0]
        );
        let cap = |pixels| grid_fitted(1493, Some(1147), pixels, 2048);
        assert_eq!([cap(14.0), cap(16.0)], [10.0, 12.0]);
        // DejaVu Serif ("x" 1063, round letters 1092): the stretched scale makes it alternate.
        let serif = |pixels| grid_fitted(1063, Some(1092), pixels, 2048);
        assert_eq!(
            [serif(34.25), serif(34.5), serif(35.75), serif(36.0), serif(36.25)],
            [18.0, 19.0, 19.0, 18.0, 19.0]
        );
    }

    #[test]
    fn synthesizes_bold_and_italics_as_chrome_decides() {
        let rule = |weight: Option<(f32, f32)>, style: FontStyle| Descriptors {
            weight,
            style,
            style_auto: false,
            ..Descriptors::default()
        };
        // A regular face is made bold from 600 on, but a bold font is not made bolder.
        let regular = web_face(
            "Inter-Latin.woff2",
            &rule(Some((400.0, 400.0)), FontStyle::Normal),
        );
        assert!(regular.synthetic_bold(600.0) && !regular.synthetic_bold(500.0));
        let bold_file = web_face(
            "Inter-Bold-Latin.woff",
            &rule(Some((400.0, 400.0)), FontStyle::Normal),
        );
        assert!(!bold_file.synthetic_bold(700.0));
        // A variable face of `font-weight: auto` takes heavy weights from its axis; with a weight
        // of its own it is made bold.
        let auto = web_face("Inter-Variable-Latin.woff", &rule(None, FontStyle::Normal));
        assert!(!auto.synthetic_bold(800.0));
        let fixed = web_face(
            "Inter-Variable-Latin.woff",
            &rule(Some((400.0, 400.0)), FontStyle::Normal),
        );
        assert!(fixed.synthetic_bold(800.0));
        // Italics and `oblique` from 14 degrees slant an upright face; shallower or backward
        // slants do not, nor does any style an italic face.
        assert!(regular.synthetic_italic(FontStyle::Italic));
        assert!(regular.synthetic_italic(FontStyle::OBLIQUE));
        assert!(regular.synthetic_italic(FontStyle::oblique(20.0)));
        assert!(!regular.synthetic_italic(FontStyle::oblique(10.0)));
        assert!(!regular.synthetic_italic(FontStyle::oblique(-20.0)));
        let italic = web_face("Inter-Latin.woff2", &rule(None, FontStyle::Italic));
        assert!(!italic.synthetic_italic(FontStyle::Italic));
    }

    #[test]
    fn matches_oblique_angles_like_chrome() {
        let mut db = FontDb::new();
        let mut add = |style: FontStyle, slope: Option<(f32, f32)>| {
            db.add_document_face(Arc::new(web_face(
                "Inter-Latin.woff2",
                &Descriptors {
                    family: "Slanted".to_owned(),
                    style,
                    slope,
                    style_auto: false,
                    ..Descriptors::default()
                },
            )))
        };
        let upright = add(FontStyle::Normal, None);
        let leaning = add(FontStyle::oblique(5.0), Some((5.0, 15.0)));
        let query = |style: FontStyle| db.query(&["Slanted"], 400.0, style);
        assert_eq!(query(FontStyle::Normal), Some(upright));
        assert_eq!(query(FontStyle::Italic), Some(leaning));
        assert_eq!(query(FontStyle::oblique(10.0)), Some(leaning));
        // Up to 14 degrees shallower slants come first, down to upright; backward slants take
        // the upright face before one that leans forward.
        assert_eq!(query(FontStyle::oblique(3.0)), Some(upright));
        assert_eq!(query(FontStyle::oblique(-10.0)), Some(upright));
        assert_eq!(query(FontStyle::oblique(30.0)), Some(leaning));
    }

    #[test]
    fn leaves_faces_that_are_not_loaded_to_the_next_family() {
        // A document's regular and italic faces of one family and another family's face; the
        // italic one is not loaded, so italic text takes the next family, and regular text its
        // own face, which keeps its id.
        let mut db = FontDb::new();
        let mut add = |family: &str, style: FontStyle| {
            db.add_document_face(Arc::new(web_face(
                "Inter-Latin.woff2",
                &Descriptors {
                    family: family.to_owned(),
                    style,
                    style_auto: false,
                    ..Descriptors::default()
                },
            )))
        };
        let regular = add("Book", FontStyle::Normal);
        let italic = add("Book", FontStyle::Italic);
        let other = add("Other", FontStyle::Normal);
        assert!(db.with_unloaded(|_| true).is_none());
        let unloaded = db.with_unloaded(|id| id != italic).expect("one face not loaded");
        let query = |db: &FontDb, style: FontStyle| db.query(&["Book", "Other"], 400.0, style);
        assert_eq!(query(&db, FontStyle::Italic), Some(italic));
        assert_eq!(query(&unloaded, FontStyle::Italic), Some(other));
        assert_eq!(query(&unloaded, FontStyle::Normal), Some(regular));
    }

    #[test]
    fn sets_slant_and_italic_axes() {
        let mut face = web_face("Inter-Variable-Latin.woff", &Descriptors::default());
        face.axes.push(Axis {
            tag: *b"slnt",
            min: -15.0,
            default: 0.0,
            max: 0.0,
        });
        let mut ranged = web_face(
            "Inter-Variable-Latin.woff",
            &Descriptors {
                style: FontStyle::Oblique(0),
                slope: Some((0.0, 10.0)),
                style_auto: false,
                ..Descriptors::default()
            },
        );
        ranged.axes.clone_from(&face.axes);
        let mut db = FontDb::new();
        let auto = db.add_document_face(Arc::new(face));
        let ranged = db.add_document_face(Arc::new(ranged));
        let slant = |face: FaceId, style: FontStyle| {
            db.variations(face, (400.0, style, 100.0), None, &[])
                .into_iter()
                .find(|(tag, _)| tag == b"slnt")
                .map(|(_, value)| value)
        };
        // OpenType leans the other way: italics are -14 on the axis, within the axis; a rule's
        // range of slants limits them, and `font-style: auto` slants without synthetic italics.
        assert_eq!(slant(auto, FontStyle::Italic), Some(-14.0));
        assert_eq!(slant(auto, FontStyle::oblique(40.0)), Some(-15.0));
        assert_eq!(slant(auto, FontStyle::Normal), Some(0.0));
        assert_eq!(slant(ranged, FontStyle::Italic), Some(-10.0));
        assert!(!db.face(auto).synthetic_italic(FontStyle::Italic));
    }

    #[test]
    fn shapes_text_in_its_language() {
        let mut db = FontDb::new();
        let face = db
            .add_font_data(
                std::fs::read(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fonts/Inter-Regular.ttf"
                ))
                .expect("font"),
            )
            .expect("faces")[0];
        let glyph = |lang: Option<&str>| {
            db.shape_in_language(face, "\u{15F}", &[], &[], Flow::LeftToRight, lang)
                .glyphs[0]
                .id
        };
        // Romanian takes the s with a comma below for the s with a cedilla (`locl`).
        assert_ne!(glyph(Some("ro")), glyph(None));
        assert_eq!(glyph(Some("en")), glyph(None));
        assert_eq!(glyph(Some("ro-RO")), glyph(Some("ro")));
    }

    #[test]
    fn shapes_text_without_a_language_in_the_default_one() {
        let mut db = FontDb::new();
        let face = db
            .add_font_data(
                std::fs::read(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fonts/Inter-Regular.ttf"
                ))
                .expect("font"),
            )
            .expect("faces")[0];
        let glyph = |db: &FontDb, lang: Option<&str>| {
            db.shape_in_language(face, "\u{15F}", &[], &[], Flow::LeftToRight, lang)
                .glyphs[0]
                .id
        };
        let plain = glyph(&db, None);
        let romanian = glyph(&db, Some("ro"));
        db.set_default_language(Some("ro"));
        // Text without a language (or with an empty one) takes the default's forms; a language
        // of its own still wins.
        assert_eq!(glyph(&db, None), romanian);
        assert_eq!(glyph(&db, Some("")), romanian);
        assert_eq!(glyph(&db, Some("en")), plain);
        db.set_default_language(None);
        assert_eq!(glyph(&db, None), plain);
    }

    #[test]
    fn matches_weights_and_styles_like_css() {
        let Some(db) = test_db() else { return };
        let regular = db.query(&["Inter"], 400.0, FontStyle::Normal).unwrap();
        let bold = db.query(&["inter"], 700.0, FontStyle::Normal).unwrap();
        assert_eq!(db.face(regular).weight, 400.0);
        assert_eq!(db.face(bold).weight, 700.0);
        // 600 is not available: the next heavier weight wins.
        assert_eq!(
            db.face(db.query(&["Inter"], 600.0, FontStyle::Normal).unwrap())
                .weight,
            700.0
        );
        // 300 is not available: lighter weights are tried first, then heavier ones.
        assert_eq!(
            db.face(db.query(&["Inter"], 300.0, FontStyle::Normal).unwrap())
                .weight,
            400.0
        );
        // Unknown families fall back to the default.
        assert!(
            db.query(&["Nonexistent", "sans-serif"], 400.0, FontStyle::Normal)
                .is_some()
        );
    }

    #[test]
    fn adds_to_the_ascent_of_times_helvetica_and_courier() {
        // Helvetica at 12px: 9 above the baseline and 3 below, 2 more above (15 % of 12).
        assert_eq!(ascent_adjustment("Helvetica", 9.0, 3.0), 2.0);
        // At 23px: 18 and 5, 3.45 rounded to 3.
        assert_eq!(ascent_adjustment("Times", 18.0, 5.0), 3.0);
        assert_eq!(ascent_adjustment("Courier", 12.0, 3.0), 2.0);
        assert_eq!(ascent_adjustment("Helvetica Neue", 9.0, 3.0), 0.0);
        assert_eq!(ascent_adjustment("Arial", 9.0, 3.0), 0.0);
    }

    #[test]
    fn shapes_with_kerning_and_caches() {
        let Some(db) = test_db() else { return };
        let face = db.query(&["Inter"], 400.0, FontStyle::Normal).unwrap();
        let shaped = db.shape(face, "AVATAR");
        assert_eq!(shaped.glyphs.len(), 6);
        let unkerned: f32 = "AVATAR"
            .chars()
            .map(|character| db.shape(face, &character.to_string()).advance)
            .sum();
        assert!(
            shaped.advance < unkerned,
            "kerning makes AVATAR narrower than its letters"
        );
        assert!(Arc::ptr_eq(&shaped, &db.shape(face, "AVATAR")));
    }

    #[test]
    fn reads_metrics() {
        let Some(db) = test_db() else { return };
        let face = db.face(db.query(&["Inter"], 400.0, FontStyle::Normal).unwrap());
        assert!(face.metrics.ascender > 0.0 && face.metrics.descender > 0.0);
        assert!(face.metrics.x_height < face.metrics.cap_height);
        assert!(face.glyph('€').is_some());
    }

    /// A database with the built-in fonts and, as the system's fonts, the fonts of `tests/fonts`
    /// and `tests/webfonts` under the families given: `(family, file)`.
    fn system_db(faces: &[(&str, &str)]) -> FontDb {
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
        let mut index = fontdb::Database::new();
        let mut names = FxHashMap::default();
        for (family, file) in faces {
            let path = [tests.join("fonts").join(file), tests.join("webfonts").join(file)]
                .into_iter()
                .find(|path| path.exists())
                .expect("test font");
            let data = unpack(std::fs::read(path).expect("font")).expect("unpacked");
            for id in index.load_font_source(fontdb::Source::Binary(Arc::new(data))) {
                names.insert(id, (*family).to_owned());
            }
        }
        let system = SystemFonts::from_index(index, |info| names.get(&info.id).cloned());
        let mut db = FontDb::new();
        db.add_default_fonts();
        db.set_system_fonts(system);
        db
    }

    fn family(db: &FontDb, id: Option<FaceId>) -> &str {
        &db.face(id.expect("a face")).family
    }

    /// Face `index` of macOS' `PingFangUI.ttc`, whose glyphs are in an `hvgl` table; `None`
    /// on systems without it.
    fn pingfang(index: u32) -> Option<Face> {
        let path =
            "/System/Library/PrivateFrameworks/FontServices.framework/Resources/Reserved/PingFangUI.ttc";
        let data = Arc::new(std::fs::read(path).ok()?);
        let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(&data) as _;
        Some(make_face(Font::new(blob, index)?, data, index))
    }

    #[test]
    fn draws_pingfang_from_its_hvgl_table() {
        // Face 20 is PingFang SC.
        let Some(face) = pingfang(20) else { return };
        assert_eq!(face.family, "PingFang SC");
        assert_eq!(face.outlines, Outlines::Hvgl);
        let glyph = face.glyph('中').expect("中");
        // At the default instance (Medium), Core Text draws 中 from (96.0, -109.6) to
        // (904.6, 824.2).
        let [left, top, right, bottom] = face.glyph_bounds(glyph, 1000.0).expect("an outline");
        assert!(
            (left - 96.0).abs() < 1.0 && (right - 904.6).abs() < 1.0,
            "{left} {right}"
        );
        assert!(
            (top + 824.2).abs() < 1.0 && (bottom - 109.6).abs() < 1.0,
            "{top} {bottom}"
        );
        // The middle stroke, alone below the baseline, is wider the heavier the instance.
        let stroke = |weight: f32| {
            let (left, right) = face
                .intercept(glyph, 1000.0, &[(*b"wght", weight)], (40.0, 60.0))
                .expect("the stroke");
            right - left
        };
        let (thin, regular, bold) = (stroke(100.0), stroke(400.0), stroke(600.0));
        assert!(thin < regular && regular < bold, "{thin} {regular} {bold}");
        // Core Text's PingFangSC-Regular: from 461.641 to 537.331.
        assert!((regular - 75.69).abs() < 0.01, "{regular}");
    }

    #[test]
    fn finds_installed_faces_by_name_once() {
        let db = system_db(&[("Text", "Inter-Regular.ttf"), ("Text", "Inter-Bold.ttf")]);
        let system = db.system.clone().expect("system fonts");
        let weight = |name: &str| {
            db.local(name).map(|(data, index)| {
                let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(&data) as _;
                Font::new(blob, index).map(|font| make_face(font, Arc::clone(&data), index).weight)
            })
        };
        // By PostScript name and by full name, in any case; again from what was found.
        for _ in 0..2 {
            assert_eq!(weight("Inter-Bold"), Some(Some(700.0)));
            assert_eq!(weight("inter bold"), Some(Some(700.0)));
            assert_eq!(weight("INTER-REGULAR"), Some(Some(400.0)));
            assert_eq!(weight("Inter Black"), None);
        }
        let known = system.names_looked_for();
        assert_eq!(known.len(), 4);
        assert_eq!(known.get("inter black"), Some(&None));
    }

    #[test]
    fn passes_over_installed_faces_without_glyphs_to_draw() {
        // Inter without its `glyf` table (renamed to `glyg`, which keeps the tables sorted).
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts");
        let inter = std::fs::read(tests.join("Inter-Regular.ttf")).expect("Inter");
        let mut bare = inter.clone();
        let tables = usize::from(u16::from_be_bytes([bare[4], bare[5]]));
        let record = (0..tables)
            .map(|table| 12 + 16 * table)
            .find(|record| &bare[*record..record + 4] == b"glyf")
            .expect("a glyf table");
        bare[record..record + 4].copy_from_slice(b"glyg");
        let mut index = fontdb::Database::new();
        let mut names = FxHashMap::default();
        for (family, data) in [("Bare", bare), ("Drawn", inter)] {
            for id in index.load_font_source(fontdb::Source::Binary(Arc::new(data))) {
                names.insert(id, family.to_owned());
            }
        }
        let system = SystemFonts::from_index(index, |info| names.get(&info.id).cloned());
        let mut db = FontDb::new();
        db.set_system_fonts(system);
        let system = db.system.clone().expect("system fonts");
        // Fallback takes the next face with the character, a query the next family.
        assert_eq!(
            system.covering('a').map(|(_, face)| face.family.as_str()),
            Some("Drawn")
        );
        let request = (400.0, FontStyle::Normal, 100.0);
        assert_eq!(
            family(&db, db.query_in(&["Bare", "Drawn"], request, None)),
            "Drawn"
        );
        // Once, until reported.
        assert_eq!(db.take_skipped_faces(), ["Bare"]);
        assert_eq!(db.take_skipped_faces(), Vec::<String>::new());
    }

    #[test]
    fn parses_the_systems_faces_when_used() {
        let db = system_db(&[
            ("Times", "Inter-Regular.ttf"),
            ("Times", "Inter-Bold.ttf"),
            ("Helvetica", "Inter-Regular.ttf"),
        ]);
        let system = db.system.clone().expect("system fonts");
        assert_eq!(system.loaded(), 0, "nothing is parsed before it is used");
        let request = (700.0, FontStyle::Normal, 100.0);
        let bold = db.query_in(&["Times"], request, None).expect("Times");
        assert_eq!(db.face(bold).family, "Times");
        assert_eq!(db.face(bold).weight, 700.0);
        assert_eq!(system.loaded(), 2, "only the family asked for is parsed");
        // Unknown families get the standard font, Times; the built-in Inter stays unused.
        assert_eq!(
            family(&db, db.query(&["Unknown"], 400.0, FontStyle::Normal)),
            "Times"
        );
        assert_eq!(
            family(&db, db.query(&["sans-serif"], 400.0, FontStyle::Normal)),
            "Helvetica"
        );
    }

    #[test]
    fn sets_cjk_text_as_chrome_on_linux() {
        // No fonts of its own for Korean: the default family for the text, and fallback in the
        // Noto Sans CJK of the language, wherever its file lies (stand-ins with a character the
        // built-in Inter lacks).
        let db = system_db(&[
            ("Liberation Sans", "Inter-400-latin.woff2"),
            ("Noto Sans CJK JP", "STIXTwoMath-Regular.woff2"),
            ("Noto Sans CJK KR", "STIXTwoMath-Regular.woff2"),
        ]);
        let request = (400.0, FontStyle::Normal, 100.0);
        let preferred = db.query_in(&["sans-serif"], request, Some("ko")).unwrap();
        assert_eq!(family(&db, Some(preferred)), "Liberation Sans");
        let found = db.fallback_in(&["sans-serif"], preferred, '𝑥', request, Some("ko"));
        assert_eq!(family(&db, found), "Noto Sans CJK KR");
        let found = db.fallback_in(&["sans-serif"], preferred, '𝑥', request, Some("ja"));
        assert_eq!(family(&db, found), "Noto Sans CJK JP");
    }

    #[test]
    fn falls_back_to_the_sans_serif_families_before_the_order_of_the_files() {
        // As fontconfig ranks the fonts for a character Chrome asks it for on Linux: DejaVu Sans
        // before DejaVu Sans Mono, wherever their files lie.
        for mono_first in [false, true] {
            // Stand-ins with a character the built-in Inter lacks.
            let sans = ("DejaVu Sans", "STIXTwoMath-Regular.woff2");
            let mono = ("DejaVu Sans Mono", "STIXTwoMath-Regular.woff2");
            let mut faces = vec![("Liberation Sans", "Inter-400-latin.woff2")];
            faces.extend(if mono_first { [mono, sans] } else { [sans, mono] });
            let db = system_db(&faces);
            let request = (400.0, FontStyle::Normal, 100.0);
            let families = ["Tahoma", "sans-serif"];
            let preferred = db.query_in(&families, request, Some("fa")).unwrap();
            assert_eq!(family(&db, Some(preferred)), "Liberation Sans");
            let found = db.fallback_in(&families, preferred, '𝑥', request, Some("fa"));
            assert_eq!(family(&db, found), "DejaVu Sans", "mono first: {mono_first}");
        }
    }

    #[test]
    fn falls_back_to_regular_faces_as_fontconfig_gives_them_to_chrome() {
        // Chrome on Linux asks fontconfig for a font with the character without the text's
        // weight, takes the regular face it gives and makes it bold: bold Cyrillic text that
        // the Latin family lacks is DejaVu Sans, not DejaVu Sans Bold.
        let mut db = system_db(&[
            ("Liberation Sans", "Inter-400-latin.woff2"),
            ("DejaVu Sans", "Inter-Regular.ttf"),
            ("DejaVu Sans", "Inter-Bold.ttf"),
        ]);
        // Without the built-in Inter, which has the character.
        db.faces.retain(|face| !face.builtin);
        db.overlaid.retain(|_| false);
        db.overlaid.resize(db.faces.len(), false);
        let bold = (700.0, FontStyle::Normal, 100.0);
        let families = ["sans-serif"];
        let preferred = db.query_in(&families, bold, Some("ru")).unwrap();
        assert_eq!(family(&db, Some(preferred)), "Liberation Sans");
        let found = db
            .fallback_in(&families, preferred, 'Ж', bold, Some("ru"))
            .expect("a face");
        assert_eq!(db.face(found).family, "DejaVu Sans");
        assert!(db.face(found).weight < 600.0, "{}", db.face(found).weight);
    }

    #[test]
    fn knows_the_scripts_of_the_languages_chrome_has_fonts_for() {
        assert!(LangScript::Japanese.writes('か') && LangScript::Japanese.writes('漢'));
        assert!(LangScript::Korean.writes('한') && !LangScript::Korean.writes('か'));
        assert!(LangScript::SimplifiedChinese.writes('汉'));
        assert!(LangScript::Devanagari.writes('न') && !LangScript::Devanagari.writes('ش'));
    }

    #[test]
    fn takes_metric_compatible_families_for_missing_ones_as_chrome_on_linux() {
        let db = system_db(&[
            ("Liberation Sans", "Inter-Regular.ttf"),
            ("Liberation Mono", "Inter-Bold.ttf"),
            ("DejaVu Serif", "Inter-Italic.ttf"),
        ]);
        let request = (400.0, FontStyle::Normal, 100.0);
        let named = |names: &[&str]| family(&db, db.query_in(names, request, None));
        assert_eq!(named(&["Arial"]), "Liberation Sans");
        assert_eq!(named(&["Helvetica", "serif"]), "Liberation Sans");
        assert_eq!(named(&["Menlo", "Courier New", "monospace"]), "Liberation Mono");
        assert_eq!(named(&["Courier", "serif"]), "Liberation Mono");
        // Not a family of the same metrics: the next of the list.
        assert_eq!(named(&["Tahoma", "Courier"]), "Liberation Mono");
        assert_eq!(named(&["Times New Roman", "Courier"]), "Liberation Mono");
    }

    #[test]
    fn leaves_the_built_in_inter_unnamed_with_the_systems_fonts() {
        // Chrome goes on to Arial where the system has no Inter, and to the standard font
        // where it has none of the list.
        let db = system_db(&[("Arial", "Inter-Bold.ttf"), ("Times", "Inter-Bold.ttf")]);
        let request = (400.0, FontStyle::Normal, 100.0);
        assert_eq!(
            family(&db, db.query_in(&["Inter", "Arial"], request, None)),
            "Arial"
        );
        let alone = db.query_in(&["Inter"], request, None).expect("a face");
        assert!(!db.face(alone).builtin);
        assert_eq!(db.face(alone).family, "Times");
        // Without the system's fonts, the built-in Inter answers to its name.
        let mut plain = FontDb::new();
        plain.add_default_fonts();
        let inter = plain
            .query(&["Inter", "Arial"], 400.0, FontStyle::Normal)
            .expect("Inter");
        assert!(plain.face(inter).builtin);
    }

    #[test]
    fn measures_advances_as_chrome_on_linux() {
        // Chrome 157 on Linux measures Inter at 11pt (14.67px, which Skia sets as 938/64px):
        // "comment" 64.84375px wide and the hyphen 5.265625px (in layout units, rounded up),
        // where the font's advances times the size give 64.88px and 5.2565px.
        let Some(db) = test_db() else { return };
        let face = db.query(&["Inter"], 400.0, FontStyle::Normal).expect("Inter");
        let width = |text: &str| {
            let shaped = db.shape(face, text);
            let sum: f32 = shaped
                .glyphs
                .iter()
                .map(|glyph| db.face(face).layout_advance(glyph, 11.0 * 96.0 / 72.0))
                .sum();
            (sum * 64.0).ceil() / 64.0
        };
        assert_eq!(width("comment"), 64.843_75);
        assert_eq!(width("\u{2010}"), 5.265_625);
        assert_eq!(
            width("equipment and the decisions behind each picture; the tutors com\u{2010}"),
            29_169.0 / 64.0
        );
        // At 13.33px (853/64px), Inter's space (576 units) is 3.7485352 pixels.
        let shaped = db.shape(face, " ");
        let space = &shaped.glyphs[0];
        assert_eq!(space.own_advance, 576.0);
        assert_eq!(db.face(face).layout_advance(space, 13.33), 245_664.0 / 65536.0);
        // Kerning (here of `T` with `o`) counts at the size cut to 1/65536 pixel, rounded.
        let pair = db.shape(face, "To");
        let kerned = &pair.glyphs[0];
        let kerning = kerned.x_advance - kerned.own_advance;
        assert!(kerning < 0.0, "{pair:?}");
        let scale = (13.33_f64 * 65536.0).trunc();
        let positioned = (f64::from(kerning) * scale / 2048.0).round();
        let unkerned = ShapedGlyph {
            x_advance: kerned.own_advance,
            ..*kerned
        };
        let own = f64::from(db.face(face).layout_advance(&unkerned, 13.33)) * 65536.0;
        let advance = db.face(face).layout_advance(kerned, 13.33);
        assert!((f64::from(advance) - (own + positioned) / 65536.0).abs() < 1e-6);
    }

    #[test]
    fn gives_glyphs_the_shaper_sets_without_advance_none() {
        // HarfBuzz zeroes the advance of a direction mark (a default ignorable character),
        // whatever the advance of its glyph that Skia hands it: none, not the difference between
        // `FreeType`'s scaled advance and the shaper's.
        let Some(db) = test_db() else { return };
        let face = db.query(&["Inter"], 400.0, FontStyle::Normal).expect("Inter");
        let shaped = db.shape(face, "a\u{200F}b");
        let mark = shaped
            .glyphs
            .iter()
            .find(|glyph| glyph.x_advance == 0.0)
            .expect("the mark");
        assert_eq!(db.face(face).layout_advance(mark, 25.0), 0.0);
        assert!(
            shaped
                .glyphs
                .iter()
                .any(|glyph| db.face(face).layout_advance(glyph, 25.0) > 0.0)
        );
    }

    #[test]
    fn prefers_added_fonts_to_the_systems() {
        let mut db = system_db(&[("Inter", "Inter-Bold.ttf"), ("Times", "Inter-Bold.ttf")]);
        // The system's Inter hides the built-in one ...
        let installed = db.query(&["Inter"], 400.0, FontStyle::Normal).expect("Inter");
        assert!(installed.0 & SYSTEM_FACE != 0);
        assert_eq!(db.face(installed).weight, 700.0);
        // ... and an added Inter hides the system's.
        let regular = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts/Inter-Regular.ttf");
        db.add_font_file(&regular).expect("font");
        let added = db.query(&["Inter"], 400.0, FontStyle::Normal).expect("Inter");
        assert!(added.0 & SYSTEM_FACE == 0 && !db.face(added).builtin);
        // So does a family that the document defines.
        let rule = Descriptors {
            family: "Times".to_owned(),
            weight: None,
            style: FontStyle::Normal,
            stretch: None,
            unicode_range: Vec::new(),
            ..Descriptors::default()
        };
        let document = db.add_document_face(Arc::new(web_face("Inter-Latin.woff2", &rule)));
        assert_eq!(db.query(&["serif"], 400.0, FontStyle::Normal), Some(document));
    }

    #[test]
    fn combines_added_faces_with_the_systems_of_the_family() {
        let mut db = system_db(&[("Inter", "Inter-Regular.ttf"), ("Inter", "Inter-Bold.ttf")]);
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts");
        db.add_font_file(&tests.join("Inter-Regular.ttf")).expect("font");
        // The added regular face stands in for the system's ...
        let regular = db.query(&["Inter"], 400.0, FontStyle::Normal).expect("Inter");
        assert!(regular.0 & SYSTEM_FACE == 0 && !db.face(regular).builtin);
        // ... and bold text still gets the system's bold face, not a synthesized one.
        let bold = db.query(&["Inter"], 700.0, FontStyle::Normal).expect("Inter");
        assert!(bold.0 & SYSTEM_FACE != 0);
        assert_eq!(db.face(bold).weight, 700.0);
        // An added bold face hides the system's in turn.
        db.add_font_file(&tests.join("Inter-Bold.ttf")).expect("font");
        let bold = db.query(&["Inter"], 700.0, FontStyle::Normal).expect("Inter");
        assert_eq!(bold.0 & SYSTEM_FACE, 0);
        assert_eq!(db.face(bold).weight, 700.0);
    }

    #[test]
    fn takes_the_generic_families_of_the_language() {
        let db = system_db(&[
            ("Times", "Inter-Regular.ttf"),
            ("Helvetica", "Inter-Regular.ttf"),
            ("Menlo", "Inter-Regular.ttf"),
            ("Hiragino Kaku Gothic ProN", "Inter-Regular.ttf"),
            ("Hiragino Mincho ProN", "Inter-Regular.ttf"),
            ("PingFang SC", "Inter-Regular.ttf"),
            ("PingFang TC", "Inter-Regular.ttf"),
            ("Songti TC", "Inter-Regular.ttf"),
            ("Apple SD Gothic Neo", "Inter-Regular.ttf"),
            (".SF NS", "Inter-Bold.ttf"),
        ]);
        let request = (400.0, FontStyle::Normal, 100.0);
        let query = |families: &[&str], lang: &str| {
            family(
                &db,
                db.query_in(families, request, Some(lang).filter(|lang| !lang.is_empty())),
            )
            .to_owned()
        };
        assert_eq!(query(&["serif"], ""), "Times");
        assert_eq!(query(&["serif"], "ja"), "Hiragino Mincho ProN");
        assert_eq!(query(&[STANDARD], "ja-JP"), "Hiragino Kaku Gothic ProN");
        assert_eq!(query(&["Unknown"], "ko"), "Apple SD Gothic Neo");
        assert_eq!(query(&["sans-serif"], "zh-CN"), "PingFang SC");
        assert_eq!(query(&["sans-serif"], "zh-HK"), "PingFang TC");
        assert_eq!(query(&["serif"], "zh-Hant"), "Songti TC");
        // Songti SC is not installed: Times stands in.
        assert_eq!(query(&["serif"], "zh"), "Times");
        assert_eq!(query(&["monospace"], "ja"), "Menlo");
        // `system-ui` is the system's hidden user interface font, which documents cannot name.
        assert_eq!(query(&["system-ui"], ""), ".SF NS");
        assert_eq!(query(&[".SF NS"], ""), "Times");
        assert_eq!(query(&["cursive"], ""), "Times");
    }

    #[test]
    fn sets_text_without_a_family_in_the_first_family_added_or_inter() {
        // What docs/fonts.md ("Set the generic families") says of an engine without the
        // system's fonts.
        let query =
            |db: &FontDb, name: &str| family(db, db.query(&[name], 400.0, FontStyle::Normal)).to_owned();
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
        let mut db = FontDb::new();
        db.add_default_fonts();
        // Without fonts of the caller, all is Inter: Fagus has no serif font.
        for name in [
            STANDARD,
            "serif",
            "sans-serif",
            "system-ui",
            "monospace",
            "Unknown",
        ] {
            assert_eq!(query(&db, name), "Inter", "{name}");
        }
        // With fonts, the first family added stands for `serif`, `sans-serif` and families no
        // font has, and so for text without a `font-family`.
        db.add_font_file(&tests.join("corpus/quittio/fonts/Geist-Regular.ttf"))
            .expect("font");
        db.add_font_file(&tests.join("webfonts/STIXTwoMath-Regular.woff2"))
            .expect("font");
        for name in [STANDARD, "serif", "sans-serif", "system-ui", "Unknown"] {
            assert_eq!(query(&db, name), "Geist", "{name}");
        }
        // `sans-serif` alone does not change the standard font, which is the `serif` family.
        db.set_generic_families(GenericFamilies {
            sans_serif: Some("Inter".to_owned()),
            ..GenericFamilies::default()
        });
        assert_eq!(query(&db, "sans-serif"), "Inter");
        assert_eq!(query(&db, "Unknown"), "Inter");
        assert_eq!(query(&db, STANDARD), "Geist");
        assert_eq!(query(&db, "serif"), "Geist");
        db.set_generic_families(GenericFamilies {
            serif: Some("STIX Two Math".to_owned()),
            ..GenericFamilies::default()
        });
        assert_eq!(query(&db, STANDARD), "STIX Two Math");
        assert_eq!(query(&db, "serif"), "STIX Two Math");
        assert_eq!(query(&db, "sans-serif"), "Geist");
    }

    #[test]
    fn skips_the_ui_families_that_chrome_does_not_know() {
        let db = system_db(&[
            ("Times", "Inter-Regular.ttf"),
            ("Helvetica", "Inter-Regular.ttf"),
            ("Menlo", "Inter-Regular.ttf"),
            (".SF NS", "Inter-Bold.ttf"),
        ]);
        let query = |families: &[&str]| family(&db, db.query(families, 400.0, FontStyle::Normal)).to_owned();
        // Tailwind's list gets the system font, as in Chrome, not Helvetica.
        assert_eq!(query(&["ui-sans-serif", "system-ui", "sans-serif"]), ".SF NS");
        assert_eq!(query(&["UI-Monospace", "monospace"]), "Menlo");
        // Alone, they get the standard font.
        for family in ["ui-serif", "ui-sans-serif", "ui-monospace", "ui-rounded"] {
            assert_eq!(query(&[family]), "Times", "{family}");
        }
    }

    #[test]
    fn falls_back_to_the_standard_font_then_the_cascade() {
        let db = system_db(&[
            ("Times", "Inter-Latin.woff2"),
            ("Hiragino Kaku Gothic ProN", "Inter-Bold.ttf"),
            ("Lucida Grande", "Inter-Regular.ttf"),
            ("Apple Symbols", "Inter-Regular.ttf"),
            ("Zapf Dingbats", "Inter-Italic.ttf"),
        ]);
        let request = (400.0, FontStyle::Normal, 100.0);
        let rule = Descriptors {
            family: "Brand".to_owned(),
            weight: None,
            style: FontStyle::Normal,
            stretch: None,
            unicode_range: Vec::new(),
            ..Descriptors::default()
        };
        let mut db = db;
        let brand = db.add_document_face(Arc::new(web_face("Inter-Latin.woff2", &rule)));
        let fallback = |character: char, lang: Option<&str>| {
            family(&db, db.fallback_in(&["Brand"], brand, character, request, lang)).to_owned()
        };
        // The standard font of the language comes first.
        assert_eq!(fallback('Ж', Some("ja")), "Hiragino Kaku Gothic ProN");
        // Times lacks the arrow: the sans-serif cascade goes to Lucida Grande.
        assert_eq!(fallback('→', None), "Lucida Grande");
        assert_eq!(fallback('a', None), "Brand");
    }

    #[test]
    fn cascades_by_script_and_kind_of_face() {
        use cascade::{Cascade, families};
        assert_eq!(Cascade::of("Times"), Cascade::Serif);
        assert_eq!(Cascade::of("Inter"), Cascade::SansSerif);
        assert_eq!(Cascade::of("Courier"), Cascade::Monospace);
        assert_eq!(Cascade::of("Menlo"), Cascade::Menlo);
        // Menlo cascades to Monaco, which has `∴`, as in Chrome.
        assert_eq!(families('∴', Cascade::Menlo)[0], "Monaco");
        // Serif text takes the rupee sign from Songti SC, before Helvetica and Geneva.
        let rupee = families('₹', Cascade::Serif);
        let at = |name: &str| rupee.iter().position(|family| *family == name);
        assert!(at("Songti SC") < at("Helvetica") && at("Helvetica") < at("Geneva"));
        assert_eq!(families('漢', Cascade::Serif)[0], "Songti SC");
        assert_eq!(families('漢', Cascade::SansSerif)[0], "PingFang SC");
        assert_eq!(families('か', Cascade::SansSerif)[0], "Hiragino Sans");
        assert_eq!(families('한', Cascade::Serif)[0], "AppleMyungjo");
        assert_eq!(families('한', Cascade::SansSerif)[0], "Apple SD Gothic Neo");
        assert!(families('ש', Cascade::SansSerif).contains(&"Lucida Grande"));
        assert!(families('✔', Cascade::SansSerif).contains(&"Zapf Dingbats"));
        assert_eq!(families('a', Cascade::Serif), [""; 0]);
    }
}
