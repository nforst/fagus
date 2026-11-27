//! Inline layout: shaping, line breaking and line boxes (CSS 2.1 §9.4.2 and §10.8, CSS Text 3).
//!
//! Inline content is split into pieces: shaped words, the edges of inline boxes, atomic inlines and
//! placeholders of out-of-flow boxes. Lines are filled greedily; each line then becomes a tree of
//! inline box fragments, aligned vertically by `vertical-align`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use fagus_dom::NodeId;
use fagus_style::values::{
    FontStyle, Hyphens, LengthPercentage, LengthPercentageAuto, LineHeight, OpticalSizing, Overflow,
    OverflowWrap, TabSize, TextAlign, TextJustify, TextOverflow, TextWrapStyle, VerticalAlign, WordBreak,
};
use fagus_style::{ComputedStyle, ComputedValues};
use fagus_text::{FaceId, FontDb};

use self::context::Around;
use crate::bidi::{Level, TextLevels};
use crate::block::Layouter;
use crate::breaks::{Paragraph, own_breaks};
use crate::counters::counter_text;
use crate::fragment::{
    Children, Fragment, FragmentKind, Glyph, GlyphOrientation, MarkerSymbol, SymbolShape, TextRun,
    VisibleSides,
};
use crate::letter_spacing;
use crate::tree::{BoxId, InlineItem, PageCounter};
use crate::units::{Au, Rect};

mod context;
mod fit;
mod hanging;
pub use hanging::may_hang as hangable_punctuation;
mod initial_letter;
pub(crate) use initial_letter::raised_lines;
mod kerning;
mod score;
pub(crate) mod spacing;

/// An inline box of the content: an element with `display: inline`.
#[derive(Clone, Debug)]
struct InlineBox {
    style: Arc<ComputedStyle>,
    node: Option<NodeId>,
    /// Margin, border and padding at the start and the end, in pixels.
    margin: (f32, f32),
    frame: (f32, f32),
    /// Border and padding above and below the content area.
    frame_top: f32,
    frame_bottom: f32,
}

/// A float met on a line: where it is in the queue of pieces, what it was there, and how many
/// floats the formatting context, the line and the floats waiting for the next line held, and
/// how far down the line's floats reached, before it.
struct MetFloat {
    index: usize,
    kind: PieceKind,
    placed: (usize, usize, Au),
    waiting: usize,
}

#[derive(Clone, Debug)]
enum PieceKind {
    /// A word (or another piece that cannot break), shaped with one face.
    Text {
        face: FaceId,
        glyphs: Vec<Glyph>,
        text: String,
        caps: Caps,
    },
    /// The start of inline box `index`.
    Open(usize),
    /// The end of inline box `index`.
    Close(usize),
    /// The left side of a fragment of inline box `index` on a line reordered for bidi text,
    /// with the box's margin, border and padding there if `edge`.
    Left { index: usize, edge: bool },
    /// The right side of such a fragment.
    Right { index: usize, edge: bool },
    /// An atomic inline, laid out: its fragment (positioned at its margins), and its baseline
    /// from the top of its margin box.
    Atomic {
        fragment: Box<Fragment>,
        height: f32,
        baseline: f32,
        /// The content of a ruby column that may break across lines.
        ruby: Option<Box<crate::ruby::Part>>,
    },
    /// The static position of an out-of-flow box.
    Placeholder(BoxId),
    /// `leader()`: its pattern shaped once, repeated over the room left on its line.
    Leader {
        face: FaceId,
        glyphs: Vec<Glyph>,
        text: String,
        caps: Caps,
        /// The width of one pattern.
        unit: f32,
    },
    /// An atomic inline that was only measured (for intrinsic widths), not laid out.
    Measured,
    /// A float, laid out at its margins, with the size of its margin box. It takes no space on
    /// the line; the line breaker places it beside the line or below it.
    Float {
        id: BoxId,
        fragment: Option<Box<Fragment>>,
        size: (Au, Au),
        side: fagus_style::values::Float,
        clear: fagus_style::values::Clear,
    },
    /// A list marker: shaped runs that start at `start` from the start of the line (outside
    /// the box, taking no space on it) or from where the marker is on the line (inside).
    Marker {
        runs: Vec<(FaceId, Vec<Glyph>, String, f32, Caps)>,
        start: f32,
        inside: bool,
        /// An image in place of the runs, with its size.
        image: Option<(Arc<crate::Image>, f32, f32)>,
        /// The symbol drawn in place of the glyphs of the one run, whose width is its box's.
        symbol: Option<MarkerSymbol>,
    },
}

/// How text is set for `font-variant-caps` in a face, as Chrome sets it (CSS Fonts 4, §6.3):
/// with the face's features for the capitals asked for, or else with small capitals made up from
/// capitals of a smaller size; and in a vertical line, upright or sideways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Caps {
    features: &'static [([u8; 4], u32)],
    case: Case,
    /// Whether the text is set at the size of made-up small capitals.
    small: bool,
    /// Whether the glyphs stand upright in a vertical line, shaped top to bottom.
    upright: bool,
}

/// The case in which text is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Same,
    Upper,
    Lower,
}

impl Caps {
    const NONE: Self = Self {
        features: &[],
        case: Case::Same,
        small: false,
        upright: false,
    };

    /// The font size of text set at `size`: for made-up small capitals 70 % of it, rounded, as
    /// in Chrome.
    fn size(self, size: f32) -> f32 {
        if self.small { (size * 0.7).round() } else { size }
    }
}

/// The runs of `text` in `face` with how each is set for the style's `font-variant-caps`: the
/// face's own capitals where it has them (small capitals standing in for petite ones and for
/// unicase), else small capitals made up where `font-synthesis` allows them: letters with a
/// capital become that capital at a smaller size; with `all-` capitals all other characters
/// take the smaller size too, with unicase only they do.
fn caps_runs(fonts: &FontDb, face: FaceId, style: &ComputedStyle, text: &str) -> Vec<(Range<usize>, Caps)> {
    use fagus_style::values::FontVariantCaps as Asked;
    const SMALL: &[([u8; 4], u32)] = &[(*b"smcp", 1)];
    const ALL_SMALL: &[([u8; 4], u32)] = &[(*b"smcp", 1), (*b"c2sc", 1)];
    const PETITE: &[([u8; 4], u32)] = &[(*b"pcap", 1)];
    const ALL_PETITE: &[([u8; 4], u32)] = &[(*b"pcap", 1), (*b"c2pc", 1)];
    const UNICASE: &[([u8; 4], u32)] = &[(*b"unic", 1)];
    const TITLING: &[([u8; 4], u32)] = &[(*b"titl", 1)];
    let asked = style.font_variant.caps;
    let whole = |caps| vec![(0..text.len(), caps)];
    let face = fonts.face(face);
    let has = |tags: &[&[u8; 4]]| tags.iter().all(|tag| face.substitutes(**tag));
    let with = |features| Caps {
        features,
        ..Caps::NONE
    };
    match asked {
        Asked::Normal => return whole(Caps::NONE),
        Asked::SmallCaps if has(&[b"smcp"]) => return whole(with(SMALL)),
        Asked::AllSmallCaps if has(&[b"smcp", b"c2sc"]) => return whole(with(ALL_SMALL)),
        Asked::PetiteCaps if has(&[b"pcap"]) => return whole(with(PETITE)),
        Asked::PetiteCaps if has(&[b"smcp"]) => return whole(with(SMALL)),
        Asked::AllPetiteCaps if has(&[b"pcap", b"c2pc"]) => return whole(with(ALL_PETITE)),
        Asked::AllPetiteCaps if has(&[b"smcp", b"c2sc"]) => return whole(with(ALL_SMALL)),
        Asked::Unicase if has(&[b"unic"]) => return whole(with(UNICASE)),
        Asked::TitlingCaps if has(&[b"titl"]) => return whole(with(TITLING)),
        // Titling capitals are not made up.
        Asked::TitlingCaps => return whole(Caps::NONE),
        _ => {}
    }
    // Unicase with small capitals of the face sets capitals as small capitals of their
    // lowercase letters.
    let unicase_features = asked == Asked::Unicase && has(&[b"smcp"]);
    if !unicase_features && !style.font_synthesis.small_caps {
        return whole(Caps::NONE);
    }
    let mut runs: Vec<(Range<usize>, Caps)> = Vec::new();
    for (offset, character) in text.char_indices() {
        let caps = match (asked, has_capital(character)) {
            (Asked::Unicase, true) => Caps::NONE,
            (Asked::Unicase, false) if unicase_features => Caps {
                case: Case::Lower,
                ..with(SMALL)
            },
            (Asked::Unicase | Asked::AllSmallCaps | Asked::AllPetiteCaps, false) => Caps {
                small: true,
                ..Caps::NONE
            },
            (_, true) => Caps {
                case: Case::Upper,
                small: true,
                ..Caps::NONE
            },
            (_, false) => Caps::NONE,
        };
        let end = offset + character.len_utf8();
        match runs.last_mut() {
            Some((range, last)) if *last == caps => range.end = end,
            _ => runs.push((offset..end, caps)),
        }
    }
    runs
}

/// Whether a character changes when uppercased, which made-up small capitals set as its
/// capitals, as Chrome does (Blink's `SmallCapsIterator`, by `UCHAR_CHANGES_WHEN_UPPERCASED`):
/// `ß` becomes small `SS`.
fn has_capital(character: char) -> bool {
    !character.to_uppercase().eq(std::iter::once(character))
}

/// `text` in `case` with the byte offset in `text` of each byte of the result; `None` where the
/// case stays.
fn cased(text: &str, case: Case) -> Option<(String, Vec<u32>)> {
    if case == Case::Same {
        return None;
    }
    let mut result = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len());
    for (offset, character) in text.char_indices() {
        match case {
            Case::Upper => result.extend(character.to_uppercase()),
            _ => result.extend(character.to_lowercase()),
        }
        offsets.resize(result.len(), offset as u32);
    }
    Some((result, offsets))
}

#[derive(Clone, Debug)]
struct Piece {
    kind: PieceKind,
    /// The style of the text, or of the box the piece belongs to.
    style: Arc<ComputedStyle>,
    /// Width including trailing spaces.
    width: f32,
    /// The narrowest the piece can be: less than `width` only for atomic inlines.
    min_width: f32,
    /// Width of the trailing spaces, which hang at the end of a line.
    trailing_space: f32,
    /// Whether a line may break after this piece.
    break_after: bool,
    /// Whether the line must break after this piece.
    forced_break_after: bool,
    /// The `clear` of the `<br>` that forces the break after this piece: the next line starts
    /// below the floats it names, and the container grows to reach past them.
    break_clear: fagus_style::values::Clear,
    /// The width of the hyphen drawn when the line breaks after this piece: a soft hyphen or a
    /// hyphenation point. 0 where a break needs no hyphen.
    hyphen: f32,
    /// Whether the piece ends at a hyphenation point of the paragraph's last word, where a line
    /// breaks only if the word starts it (as in Blink's `ShouldHyphenate`).
    last_word_hyphenation: bool,
    /// The bidi level of the piece and of its paragraph; text at an odd level is shaped right to
    /// left.
    bidi: Level,
    /// What the piece takes from the characters around it where lines start or end at it.
    edges: spacing::Edges,
    /// What the piece becomes where a line breaks after it, if not itself: where the break lies
    /// inside a ligature or between glyphs that shaping set together (kerned).
    broken: Option<Box<Broken>>,
}

/// A piece that ends a line where it breaks inside a word at a place that shaping does not
/// allow to break as it is, as Chrome shapes the text before such a break again. The word is
/// set whole as long as it does not break there.
#[derive(Clone, Debug)]
struct Broken {
    /// The piece shaped again with the text of the next piece up to the break.
    end: Piece,
    /// The bytes of the next piece's text before the break (a ligature's that the break
    /// divides), which the next line starts without: it is shaped again from there.
    into_next: usize,
}

/// A laid-out line box.
#[derive(Clone, Debug)]
pub struct Line {
    pub height: Au,
    pub fragment: Fragment,
    /// The top of the line below the top of the first line: lines move down past floats that
    /// leave too little room.
    pub(crate) top: Au,
    /// The piece after the line, where the next line starts, and the byte offset in its text
    /// where a word broke inside.
    pub(crate) end: (usize, usize),
    /// The floats placed with the line, positioned like the line (from the top left corner of
    /// the first line's content box).
    pub(crate) floats: Vec<Fragment>,
    /// The bottom of those floats, from the top of the first line.
    pub(crate) floats_bottom: Au,
    /// How many floats the formatting context holds after the line.
    pub(crate) float_count: usize,
    /// The room below the line that a `<br>` with `clear` at its end adds to reach past the
    /// floats it names: the next line starts there, and the container reaches that far.
    pub(crate) clearance_after: Au,
    /// How far the annotations under the line reach below it, which the lines after it make
    /// room for: the line fits on a page only with them, as in Chrome (WPT css-break
    /// ruby-001).
    pub(crate) annotations_under: Au,
    /// How far the line moved down to make room for its annotations over it: at the top of a
    /// page, nothing but them lies above it.
    pub(crate) annotations_over: Au,
    /// Whether the line has no content (text, atomic inlines, inline box edges or a forced
    /// break): it takes no space, and margins collapse through it (CSS 2.1, §9.4.2).
    pub(crate) phantom: bool,
    /// The factor by which `text-fit` scaled the line's text, and its edges for `text-box-trim`.
    pub(crate) text_scale: f32,
}

/// How far the content of a line may reach past the room for it: one layout unit (1/64 px),
/// as Chrome's line breaker fits content whose width, rounded up to layout units, is at most the
/// room plus one (`LineBreaker::AvailableWidthToFit`). Text 439.012 px wide fits on a line 439 px
/// wide.
pub(crate) const FIT_TOLERANCE: f32 = 1.0 / 64.0;

/// Font face and size of a style.
pub fn face_for(fonts: &FontDb, style: &ComputedStyle) -> Option<FaceId> {
    let families: Vec<&str> = style.font_family.iter().map(String::as_str).collect();
    fonts.query_in(
        &families,
        (style.font_weight, font_style(style), style.font_stretch),
        style.lang.as_deref(),
    )
}

/// The size at which a style's text is set: its font size, adjusted by `font-size-adjust` so that
/// the metric it names is the aspect value it gives times the font size (CSS Fonts 5, §2.6), as
/// Chrome adjusts it, by the style's first available font. Line heights from numbers and `em`
/// stay with the font size. Chrome sets text at that size cut to hundredths of a pixel (the
/// precision of its font cache, `FontCacheKey::PrecisionMultiplier`): text of 10pt, 13.333
/// pixels, is set at 13.33.
pub fn used_font_size(fonts: &FontDb, style: &ComputedStyle) -> f32 {
    let size = adjusted_font_size(fonts, style);
    if size.is_finite() && size > 0.0 {
        (size * 100.0).trunc() / 100.0
    } else {
        size
    }
}

/// The font size of a style, adjusted by `font-size-adjust`; see [`used_font_size`].
fn adjusted_font_size(fonts: &FontDb, style: &ComputedStyle) -> f32 {
    use fagus_style::values::FontSizeAdjust;
    let (metric, aspect) = match &style.font_size_adjust {
        FontSizeAdjust::None | FontSizeAdjust::FromFont(_, None) => return style.font_size,
        FontSizeAdjust::Aspect(metric, aspect) => (*metric, *aspect),
        FontSizeAdjust::FromFont(metric, Some(request)) => {
            let families: Vec<&str> = request.families.iter().map(String::as_str).collect();
            let font_style = text_font_style(request.style);
            let Some(face) = fonts.query_in(
                &families,
                (request.weight, font_style, request.stretch),
                request.lang.as_deref(),
            ) else {
                return style.font_size;
            };
            (*metric, aspect_value(fonts, face, *metric))
        }
    };
    let own = face_for(fonts, style).map_or(0.0, |face| aspect_value(fonts, face, metric));
    if own > 0.0 && aspect.is_finite() {
        style.font_size * aspect / own
    } else {
        style.font_size
    }
}

/// The fonts of a document as the cascade asks for them: the metrics of the first available font
/// for an element's font properties, which its font-relative lengths refer to (`ex`, `ch`, `cap`
/// and `ic`) and whose line spacing `line-height: normal` is (`lh`). A face without a "0" gives
/// `ch` half an em, as in Chrome.
pub struct StyleFonts<'a>(pub &'a FontDb);

impl fagus_style::values::FontMetrics for StyleFonts<'_> {
    fn units(&self, request: &fagus_style::values::FontRequest) -> fagus_style::values::FontUnits {
        use fagus_style::values::{FontMetric, FontUnits};
        let fonts = self.0;
        let families: Vec<&str> = request.families.iter().map(String::as_str).collect();
        let font_style = text_font_style(request.style);
        let Some(face) = fonts.first_available(
            &families,
            (request.weight, font_style, request.stretch),
            request.lang.as_deref(),
        ) else {
            return FontUnits::default();
        };
        let fallback = FontUnits::default();
        let positive = |value: f32, otherwise: f32| {
            if value > 0.0 && value.is_finite() {
                value
            } else {
                otherwise
            }
        };
        // The "0" of a variable face at the weight, width and optical size of the request, as
        // Chrome measures it.
        let ch = if fonts.face(face).axes.is_empty() {
            aspect_value(fonts, face, FontMetric::ChWidth)
        } else {
            let optical_size = request
                .optical_size
                .map(|size| size * fonts.face(face).size_adjust);
            let variations = fonts.variations(
                face,
                (request.weight, font_style, request.stretch),
                optical_size,
                &[],
            );
            let em = fonts.face(face).metrics.units_per_em / fonts.face(face).size_adjust;
            fonts.shape_variable(face, "0", &[], &variations).advance / em
        };
        // The x-height and cap height at the size of the request, where the face's are fitted to
        // the pixel grid (see `Face::x_height_at`).
        let at_size = |height: fn(&fagus_text::Face, f32) -> f32, metric: FontMetric| {
            if request.size > 0.0 && request.size.is_finite() {
                height(fonts.face(face), request.size) / request.size
            } else {
                aspect_value(fonts, face, metric)
            }
        };
        FontUnits {
            ex: positive(
                at_size(fagus_text::Face::x_height_at, FontMetric::ExHeight),
                fallback.ex,
            ),
            // The face's own "0", outside its `unicode-range` too, as Chrome measures it.
            ch: if fonts.face(face).glyph('0').is_some() {
                positive(ch, fallback.ch)
            } else {
                fallback.ch
            },
            cap: positive(
                at_size(fagus_text::Face::cap_height_at, FontMetric::CapHeight),
                fallback.cap,
            ),
            // A water ideograph of no advance makes `ic` nothing, as in Chrome.
            ic: Some(aspect_value(fonts, face, FontMetric::IcWidth))
                .filter(|ic| ic.is_finite() && *ic >= 0.0)
                .unwrap_or(fallback.ic),
            math_scale: crate::math::script_scales(fonts.face(face)),
            ch_upright: upright_zero(fonts, face),
            ascent: fonts.face(face).metrics.ascender * fonts.face(face).scale(1.0),
            descent: fonts.face(face).metrics.descender * fonts.face(face).scale(1.0),
            // The face's adjustment at a size where its ascent and descent are large.
            ascent_hack: fonts.face(face).ascent_adjustment(100.0, 0.0) > 0.0,
        }
    }

    fn line_spacing(&self, request: &fagus_style::values::FontRequest, size: f32) -> Option<f32> {
        let fonts = self.0;
        let families: Vec<&str> = request.families.iter().map(String::as_str).collect();
        let font_style = text_font_style(request.style);
        let face = fonts.query_in(
            &families,
            (request.weight, font_style, request.stretch),
            request.lang.as_deref(),
        )?;
        // The size as text is laid out in it (see `used_font_size`).
        let size = if size.is_finite() && size > 0.0 {
            (size * 100.0).trunc() / 100.0
        } else {
            size
        };
        Some(face_metrics(fonts, face, size).line_height)
    }
}

/// The metric of a face that `font-size-adjust` names, divided by its em.
fn aspect_value(fonts: &FontDb, face: FaceId, metric: fagus_style::values::FontMetric) -> f32 {
    use fagus_style::values::FontMetric;
    // An em of the face, in font units: its `size-adjust` makes it smaller.
    let em = fonts.face(face).metrics.units_per_em / fonts.face(face).size_adjust;
    let advance = |text: &str| fonts.shape(face, text).advance / em;
    let metrics = &fonts.face(face).metrics;
    match metric {
        FontMetric::ExHeight => metrics.x_height / em,
        FontMetric::CapHeight => metrics.cap_height / em,
        FontMetric::ChWidth => advance("0"),
        // The water ideograph, an em wide and high where a face lacks it.
        FontMetric::IcWidth if fonts.face(face).glyph('\u{6C34}').is_some() => advance("\u{6C34}"),
        FontMetric::IcWidth | FontMetric::IcHeight => 1.0,
    }
}

/// The coordinates of a variable face for text in a style at `size` (CSS pixels): from the
/// weight, the width, the size and `font-variation-settings`. Empty for other faces.
pub fn font_variations(
    fonts: &FontDb,
    face: FaceId,
    style: &ComputedStyle,
    size: f32,
) -> Vec<([u8; 4], f32)> {
    if fonts.face(face).axes.is_empty() {
        return Vec::new();
    }
    // The optical size is that of the text as set: with the face's `size-adjust`.
    let optical_size =
        (style.font_optical_sizing == OpticalSizing::Auto).then(|| size * fonts.face(face).size_adjust);
    fonts.variations(
        face,
        (style.font_weight, font_style(style), style.font_stretch),
        optical_size,
        &style.font_variation_settings,
    )
}

/// The coordinates of a variable face for a text run (see [`font_variations`]): made-up small
/// capitals, set smaller than their style's font size, keep the optical size of that size, as
/// in Chrome.
pub fn run_variations(fonts: &FontDb, run: &TextRun) -> Vec<([u8; 4], f32)> {
    font_variations(fonts, run.face, &run.style, unscaled_size(fonts, run))
}

/// The font size of the text a run stands for: for made-up small capitals the size of their
/// style, which their font only scales down, else the run's.
pub(crate) fn unscaled_size(fonts: &FontDb, run: &TextRun) -> f32 {
    let used = used_font_size(fonts, &run.style);
    let small = Caps {
        small: true,
        ..Caps::NONE
    };
    let small_size = small.size(used);
    if run.style.font_variant.caps != fagus_style::values::FontVariantCaps::Normal
        && (run.size - used).abs() > 1e-4
        && (run.size - small_size).abs() < 1e-4
    {
        used
    } else {
        run.size
    }
}

pub(crate) fn font_style(style: &ComputedStyle) -> fagus_text::FontStyle {
    text_font_style(style.font_style)
}

/// A `font-style` as font matching takes it.
pub fn text_font_style(style: FontStyle) -> fagus_text::FontStyle {
    match style {
        FontStyle::Normal => fagus_text::FontStyle::Normal,
        FontStyle::Italic => fagus_text::FontStyle::Italic,
        FontStyle::Oblique(tenths) => fagus_text::FontStyle::Oblique(tenths),
    }
}

/// Vertical font metrics of a style in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_height: f32,
    pub x_height: f32,
}

impl Metrics {
    /// The leading above the text: half of it, cut to whole pixels as browsers do.
    pub(crate) fn leading_above(&self) -> f32 {
        half_leading(self.line_height - (self.ascent + self.descent))
    }

    /// The leading below the text: the rest of it.
    fn leading_below(&self) -> f32 {
        self.line_height - (self.ascent + self.descent) - self.leading_above()
    }
}

/// The part of `leading` (pixels, in 1/64 steps) above the text, as Blink splits it
/// (`CalculateLeadingSpace`): half of it in layout units of 1/64 pixel, the division cutting
/// toward zero, then floored to whole pixels. A leading of -1/64 pixel thus puts nothing above
/// the text, where flooring its half would take a whole pixel away.
#[allow(clippy::cast_possible_truncation, reason = "layout units of a line's leading")]
pub(crate) fn half_leading(leading: f32) -> f32 {
    let units = (leading * 64.0).round() as i32;
    (f64::from(units / 2) / 64.0).floor() as f32
}

/// The metrics of a style's first available font, with `line-height: normal` from the font.
pub fn metrics(fonts: &FontDb, style: &ComputedStyle) -> Metrics {
    on_baseline(alphabetic_metrics(fonts, style), style)
}

/// The metrics of a style's first available font around its alphabetic baseline, in vertical
/// lines too.
pub(crate) fn alphabetic_metrics(fonts: &FontDb, style: &ComputedStyle) -> Metrics {
    let Some(face) = face_for(fonts, style) else {
        return Metrics {
            ascent: style.font_size * 0.8,
            descent: style.font_size * 0.2,
            line_height: style.line_height_px(style.font_size * 1.2),
            x_height: style.font_size * 0.5,
        };
    };
    let fit = style.text_fit_scale;
    if (fit - 1.0).abs() > f32::EPSILON && fit > 0.0 {
        // Text scaled by `text-fit` has the metrics of its font at the size before, scaled (the
        // size cut to hundredths of a pixel as `used_font_size` cuts it).
        let before = (adjusted_font_size(fonts, style) / fit * 1000.0).round() / 1000.0;
        let metrics = face_metrics(fonts, face, (before * 100.0).trunc() / 100.0);
        return Metrics {
            ascent: metrics.ascent * fit,
            descent: metrics.descent * fit,
            line_height: style.line_height_px(metrics.line_height * fit),
            x_height: metrics.x_height * fit,
        };
    }
    let metrics = face_metrics(fonts, face, used_font_size(fonts, style));
    Metrics {
        line_height: style.line_height_px(metrics.line_height),
        ..metrics
    }
}

/// Metrics around the baseline of a style: in vertical lines whose text is not all sideways the
/// central one, halfway between the ascent and the descent (whole pixels, the ascent taking the
/// odd one), as in Chrome.
fn on_baseline(metrics: Metrics, style: &ComputedStyle) -> Metrics {
    if !central_baseline(style) {
        return metrics;
    }
    let height = metrics.ascent + metrics.descent;
    let descent = (height / 2.0).floor();
    Metrics {
        ascent: height - descent,
        descent,
        ..metrics
    }
}

/// Whether the text of a style sits on the central baseline: in a vertical line, unless all of
/// it is set sideways (CSS Inline 3, `dominant-baseline: auto`).
pub(crate) fn central_baseline(style: &ComputedStyle) -> bool {
    style.is_vertical()
        && !style.writing_mode.is_sideways()
        && style.text_orientation != fagus_style::values::TextOrientation::Sideways
}

/// The metrics of a face at `size` pixels, with its own line height (`line-height: normal`).
fn face_metrics(fonts: &FontDb, face: FaceId, size: f32) -> Metrics {
    let face = fonts.face(face);
    let scale = face.scale(size);
    // Browsers round the font's ascent, descent and line gap to whole pixels.
    let (ascent, descent) = face.rounded_ascent_descent(size);
    Metrics {
        ascent,
        descent,
        line_height: ascent + descent + (face.metrics.line_gap * scale).round(),
        x_height: face.x_height_at(size),
    }
}

/// The advance of the "0" of a face set upright in vertical text, per pixel of font size, where
/// the face has vertical metrics of its own; `None` where Chrome advances upright glyphs by the
/// face's ascent and descent.
fn upright_zero(fonts: &FontDb, face: FaceId) -> Option<f32> {
    let font = fonts.face(face);
    if !font.vertical_metrics || font.glyph('0').is_none() {
        return None;
    }
    let shaped = fonts.shape_vertical(face, "0", &[], &[]);
    let advance = shaped.advance * font.scale(1.0);
    (advance.is_finite() && advance > 0.0).then_some(advance)
}

/// A list marker whose symbol is drawn as a shape, as Blink lays it out (`ListMarker`): where its
/// box starts (from the start of the content outside the list item, from where it is on the line
/// inside), the room it takes on the line, its box's width and its symbol. A bullet is half of
/// 2/3 of the rounded ascent of the marker's font (whole pixels), a pixel into a box two pixels
/// wider that ends 7 pixels before the content outside, and is followed by 1em inside; a
/// disclosure triangle is 0.66em large, on the baseline, with 0.4em after it inside.
fn symbol_marker(
    fonts: &FontDb,
    style: &ComputedStyle,
    shape: SymbolShape,
    inside: bool,
) -> (f32, f32, (f32, MarkerSymbol)) {
    // Blink's layout units: 1/64 of a pixel, cut.
    let units = |value: f32| (value * 64.0).floor() / 64.0;
    let ascent = alphabetic_metrics(fonts, style).ascent as i32;
    if let SymbolShape::Disclosure { .. } = shape {
        let size = units(style.font_size * 0.66);
        let symbol = MarkerSymbol {
            shape,
            x: 0.0,
            y: ascent as f32 - size,
            size,
        };
        let (start, room) = if inside {
            (0.0, size + units(style.font_size * 0.4))
        } else {
            (-(size + 8.0), 0.0)
        };
        return (start, room, (size, symbol));
    }
    let offset = ascent.saturating_mul(2) / 3;
    let bullet = (offset.saturating_add(1) / 2) as f32;
    let symbol = MarkerSymbol {
        shape,
        x: 1.0,
        y: (3 * (ascent - offset) / 2) as f32,
        size: bullet,
    };
    let width = bullet + 2.0;
    let (start, room) = if inside {
        (-1.0, width - 1.0 + units(style.font_size))
    } else {
        (-(offset as f32 + 8.0), 0.0)
    };
    (start, room, (width, symbol))
}

/// Lays out inline content into lines of width `width`, in a formatting context of its own (floats
/// in it go with the line they are placed with).
pub fn layout_lines(
    layouter: &Layouter<'_>,
    container: &ComputedStyle,
    items: &[InlineItem],
    width: Au,
) -> Vec<Line> {
    layout_lines_from(layouter, container, items, width, (0, 0))
}

/// Like [`layout_lines`], from `start`: where the line after one laid out before starts (its
/// [`Line::end`]).
pub(crate) fn layout_lines_from(
    layouter: &Layouter<'_>,
    container: &ComputedStyle,
    items: &[InlineItem],
    width: Au,
    start: (usize, usize),
) -> Vec<Line> {
    layouter.push_context();
    let mut lines = lines_in_flow(
        layouter,
        (container, None),
        (items, None),
        width,
        (start, None),
        (Au::ZERO, Au::ZERO),
    );
    layouter.pop_context();
    for line in &mut lines {
        for mut float in std::mem::take(&mut line.floats) {
            float.rect.x -= line.fragment.rect.x;
            float.rect.y -= line.top;
            line.fragment.children.push(float);
        }
    }
    lines
}

/// The shaped content of the inline formatting context laid out last, by its box and width. A
/// paragraph that spans many pages is laid out again on each from where the page before ended;
/// shaping all of it each time would take time growing with the square of its length. Content
/// whose shaping depends on the page (page numbers, named strings) or on layout (floats,
/// out-of-flow boxes) is not kept; content with atomic inlines (inline blocks and tables) is kept
/// with the state it was shaped in, and shaped again where that differs.
#[derive(Default)]
pub(crate) struct ShapedCache(RefCell<Option<KeptShaped>>);

/// Shaped content kept, by what it is kept by, with the state it was shaped in where that
/// matters (see [`ShapedCache`]).
type KeptShaped = (ShapedKey, Option<crate::memo::Kept>, Rc<Shaped>);

/// Shaped inline content: its pieces and its inline boxes.
type Shaped = (Vec<Piece>, Vec<InlineBox>);

/// What shaped content is kept by: the box, the line width, the container's style (by address)
/// and the height that percentages inside refer to, where something inside has a size of its
/// own (an atomic inline).
type ShapedKey = (BoxId, Au, usize, Option<Au>);

impl ShapedCache {
    /// The pieces and inline boxes of `items`, shaped for lines of `width`.
    fn get_or_shape(
        &self,
        layouter: &Layouter<'_>,
        (items, owner): (&[InlineItem], Option<BoxId>),
        container: &ComputedStyle,
        width: Au,
    ) -> Rc<Shaped> {
        let shape = || {
            Rc::new(shape_pieces(
                layouter,
                (items, Some(container)),
                width,
                Purpose::Layout,
            ))
        };
        let Some((owner, stateful)) = owner.zip(kept_shaping(layouter.tree, items)) else {
            return shape();
        };
        let sized = items.iter().any(|item| matches!(item, InlineItem::Atomic(_)));
        let key = (
            owner,
            width,
            std::ptr::from_ref(container) as usize,
            layouter.inline_containing_height.get().filter(|_| sized),
        );
        if let Some((cached, kept, shaped)) = &*self.0.borrow()
            && *cached == key
        {
            match kept {
                None => return Rc::clone(shaped),
                Some(kept) if layouter.repeats(kept) => {
                    layouter.repeat(kept);
                    return Rc::clone(shaped);
                }
                Some(_) => {}
            }
        }
        let (shaped, kept) = if stateful {
            let (shaped, kept) = layouter.keeping(shape);
            let Some(kept) = kept else {
                return shaped;
            };
            (shaped, Some(kept))
        } else {
            (shape(), None)
        };
        *self.0.borrow_mut() = Some((key, kept, Rc::clone(&shaped)));
        shaped
    }
}

/// Whether the shaping of inline content may be kept (see [`ShapedCache`]): `Some(false)` where
/// it is only text, list markers, images and ruby in inline boxes, whose shaping and lines
/// depend on nothing but itself and the width of the lines; `Some(true)` where it has other
/// atomic inlines too, whose layouts depend on the state of the layout as well.
fn kept_shaping(tree: &crate::tree::BoxTree, items: &[InlineItem]) -> Option<bool> {
    let mut stateful = false;
    for item in items {
        match item {
            InlineItem::Text { .. }
            | InlineItem::LineBreak { .. }
            | InlineItem::Open { .. }
            | InlineItem::Close
            | InlineItem::Marker { .. }
            | InlineItem::Leader { .. } => {}
            InlineItem::Atomic(id)
                if matches!(tree.get(*id).kind, crate::tree::BoxKind::Replaced { .. })
                    || tree.ruby_columns.contains_key(id) => {}
            InlineItem::Atomic(_) => stateful = true,
            _ => return None,
        }
    }
    Some(stateful)
}

/// Lays out inline content from piece `start.0` on (`start.1` bytes into its text, where a word
/// broke inside) into lines of width `width`, beside the floats of the current block formatting
/// context. `flow` is where the top left corner of the first line's content box lies in that
/// context. Floats in the content are placed there too.
/// `owner` is the box whose content `items` is, if it is one of the box tree: its shaped content
/// is kept for the next page (see [`ShapedCache`]). With a `spacer`, lines move apart where their
/// ruby annotations and emphasis marks need room, as they are laid out.
pub(crate) fn lines_in_flow(
    layouter: &Layouter<'_>,
    styles: (&ComputedStyle, Option<&ComputedStyle>),
    content: (&[InlineItem], Option<BoxId>),
    width: Au,
    (start, spacer): ((usize, usize), Option<crate::annotation::Spacer>),
    flow: (Au, Au),
) -> Vec<Line> {
    fagus_dom::budget::check_time();
    let mut fitting = fit::Fitting::of(layouter, styles.0, content.0);
    // A `consistent` fit is measured on all lines of the paragraph, laid out without it.
    if matches!(fitting, fit::Fitting::Consistent(_)) {
        let mut measure = fit::Fitting::measuring();
        flow_lines(
            layouter,
            styles,
            content,
            width,
            ((0, 0), None),
            flow,
            &mut measure,
        );
        fitting = fitting.measured(&measure);
    }
    flow_lines(
        layouter,
        styles,
        content,
        width,
        (start, spacer),
        flow,
        &mut fitting,
    )
}

/// [`lines_in_flow`], with the lines fitted by `text-fit` as `fitting` tells.
fn flow_lines(
    layouter: &Layouter<'_>,
    (container, first_line): (&ComputedStyle, Option<&ComputedStyle>),
    (items, owner): (&[InlineItem], Option<BoxId>),
    width: Au,
    (start, mut spacer): ((usize, usize), Option<crate::annotation::Spacer>),
    flow: (Au, Au),
    fitting: &mut fit::Fitting,
) -> Vec<Line> {
    let fonts = layouter.fonts;
    // The lines `line-clamp` shows still to come; those after them are laid out as overflow.
    let budget = layouter.line_budget.get();
    let shaped = layouter
        .shaped
        .get_or_shape(layouter, (items, owner), container, width);
    let (pieces, boxes) = (&shaped.0, &shaped.1);
    // The floats placed before this content: an outside marker stands beside them, but not
    // beside the floats of the content itself, as in Chrome.
    let floats_before = layouter.float_count();
    // Looked for among the pieces where the items have one: a paragraph may have a million.
    let outside_marker = items
        .iter()
        .any(|item| matches!(item, InlineItem::Marker { inside: false, .. }))
        && pieces
            .iter()
            .any(|piece| matches!(piece.kind, PieceKind::Marker { inside: false, .. }));
    if outside_marker && layouter.list_item_edges.get().is_none() {
        // In a formatting context inside the list item: placed once the item is laid out.
        layouter.unplaced_marker.set(layouter.in_list_item.get());
    }
    let strut = metrics(fonts, container);
    let builder = LineBuilder {
        fonts,
        container,
        boxes,
        strut,
        quirk: layouter.tree.line_height_quirk,
    };
    // The first line in the style of `::first-line`, where its pieces match the others.
    let pseudo_first_line = first_line;
    let first_container = first_line
        .filter(|_| start == (0, 0))
        .map(|first_line| restyled(container, container, first_line));
    let first = first_line
        .filter(|_| first_container.is_some())
        .and_then(|first_line| {
            let mut before = crate::text_case::Before::default();
            let items: Vec<InlineItem> = items
                .iter()
                .map(|item| match item {
                    InlineItem::Text { text, style } => {
                        let first_style = restyled(style, container, first_line);
                        // Text is transformed as the tree is built: again where the line transforms
                        // it otherwise.
                        let text = if first_style.text_transform == style.text_transform {
                            text.clone()
                        } else {
                            crate::tree::transform(
                                text,
                                first_style.text_transform,
                                first_style.lang.as_deref(),
                                before,
                            )
                        };
                        before.push_str(&text);
                        InlineItem::Text {
                            text,
                            style: Arc::new(first_style),
                        }
                    }
                    InlineItem::Open { style, node } => InlineItem::Open {
                        style: Arc::new(restyled(style, container, first_line)),
                        node: *node,
                    },
                    other => other.clone(),
                })
                .collect();
            let (first_pieces, first_boxes) =
                shape_pieces(layouter, (&items, Some(container)), width, Purpose::Layout);
            // Pieces that differ in number (made-up small capitals split words into runs of
            // their own) carry on where the same text does.
            let map = if first_pieces.len() == pieces.len() {
                None
            } else {
                Some(FirstLineMap::new(&first_pieces, pieces)?)
            };
            Some(((first_pieces, first_boxes), map))
        });
    let (first, first_map) = first.unzip();
    let first_map = first_map.flatten();
    let (first_pieces, first_boxes) = first.unzip();
    let first_builder =
        first_boxes
            .as_ref()
            .zip(first_container.as_ref())
            .map(|(first_boxes, first_container)| LineBuilder {
                fonts,
                container: first_container,
                boxes: first_boxes,
                strut: metrics(fonts, first_container),
                quirk: layouter.tree.line_height_quirk,
            });
    let strut_height = Au::from_px(strut.line_height);
    let tab_stops = tab_stops(fonts, container);
    let (left_edge, right_edge) = (flow.0, flow.0 + width);
    // An initial letter stands after the text indent of the first line, which the lines beside
    // it do not have again.
    let starts_with_letter = start == (0, 0)
        && pieces
            .iter()
            .find(|piece| !matches!(piece.kind, PieceKind::Open(_)))
            .is_some_and(|piece| {
                matches!(piece.kind, PieceKind::Float { .. }) && piece.style.initial_letter.is_some()
            });
    let indent = if starts_with_letter {
        0.0
    } else {
        container.text_indent.resolve(width.to_px())
    };

    // Inline boxes open where the content starts continue on the first line.
    let mut open: Vec<usize> = Vec::new();
    // Without inline boxes there are none (the pieces before may be a million).
    let before = if boxes.is_empty() { 0 } else { start.0 };
    for piece in pieces.iter().take(before) {
        match piece.kind {
            PieceKind::Open(index) => open.push(index),
            PieceKind::Close(_) => {
                open.pop();
            }
            _ => {}
        }
    }
    // Outside markers stand beside the list item's border box, not beside the lines.
    let insets = layouter
        .list_item_edges
        .get()
        .map(|(left, right)| (flow.0 - left, right - (flow.0 + width)));
    // The pieces still to lay out. Where the content goes on from a page before, they are taken
    // into the queue as the lines need them: the rest of a paragraph that goes on for hundreds of
    // pages (a `<pre>` with a log), of which only the lines of this page are laid out, would
    // otherwise be copied for each page.
    let mut later = Later {
        pieces: &pieces[start.0.min(pieces.len())..],
        insets,
        rtl: container.is_rtl(),
    };
    let mut queue: VecDeque<Piece> = VecDeque::new();
    // A pretty paragraph that goes on from an earlier page breaks its last lines anew there too
    // (unless it goes on for many pages, which would each break the rest of it again).
    let pretty = container.text_wrap_style == TextWrapStyle::Pretty
        && pieces.len().saturating_sub(start.0) <= MAX_PRETTY_PIECES;
    if start == (0, 0) || pretty {
        later.fill(&mut queue, usize::MAX);
    } else {
        later.fill(&mut queue, Later::AHEAD);
    }
    // The pieces after the first line, while it is laid out in its own style.
    let mut after_first = None;
    if let Some(first_pieces) = first_pieces {
        after_first = Some(std::mem::replace(&mut queue, first_pieces.into_iter().collect()));
    }
    let mut consumed = start.0;
    // Where the first piece of the queue starts in the text of its piece, after a word broke.
    let mut offset = 0;
    if start.1 > 0
        && let Some(front) = queue.front_mut()
        && matches!(front.kind, PieceKind::Text { .. })
    {
        *front = split_piece(fonts, &layouter.han_fonts, front, start.1).1;
        offset = start.1;
    }
    // `text-wrap: balance` and `pretty` limit how far lines reach, where no float takes room.
    let limits = if (start == (0, 0) || pretty)
        && layouter.free_range(left_edge, right_edge, flow.1, Au::MAX) == (left_edge, right_edge)
    {
        let indent = if start == (0, 0) { indent } else { 0.0 };
        score::wrap_limits(&queue, container, (width.to_px(), indent), strut_height.to_px())
    } else {
        Vec::new()
    };
    let mut lines: Vec<Line> = Vec::new();
    // The top of the next line, from the top of the first. The first goes below the initial
    // letters before it where it starts with one, else below those its `clear` names, as in
    // Blink (`InlineLayoutAlgorithm::Layout`).
    let mut y = if start == (0, 0) {
        let clear = if starts_with_letter {
            fagus_style::values::Clear::Both
        } else {
            container.clear
        };
        layouter
            .letter_clearance(clear)
            .map_or(Au::ZERO, |bottom| (bottom - flow.1).max(Au::ZERO))
    } else {
        Au::ZERO
    };
    // Where the content has to end on the page. Lines of plain text (or text with inline blocks
    // and tables) that start below it go to the next page, where they are laid out again: only
    // as many are laid out here as the widows need to be counted (and one more, for lines that
    // text-box-trim moves up), not all of a paragraph that may go on for hundreds of pages.
    let page_end = layouter.flow_limit().filter(|_| {
        budget.is_none()
            && owner.is_some()
            && !fitting.is_measuring()
            && kept_shaping(layouter.tree, items).is_some()
    });
    let mut below_page = 0_usize;
    // Floats that did not fit beside their line wait for the top of the next one.
    let mut waiting: Vec<PieceKind> = Vec::new();
    let place = |float: PieceKind, top: Au, placed: &mut Vec<Fragment>, bottom: &mut Au| {
        let PieceKind::Float {
            id,
            fragment: Some(fragment),
            size,
            side,
            clear,
        } = float
        else {
            return;
        };
        let top = layouter
            .clearance(clear)
            .map_or(flow.1 + top, |clear| clear.max(flow.1 + top));
        // A float that does not fit on the page moves to the next.
        let Some((rect, mut fragment)) =
            layouter.place_float_box(id, (*fragment, size), side, top, (left_edge, right_edge))
        else {
            return;
        };
        fragment.rect.x += rect.x - flow.0;
        fragment.rect.y += rect.y - flow.1;
        *bottom = (*bottom).max(rect.y + rect.height - flow.1);
        placed.push(fragment);
    };
    // Whether the line before ended at a forced break, after which a paragraph starts.
    let mut after_forced = false;
    // An initial letter whose text line has not been placed yet: the line it stands on and its
    // index among that line's floats. It moves down with its text line.
    let mut letter_at: Option<(usize, usize)> = None;
    // A line beside floats with shapes is laid out again in a band as tall as it came to where
    // that gives it another width, as in Blink (`InlineLayoutAlgorithm::Layout` re-tests the
    // opportunity with the line's block size): the band and the one it was laid out in before.
    let mut retry: Option<(Au, Au)> = None;
    let mut retries = 0_u32;
    while !queue.is_empty() {
        // What laying out the line again starts from.
        let shaped_float = |kind: &PieceKind| matches!(kind, PieceKind::Float { id, .. } if layouter.tree.get(*id).style.shape_outside.is_some());
        let snapshot = (layouter.shapes_below(flow.1 + y)
            || waiting.iter().any(shaped_float)
            || queue
                .iter()
                .chain(later.pieces.iter().take(Later::AHEAD))
                .any(|piece| shaped_float(&piece.kind)))
        .then(|| {
            (
                queue.clone(),
                later.clone(),
                waiting.clone(),
                layouter.float_count(),
                y,
            )
        });
        later.fill(&mut queue, Later::AHEAD);
        let first_line = consumed == 0 && offset == 0;
        if !first_line && !after_forced {
            spacing::start_line(&mut queue);
            // Kerning across the start of the line looks at the rest of its item.
            if kerning::starts_kerned(&queue) {
                later.fill(&mut queue, usize::MAX);
            }
            kerning::start_line(fonts, &mut queue);
        }
        let mut floats = Vec::new();
        let mut floats_bottom = Au::ZERO;
        // An initial letter placed on this line: its index among the floats and how many lines
        // it is raised above its text.
        let mut letter_here: Option<(usize, u32)> = None;
        for float in waiting.drain(..) {
            place(float, y, &mut floats, &mut floats_bottom);
        }
        // `each-line` indents the lines after forced breaks too, `hanging` the other lines.
        let indented = first_line || container.text_indent_each_line && after_forced;
        let line_indent = if indented == container.text_indent_hanging {
            0.0
        } else {
            indent
        };
        // The first unbreakable run must fit beside the floats, or the line moves down past them;
        // where the lines of the container do not wrap, the line stays and overflows, as in
        // Blink (`InlineLayoutAlgorithm::Layout` tries the next opportunity only for lines that
        // wrap).
        let wraps = container.white_space.wraps();
        later.take_run(&mut queue, 0);
        let needed = Au::from_px_ceil(first_run_width(&queue, 0) + line_indent);
        // The line is at least as tall as the atomic inlines of that run, and must have the room
        // all down them, as in Chrome: one that starts above a float lower down goes below it.
        let line_height = retry.map_or_else(
            || strut_height.max(Au::from_px(first_run_height(&queue, 0))),
            |(band, _)| band,
        );
        // The band of the line beside floats placed at its top: where floats with shapes may
        // be beside it, as tall as the line is taken to be.
        let band = if snapshot.is_some() {
            line_height
        } else {
            strut_height
        };
        let (mut left, mut right) = layouter.line_range(left_edge, right_edge, flow.1 + y, line_height);
        while wraps && right - left < needed && right - left < width {
            let Some(edge) = layouter.next_line_top(flow.1 + y) else {
                break;
            };
            y = edge - flow.1;
            (left, right) = layouter.line_range(left_edge, right_edge, flow.1 + y, line_height);
        }

        // Boxes that go on from the line before and clone their decorations start this line with
        // their start margin, border and padding (`box-decoration-break: clone`). Their end
        // ones, where they go on to the next line, may reach past it, as in Chrome.
        let start_frames: f32 = open
            .iter()
            .filter(|index| boxes[**index].style.clones_decorations())
            .map(|index| boxes[*index].margin.0 + boxes[*index].frame.0)
            .sum();
        // Greedy line breaking: break at the last opportunity that fits.
        let mut available = (right - left).to_px() - line_indent - start_frames;
        if let Some(limit) = limits.get(lines.len()) {
            available = available.min(limit - line_indent - start_frames);
        }
        // The boxes of the line's inline items, in the style of `::first-line` on the first.
        let line_boxes: &[InlineBox] = match (&first_builder, &after_first) {
            (Some(first_builder), Some(_)) => first_builder.boxes,
            _ => boxes,
        };
        // An opening bracket or quote that hangs at the start of the first formatted line
        // takes no room on it (CSS Text 3, §8.2).
        let start_hang = if first_line {
            hanging::start(queue.iter(), line_boxes)
        } else {
            0.0
        };
        available += start_hang;
        let mut current_width = 0.0;
        let mut since_break_width = 0.0;
        // The width of the part of the current inline item on the line so far.
        let mut item_width = 0.0;
        // What rounding the width of the inline item that the last piece ended added: it counts
        // for what follows on the line, not for the line ending there.
        let mut snap = 0.0;
        let mut end = 0;
        // The last break that fits other than at a hyphenation point, with the width before it.
        let mut before_word = (0, 0.0);
        let mut forced = false;
        // A break inside a word of a run that does not fit: a piece and an offset in its text.
        let mut inside = None;
        // A ruby column broken across this line and the next, with what it was before.
        let mut split_ruby: Option<(usize, Piece)> = None;
        // The floats met on this line (not initial letters), to take back where the line ends
        // before them.
        let mut met_floats: Vec<MetFloat> = Vec::new();
        // The pieces of the floats placed beside this line.
        let mut placed_floats: Vec<usize> = Vec::new();
        let mut index = 0;
        // The piece after the one looked at is in the queue too, if there is one.
        while later.get(&mut queue, index + 1).is_some() || index < queue.len() {
            let piece = &mut queue[index];
            if let PieceKind::Float { size, .. } = piece.kind {
                // Beside this line if it fits next to what the line holds, else below it. A
                // space that ends what the line holds hangs: it takes no room from the float,
                // as in Chrome (floats one after the other in the source, white space between
                // them, fill a line).
                let hanging = queue
                    .range(..index)
                    .rev()
                    .find(|piece| piece.width > 0.0)
                    .map_or(0.0, |piece| piece.trailing_space);
                let piece = &mut queue[index];
                // (What is left of it after rounding to layout units counts as nothing.)
                let used = current_width + since_break_width - hanging;
                let used = if used <= FIT_TOLERANCE { 0.0 } else { used };
                let letter = piece.style.initial_letter;
                let float = std::mem::replace(&mut piece.kind, PieceKind::Measured);
                if let Some(letter) = letter
                    && let PieceKind::Float {
                        fragment: Some(mut fragment),
                        side,
                        ..
                    } = float
                {
                    // An initial letter stands at the start of its line, beside the floats
                    // there all down the letter (Blink's line opportunity for a line with an
                    // initial letter is as tall as the letter).
                    let (start, end) =
                        layouter.line_range(left_edge, right_edge, flow.1 + y, strut_height.max(size.1));
                    let x = if side == fagus_style::values::Float::Right {
                        end - size.0
                    } else {
                        start
                    };
                    let rect = Rect::new(x, flow.1 + y, size.0, size.1);
                    layouter.place_initial_letter(side, rect);
                    fragment.rect.x += rect.x - flow.0;
                    fragment.rect.y += rect.y - flow.1;
                    floats_bottom = floats_bottom.max(y + size.1);
                    letter_here = Some((floats.len(), raised_lines(letter).min(1000.0) as u32));
                    floats.push(*fragment);
                    (left, right) = layouter.line_range(left_edge, right_edge, flow.1 + y, band);
                    // Where what comes first does not fit beside it, the line goes below it.
                    if used <= 0.0 {
                        later.take_run(&mut queue, index + 1);
                        let needed = Au::from_px_ceil(first_run_width(&queue, index + 1) + line_indent);
                        while wraps && right - left < needed && right - left < width {
                            let Some(edge) = layouter.next_line_top(flow.1 + y) else {
                                break;
                            };
                            y = edge - flow.1;
                            (left, right) = layouter.line_range(left_edge, right_edge, flow.1 + y, band);
                        }
                    }
                    available = (right - left).to_px() - line_indent + start_hang;
                    index += 1;
                    continue;
                }
                // A float before anything of the line (Blink's leading float) is placed where
                // it fits, even where that is not beside the line (`PositionLeadingFloats`), and
                // the line goes beside or below it; one after content goes beside the line if it
                // fits next to that content.
                let room = if used <= 0.0 {
                    f32::INFINITY
                } else {
                    available - used
                };
                met_floats.push(MetFloat {
                    index,
                    kind: float.clone(),
                    placed: (layouter.float_count(), floats.len(), floats_bottom),
                    waiting: waiting.len(),
                });
                if waiting.is_empty() && size.0.to_px() <= room + 0.01 {
                    placed_floats.push(index);
                    place(float, y, &mut floats, &mut floats_bottom);
                    (left, right) = layouter.line_range(left_edge, right_edge, flow.1 + y, band);
                    // A float that starts the line stays at its top, but where it leaves too
                    // little room for what comes first, the line moves down past floats until
                    // that fits, as in Chrome: a box as wide as the line goes below the float.
                    // Not past the end of the page or column where a float goes on (the line
                    // stays beside it); below floats that all end there where a float moved on
                    // to the next page, the floats end a line of their own on the page, and what
                    // follows goes on to the next page, beside the floats that moved there
                    // (Chrome 157, measured).
                    let mut floats_alone = false;
                    if used <= 0.0 {
                        later.take_run(&mut queue, index + 1);
                        let needed = Au::from_px_ceil(first_run_width(&queue, index + 1) + line_indent);
                        let limit = layouter.flow_limit();
                        while wraps && right - left < needed && right - left < width {
                            let Some(edge) = layouter.next_line_top(flow.1 + y) else {
                                break;
                            };
                            if let Some(limit) = limit.filter(|limit| edge >= *limit) {
                                floats_alone =
                                    layouter.floats_continue() && !layouter.floats_reach_past(limit);
                                break;
                            }
                            y = edge - flow.1;
                            (left, right) = layouter.line_range(left_edge, right_edge, flow.1 + y, band);
                        }
                    }
                    if floats_alone {
                        end = index + 1;
                        index += 1;
                        break;
                    }
                    available = (right - left).to_px() - line_indent + start_hang;
                } else {
                    waiting.push(float);
                }
                index += 1;
                continue;
            }
            // A ruby column that does not fit breaks across lines where it may.
            let room = available - current_width - since_break_width;
            if piece.width > room + FIT_TOLERANCE
                && split_ruby.is_none()
                && container.text_wrap_style == TextWrapStyle::Auto
                && let PieceKind::Atomic { ruby: Some(part), .. } = &piece.kind
                && let Some([first, rest]) = part.split(layouter, room)
            {
                let original = piece.clone();
                let [first, rest] = [first, rest].map(|(atomic, part)| Piece {
                    width: atomic.width.to_px(),
                    min_width: atomic.width.to_px(),
                    kind: PieceKind::Atomic {
                        fragment: Box::new(atomic.fragment),
                        height: atomic.height.to_px(),
                        baseline: atomic.baseline.to_px(),
                        ruby: part.breaks().then(|| Box::new(part)),
                    },
                    ..original.clone()
                });
                queue[index] = Piece {
                    break_after: true,
                    ..first
                };
                queue.insert(index + 1, rest);
                split_ruby = Some((index, original));
            }
            // The last word of the paragraph is hyphenated only where it starts the line.
            let after_words = end > 0 && !queue[end - 1].last_word_hyphenation;
            let continues = queue
                .get(index + 1)
                .is_some_and(|next| same_item(&queue[index], next));
            let piece = &mut queue[index];
            if !piece.style.white_space.collapses_spaces() {
                expand_tabs(piece, line_indent + current_width + since_break_width, tab_stops);
            }
            since_break_width += piece.width;
            snap = 0.0;
            if matches!(piece.kind, PieceKind::Text { .. }) {
                item_width += piece.width;
                if !continues {
                    snap = snapped_width(item_width) - item_width;
                    since_break_width += snap;
                    item_width = 0.0;
                }
            } else {
                item_width = 0.0;
            }
            let breakable = piece.break_after && !(piece.last_word_hyphenation && after_words);
            if breakable || piece.forced_break_after {
                let used = current_width + since_break_width - snap - piece.trailing_space
                    + piece.hyphen
                    + broken_delta(piece);
                let used = used - spacing::end_autospace(&queue, index);
                // Punctuation that hangs at the end of the line takes no room on it.
                let hang = if queue[index].hyphen == 0.0 && queue[index].broken.is_none() {
                    hanging::end(queue.range(..=index).rev(), line_boxes)
                } else {
                    None
                };
                let hang = match hang {
                    Some(hang) if hang.kind == hanging::EndKind::Close => {
                        // A closing bracket or quote, at the end of the last formatted line.
                        later.fill(&mut queue, usize::MAX);
                        if hanging::nothing_after(queue.range(index + 1..)) {
                            hang.width
                        } else {
                            0.0
                        }
                    }
                    Some(hang) => hang.width,
                    None => 0.0,
                };
                let used = used - hang;
                let mut fits = used <= available + FIT_TOLERANCE;
                // Closing punctuation that does not fit loses its blank half where it then fits
                // and is the only character that does not.
                if !fits
                    && let Some((text, trim, glyph)) = spacing::end_trim(&queue, index)
                    && used - trim <= available + FIT_TOLERANCE
                    && used - glyph < available + FIT_TOLERANCE
                {
                    spacing::trim_end(&mut queue, text);
                    since_break_width -= trim;
                    fits = true;
                }
                // Spaces that hang at the end of the line add nothing to it: after a box that
                // overflows the line on its own, they stay on its line, as in Chrome, and do not
                // make a line of their own. A zero width space before them is no space that
                // hangs: the line breaks before it, as in Chrome.
                if !fits
                    && end > 0
                    && since_break_width - snap - queue[index].trailing_space <= FIT_TOLERANCE
                    && only_spaces(queue.range(end..=index))
                {
                    fits = true;
                }
                if !fits {
                    // The run since the last opportunity breaks inside a word, or before it.
                    inside = break_run(fonts, &queue, end..index + 1, available - current_width, end == 0);
                    // A word whose letters fit, only the two or more punctuation characters
                    // after them not, is not hyphenated but moves to the next line whole, as in
                    // Chrome on macOS (`CFStringGetHyphenationLocationBeforeIndex` finds no
                    // hyphenation point there; after one such character it does).
                    if inside.is_none()
                        && end > before_word.0
                        && before_word.0 > 0
                        && queue[index].hyphen == 0.0
                        && trailing_punctuation(&queue[index])
                            .is_some_and(|width| used - width <= available + FIT_TOLERANCE)
                    {
                        (end, current_width) = before_word;
                    }
                    if inside.is_some() || end > 0 {
                        break;
                    }
                }
                current_width += since_break_width;
                since_break_width = 0.0;
                end = index + 1;
                if !hyphenation_point(&queue[index]) {
                    before_word = (end, current_width);
                }
                if queue[index].forced_break_after {
                    forced = true;
                    break;
                }
            }
            index += 1;
        }
        // The pieces after the last break opportunity stay if they fit, or if nothing else does.
        if inside.is_none() && index >= queue.len() && end < queue.len() {
            let trailing = queue.back().map_or(0.0, |piece| piece.trailing_space);
            let hang = hanging::end(queue.iter().rev(), line_boxes).map_or(0.0, |hang| hang.width);
            if current_width + since_break_width - snap - trailing - hang <= available + FIT_TOLERANCE {
                end = queue.len();
            } else {
                inside = break_run(
                    fonts,
                    &queue,
                    end..queue.len(),
                    available - current_width,
                    end == 0,
                );
                if inside.is_none() && end == 0 {
                    end = queue.len();
                }
            }
        }
        // Beside floats with shapes, the line is laid out again in a band as tall as it comes to
        // where that changes its width: but where it goes back and forth between two heights,
        // only where the width shrinks (Blink).
        if let Some((pieces, rest, floats_waiting, count, top)) = snapshot {
            let last = inside.map_or(end, |(piece, _)| piece + 1);
            let height = strut_height.max(Au::from_px(line_atomic_height(&queue, last)));
            let (start, stop) = layouter.line_range(left_edge, right_edge, flow.1 + y, height);
            let (new, old) = (stop - start, right - left);
            let again = if retry.is_some_and(|(_, before)| before == height) {
                new < old
            } else {
                new != old
            };
            if again && retries < 8 {
                retries += 1;
                retry = Some((height, line_height));
                queue = pieces;
                later = rest;
                waiting = floats_waiting;
                layouter.truncate_floats(count);
                y = top;
                continue;
            }
        }
        retry = None;
        retries = 0;
        // A float placed beside the line right where it ends, after the spaces that hang at its
        // end, stays beside it, as in Chrome.
        if inside.is_none() {
            while end < queue.len() && placed_floats.contains(&end) {
                end += 1;
            }
        }
        // Floats after where the line ends were met only on the way to its end: they are taken
        // back and met again on the next line, beside it if they fit next to what comes before
        // them there, as in Blink (`LineBreaker::RewindFloats`).
        let kept = match inside {
            Some((piece, 0)) => piece,
            Some((piece, _)) => piece + 1,
            None => end,
        };
        if let Some(first) = met_floats.iter().position(|met| met.index >= kept) {
            let (count, placed, bottom) = met_floats[first].placed;
            layouter.truncate_floats(count);
            floats.truncate(placed);
            floats_bottom = bottom;
            waiting.truncate(met_floats[first].waiting);
            for met in met_floats.drain(first..) {
                queue[met.index].kind = met.kind;
            }
        }
        // A ruby column broken for this line goes on the next whole where the line ends
        // before it.
        if let Some((at, original)) = split_ruby.take() {
            let ends = match inside {
                Some((piece, 0)) => piece,
                Some((piece, _)) => piece + 1,
                None => end,
            };
            if ends <= at {
                queue[at] = original;
                queue.remove(at + 1);
            }
        }
        match inside {
            // Between two pieces: before the inline boxes that open there.
            Some((piece, 0)) => {
                end = piece;
                while end > 1 && matches!(queue[end - 1].kind, PieceKind::Open(_)) {
                    end -= 1;
                }
                inside = None;
            }
            Some((piece, at)) => {
                let (first, rest) = split_piece(fonts, &layouter.han_fonts, &queue[piece], at);
                queue[piece] = first;
                queue.insert(piece + 1, rest);
                end = piece + 1;
            }
            // A word that breaks where it was not safe to break is shaped again there.
            None if !forced && end > 0 && end < queue.len() => {
                inside = take_broken(fonts, &layouter.han_fonts, &mut queue, end)
                    .filter(|into| *into > 0)
                    .map(|into| (end, into));
            }
            None => {}
        }
        // A `<br>` with `clear` that ends the line moves what follows below the floats it names.
        let clear_after = if forced && end > 0 {
            queue[end - 1].break_clear
        } else {
            fagus_style::values::Clear::None
        };
        let mut line_pieces: Vec<Piece> = queue
            .drain(..end)
            .filter(|piece| !matches!(piece.kind, PieceKind::Float { .. } | PieceKind::Measured))
            .collect();
        later.fill(&mut queue, Later::AHEAD);
        if inside.is_none() && !forced {
            rehyphenate(fonts, &layouter.han_fonts, line_pieces.last(), &mut queue);
        }
        spacing::end_line(&mut line_pieces);
        if let Some((piece, at)) = inside {
            consumed += piece;
            offset = if piece == 0 { offset + at } else { at };
        } else {
            consumed += end;
            offset = 0;
        }
        // A line that ends at a hyphenation point shows a hyphen, inside the inline boxes that
        // end there.
        if !queue.is_empty()
            && !forced
            && line_pieces.last().is_some_and(|last| last.hyphen > 0.0)
            && let Some(text) = line_pieces
                .iter()
                .rposition(|piece| !matches!(piece.kind, PieceKind::Close(_)))
        {
            let hyphen = hyphen_piece(fonts, &line_pieces[text]);
            let after = line_pieces.split_off(text + 1);
            line_pieces.extend(hyphen);
            line_pieces.extend(after);
        }
        let continuing = open.clone();
        for piece in &line_pieces {
            match piece.kind {
                PieceKind::Open(index) => open.push(index),
                PieceKind::Close(_) => {
                    open.pop();
                }
                _ => {}
            }
        }
        let last = queue.is_empty() || forced;
        let formatted = Formatted {
            first: first_line,
            last: later.pieces.is_empty() && hanging::nothing_after(queue.iter()),
        };
        after_forced = forced;
        // The last line `line-clamp` shows ends with an ellipsis where more follows.
        let clamped = (!queue.is_empty() || layouter.clamp_follows.get())
            && budget.is_some_and(|budget| lines.len() + 1 == budget);
        let in_first_style = first_builder.is_some() && after_first.is_some();
        let line_builder = match (&first_builder, &after_first) {
            (Some(first_builder), Some(_)) => first_builder,
            _ => &builder,
        };
        // An outside marker beside the floats placed before the content alone.
        if first_line && outside_marker {
            let (start_edge, end_edge) =
                layouter.line_range_among(floats_before, left_edge, right_edge, flow.1 + y, strut_height);
            for piece in &mut line_pieces {
                if let PieceKind::Marker {
                    start, inside: false, ..
                } = &mut piece.kind
                {
                    *start += if container.is_rtl() {
                        (right - end_edge).to_px()
                    } else {
                        (start_edge - left).to_px()
                    };
                }
            }
        }
        // `text-fit` scales the text of the line, and the line with it.
        let scale = fitting.line_scale(
            &line_pieces,
            line_indent,
            (right - left).to_px(),
            queue.is_empty(),
        );
        let mut line = if (scale - 1.0).abs() > f32::EPSILON {
            let scaled = fit::scale_line(
                &mut line_pieces,
                line_builder.boxes,
                line_builder.container,
                scale,
            );
            LineBuilder {
                container: &scaled.container,
                boxes: &scaled.boxes,
                strut: metrics(fonts, &scaled.container),
                ..*line_builder
            }
            .build(
                line_pieces,
                &continuing,
                (right - left).to_px(),
                line_indent,
                (last, clamped),
                formatted,
            )
        } else {
            line_builder.build(
                line_pieces,
                &continuing,
                (right - left).to_px(),
                line_indent,
                (last, clamped),
                formatted,
            )
        };
        line.text_scale = scale;
        if in_first_style && let (Some(pseudo), Some(first_container)) = (pseudo_first_line, &first_container)
        {
            first_line_box(fonts, &mut line.fragment, pseudo, first_container);
        }
        crate::ruby::share_annotation_baselines(&mut line.fragment);
        line.fragment.rect.x = left - flow.0;
        // Room for the annotations of the line over and under it moves it and the lines after
        // it down; an initial letter moves with its text line.
        let (mut shift, mut after) = (Au::ZERO, Au::ZERO);
        if let Some(spacer) = &mut spacer {
            if let Some((index, raised)) = letter_here {
                letter_at = Some((lines.len(), index));
                spacer.raise(raised);
            }
            let filler = spacer.raising();
            let (over, under) = spacer.place(fonts, &line.fragment, line.height.to_px());
            (shift, after) = (Au::from_px(over), Au::from_px(under));
            // The letter moves with its text line, and up with the lines it raises the text
            // by where they take no room.
            let letter_move = if filler { after.min(Au::ZERO) } else { shift };
            let pending = if filler { letter_at } else { letter_at.take() };
            if let Some((at, index)) = pending {
                let letter = if at == lines.len() {
                    floats.get_mut(index).map(|letter| (letter, &mut floats_bottom))
                } else {
                    lines.get_mut(at).and_then(|line| {
                        line.floats
                            .get_mut(index)
                            .map(|letter| (letter, &mut line.floats_bottom))
                    })
                };
                if let Some((letter, bottom)) = letter
                    && letter_move != Au::ZERO
                {
                    letter.rect.y += letter_move;
                    *bottom = (*bottom).max(letter.rect.y + letter.rect.height);
                    layouter.move_initial_letter(letter_move);
                }
            }
        }
        line.top = y + shift;
        line.end = (consumed, offset);
        line.annotations_under = after.max(Au::ZERO);
        line.annotations_over = shift.max(Au::ZERO);
        y = line.top + line.height + after;
        // As Blink's `AddAnyClearanceAfterLine`: unlike `clear` on a block, this adds room after
        // the content, so that the next line and the end of the container come below the floats.
        if let Some(bottom) = layouter.clearance(clear_after)
            && bottom > flow.1 + y
        {
            line.clearance_after = bottom - flow.1 - y;
            y = bottom - flow.1;
        }
        line.floats = floats;
        line.floats_bottom = floats_bottom;
        line.float_count = layouter.float_count();
        let line_top = flow.1 + line.top;
        lines.push(line);
        if let Some(page_end) = page_end
            && line_top >= page_end
        {
            below_page += 1;
            if below_page > container.widows as usize + 1 {
                break;
            }
        }
        // After the first line, the rest goes on in its own style, from where the first ended.
        if let Some(mut rest) = after_first.take() {
            if let Some(map) = &first_map {
                (consumed, offset) = map.position(consumed, offset);
                if let Some(line) = lines.last_mut() {
                    line.end = (consumed, offset);
                }
            }
            rest.drain(..consumed.min(rest.len()));
            if offset > 0
                && let Some(front) = rest.front_mut()
                && let PieceKind::Text { text, .. } = &front.kind
                && text.is_char_boundary(offset)
            {
                *front = split_piece(fonts, &layouter.han_fonts, front, offset).1;
            }
            queue = rest;
        }
    }
    // Floats after the last line go below it.
    if !waiting.is_empty() {
        let mut floats = Vec::new();
        let mut floats_bottom = Au::ZERO;
        for float in waiting.drain(..) {
            place(float, y, &mut floats, &mut floats_bottom);
        }
        match lines.last_mut() {
            Some(line) => {
                line.floats.extend(floats);
                line.floats_bottom = line.floats_bottom.max(floats_bottom);
                line.float_count = layouter.float_count();
            }
            None => lines.push(Line {
                height: Au::ZERO,
                fragment: Fragment {
                    rect: Rect::new(Au::ZERO, Au::ZERO, width, Au::ZERO),
                    kind: FragmentKind::Line { baseline: Au::ZERO },
                    children: Children::new(),
                },
                top: y,
                end: (consumed, offset),
                floats,
                floats_bottom,
                float_count: layouter.float_count(),
                clearance_after: Au::ZERO,
                annotations_under: Au::ZERO,
                annotations_over: Au::ZERO,
                phantom: true,
                text_scale: 1.0,
            }),
        }
    }
    lines
}

/// The most pieces a pretty paragraph that goes on from an earlier page may have left to break
/// its last lines anew there.
const MAX_PRETTY_PIECES: usize = 20_000;

/// Where the pieces of the first line in the style of `::first-line` and the pieces of the
/// paragraph in its own style hold the same content: for each piece (and the end), the number
/// of pieces that are not text before it and the bytes of text before it.
struct FirstLineMap {
    first: Vec<(usize, usize)>,
    own: Vec<(usize, usize)>,
}

impl FirstLineMap {
    /// The map between `first` and `own`, if they hold the same text and as many other pieces.
    fn new(first: &[Piece], own: &[Piece]) -> Option<Self> {
        fn starts(pieces: &[Piece]) -> (Vec<(usize, usize)>, String) {
            let mut text = String::new();
            let mut others = 0;
            let mut starts = Vec::with_capacity(pieces.len() + 1);
            for piece in pieces {
                starts.push((others, text.len()));
                match &piece.kind {
                    PieceKind::Text { text: piece_text, .. } => text.push_str(piece_text),
                    _ => others += 1,
                }
            }
            starts.push((others, text.len()));
            (starts, text)
        }
        let ((first, first_text), (own, own_text)) = (starts(first), starts(own));
        (first_text == own_text && first.last() == own.last()).then_some(Self { first, own })
    }

    /// The piece of the paragraph's own and the byte offset in its text where the first line,
    /// which ends at `offset` bytes into its piece `consumed`, ends.
    fn position(&self, consumed: usize, offset: usize) -> (usize, usize) {
        let Some(&(others, bytes)) = self.first.get(consumed).or(self.first.last()) else {
            return (consumed, offset);
        };
        let key = (others, bytes + offset);
        let pieces = self.own.len() - 1;
        // A piece that starts there, or else the text it lies in.
        let at = self.own[..pieces].partition_point(|&start| start < key);
        if self.own.get(at) == Some(&key) {
            return (at, 0);
        }
        let inside = at.saturating_sub(1);
        (inside, key.1.saturating_sub(self.own[inside].1))
    }
}

/// Puts the box of `::first-line` (style `pseudo`) behind the content of the first line,
/// where it has a background (a colour or an image, not the layers of `none` it has without
/// one): an inline box around that content, as tall as the content area of the font of the
/// first line (`first`), as Chrome paints it.
fn first_line_box(fonts: &FontDb, line: &mut Fragment, pseudo: &ComputedStyle, first: &ComputedStyle) {
    if !crate::tree::has_background(pseudo) {
        return;
    }
    let FragmentKind::Line { baseline } = line.kind else {
        return;
    };
    let (left, right) = line
        .children
        .iter()
        .filter(|child| {
            matches!(
                child.kind,
                FragmentKind::Text(_) | FragmentKind::Box { .. } | FragmentKind::Anonymous
            )
        })
        .fold((Au::MAX, -Au::MAX), |(left, right), child| {
            (left.min(child.rect.x), right.max(child.rect.x + child.rect.width))
        });
    if left >= right {
        return;
    }
    let font = metrics(fonts, first);
    let top = baseline - Au::from_px(font.ascent);
    let height = Au::from_px(font.ascent + font.descent);
    let style = Arc::new(ComputedStyle::from(ComputedValues {
        display: fagus_style::values::Display::INLINE,
        ..ComputedValues::from(pseudo.clone())
    }));
    line.children.insert(
        0,
        Fragment {
            rect: Rect::new(left, top, right - left, height),
            kind: FragmentKind::Box {
                style,
                node: None,
                sides: VisibleSides::ALL,
            },
            children: Children::new(),
        },
    );
}

/// The style of what lies on the first line of a block with style `block`: where it inherits
/// the font and text properties from the block, it takes those of `::first-line`
/// (CSS Pseudo-Elements 4, §2.1).
#[allow(clippy::float_cmp, reason = "an inherited value is the very same")]
pub(crate) fn restyled(
    style: &ComputedStyle,
    block: &ComputedStyle,
    first_line: &ComputedStyle,
) -> ComputedStyle {
    let mut restyled = style.clone();
    macro_rules! inherit {
        ($($field:ident),+) => {
            $(if style.$field == block.$field {
                restyled.$field = first_line.$field.clone();
            })+
        };
    }
    inherit!(
        color,
        font_family,
        font_size,
        font_weight,
        font_style,
        font_stretch,
        font_variant,
        font_feature_settings,
        font_variation_settings,
        font_synthesis,
        font_kerning,
        font_size_adjust,
        letter_spacing,
        word_spacing,
        text_transform,
        line_height,
        text_decoration_line,
        text_decoration_style,
        text_decoration_color
    );
    restyled
}

/// What inline content is shaped into pieces for.
#[derive(Clone, Copy)]
enum Purpose<'s> {
    /// Lines.
    Layout,
    /// Intrinsic widths, in a box whose lines wrap or not, if known, and of a style, if known,
    /// which the initial letter of the content is sized for.
    Measure(Option<bool>, Option<&'s ComputedStyle>),
}

/// Splits the text of the items at break opportunities, shapes every piece, and lays out the
/// atomic inlines (or, when only measuring, takes their intrinsic widths). `width` is the width
/// of the containing block.
fn shape_pieces(
    layouter: &Layouter<'_>,
    (items, container): (&[InlineItem], Option<&ComputedStyle>),
    width: Au,
    purpose: Purpose<'_>,
) -> (Vec<Piece>, Vec<InlineBox>) {
    let measure = matches!(purpose, Purpose::Measure(..));
    // The style an initial letter is sized for.
    let letter_block = match purpose {
        Purpose::Measure(_, block) => block,
        Purpose::Layout => container,
    };
    let fonts = layouter.fonts;
    let basis = width.to_px();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut boxes: Vec<InlineBox> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    // The text of generated items, and the bidi levels of all items, in the direction of the
    // container (when measuring, of the content).
    let generated: Vec<Option<String>> = items.iter().map(|item| generated_text(layouter, item)).collect();
    let levels = container
        .or_else(|| first_style(items).map(|style| &**style))
        .and_then(|block| {
            crate::bidi::levels(
                block,
                items
                    .iter()
                    .zip(&generated)
                    .map(|(item, text)| bidi_content(item, text.as_deref())),
            )
        });
    let levels_of = |index: usize| {
        levels
            .as_ref()
            .map_or(TextLevels::LTR, |levels| levels.item(index))
    };
    let paragraph = Paragraph::new(layouter.tree, items, &generated);
    let ends_wrap = ends_wrap(items, container);
    // The text around each item that its letters join with.
    let around = if fagus_text::has_joining(&paragraph.text) {
        context::of_items(items, &generated, |earlier, later| {
            let (InlineItem::Text { text, style }, InlineItem::Text { style: other, .. }) =
                (&items[earlier], &items[later])
            else {
                return false;
            };
            // In vertical lines of mixed orientation, Blink shapes each item by itself (its
            // items are segments by orientation of their own), with the text around as context.
            let mixed = |style: &ComputedStyle| {
                style.is_vertical()
                    && !style.writing_mode.is_sideways()
                    && style.text_orientation == fagus_style::values::TextOrientation::Mixed
            };
            same_font(fonts, style, other)
                && !mixed(style)
                && levels_of(earlier).at(text.len().saturating_sub(1)) == levels_of(later).at(0)
        })
    } else {
        Vec::new()
    };
    // Whether lines wrap in the content of the paragraph's box itself.
    let known = match purpose {
        Purpose::Measure(wraps, _) => wraps,
        Purpose::Layout => None,
    };
    let paragraph_wraps = known.unwrap_or_else(|| {
        container
            .or_else(|| first_style(items).map(|style| &**style))
            .is_none_or(|style| style.white_space.wraps())
    });
    for (index, item) in items.iter().enumerate() {
        let level = levels_of(index).at(0);
        // Whether lines wrap in the inline box the item is in.
        let wraps = open
            .last()
            .map_or(paragraph_wraps, |&open| boxes[open].style.white_space.wraps());
        let at = paragraph.ranges[index].start;
        match item {
            InlineItem::LineBreak { style } => match pieces.last_mut() {
                // A break ends the line of what precedes it, or, at the start, after another
                // break or after a float alone, an empty line of its own.
                Some(last)
                    if !last.forced_break_after
                        && !matches!(
                            last.kind,
                            PieceKind::Float { .. } | PieceKind::Measured | PieceKind::Placeholder(_)
                        ) =>
                {
                    last.forced_break_after = true;
                    last.break_after = true;
                    last.break_clear = style.clear;
                }
                _ => pieces.push(Piece {
                    bidi: level,
                    break_clear: style.clear,
                    ..empty_piece(fonts, style)
                }),
            },
            InlineItem::Marker {
                text,
                style,
                symbol,
                inside,
                image,
            } => {
                let Some(face) = face_for(fonts, style) else {
                    continue;
                };
                // An image marker is as large as the image, or without a size a square of half
                // the font's rounded ascent, with 7 pixels between it and the content, as in
                // Blink; the image of a marker's `content` without them.
                if let Some(image) = image {
                    let bullet = (fonts.face(face).rounded_ascent_descent(style.font_size).0 / 2.0).floor();
                    let (image_width, image_height) = match image.natural {
                        (true, true) => (image.width, image.height),
                        _ => (bullet, bullet),
                    };
                    let gap = if matches!(style.content, fagus_style::values::Content::Normal) {
                        7.0
                    } else {
                        0.0
                    };
                    let (start, room) = if *inside {
                        (0.0, image_width + gap)
                    } else {
                        (-(image_width + gap), 0.0)
                    };
                    pieces.push(Piece {
                        kind: PieceKind::Marker {
                            runs: Vec::new(),
                            start,
                            inside: *inside,
                            image: Some((Arc::clone(image), image_width, image_height)),
                            symbol: None,
                        },
                        style: Arc::clone(style),
                        width: room,
                        min_width: 0.0,
                        trailing_space: 0.0,
                        break_after: false,
                        forced_break_after: false,
                        break_clear: fagus_style::values::Clear::None,
                        hyphen: 0.0,
                        last_word_hyphenation: false,
                        bidi: level,
                        edges: spacing::Edges::default(),
                        broken: None,
                    });
                    continue;
                }
                let mut runs: Vec<(FaceId, Vec<Glyph>, String, f32, Caps)> =
                    crate::bidi::visual_runs(text, style.is_rtl())
                        .into_iter()
                        .flat_map(|(range, own)| {
                            let level = Level {
                                level: own,
                                paragraph: u8::from(style.is_rtl()),
                            };
                            // The pieces of a run, one for each face, in logical order: right to
                            // left, the last stands leftmost.
                            let mut pieces =
                                shape_segment(fonts, style, face, &text[range], TextLevels::uniform(level));
                            if level.is_rtl() {
                                pieces.reverse();
                            }
                            pieces
                        })
                        // The tabs of an outside marker, which keeps them (`white-space: pre`),
                        // reach the tab stops of its own box, as in Chrome.
                        .scan(0.0, |pen, mut piece| {
                            if !style.white_space.collapses_spaces() && !*inside {
                                expand_tabs(&mut piece, *pen, tab_stops(fonts, style));
                            }
                            *pen += piece.width;
                            Some(piece)
                        })
                        .filter_map(|piece| match piece.kind {
                            PieceKind::Text {
                                face,
                                glyphs,
                                text,
                                caps,
                            } => Some((face, glyphs, text, piece.width, caps)),
                            _ => None,
                        })
                        .collect();
                let width: f32 = runs.iter().map(|run| run.3).sum();
                // Where the marker's text starts, and the room it takes on the line.
                let (start, room, symbol) = match symbol {
                    Some(shape) => {
                        let (start, room, symbol) = symbol_marker(fonts, style, *shape, *inside);
                        // One run as wide as the symbol's box, in the marker's first font (whose
                        // metrics alone count for the line), without glyphs.
                        runs.truncate(1);
                        if let Some(run) = runs.first_mut() {
                            *run = (face, Vec::new(), std::mem::take(&mut run.2), symbol.0, run.4);
                        }
                        (start, room, Some(symbol.1))
                    }
                    // Outside, the marker's box is as wide as its text rounded up to layout
                    // units, as Blink's, and ends where the content starts.
                    None if !*inside => (-snapped_width(width), 0.0, None),
                    None => (-width, 0.0, None),
                };
                // Lines break after a symbol inside the box, as in Chrome (see
                // `BoxTree::marker` for other markers).
                let break_after = *inside && wraps && symbol.is_some();
                pieces.push(Piece {
                    kind: PieceKind::Marker {
                        runs,
                        start,
                        inside: *inside,
                        image: None,
                        symbol,
                    },
                    style: Arc::clone(style),
                    width: room,
                    min_width: 0.0,
                    trailing_space: 0.0,
                    break_after,
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    bidi: level,
                    edges: spacing::Edges::default(),
                    broken: None,
                });
            }
            InlineItem::OutOfFlow(id) => {
                layouter.note_inline_static(*id, level.level % 2 == 1);
                let style = pieces
                    .last()
                    .map(|piece| Arc::clone(&piece.style))
                    .or_else(|| first_style(items).cloned())
                    .unwrap_or_default();
                // A line may break after the box where it may break before it, so that it stays
                // at the end of the line before: after text that collapsed to nothing where the
                // text allows a break there, else where the line may break after what comes
                // before (Blink's `HandleOutOfFlowPositioned`).
                let break_after = if layouter.tree.after_collapsed.contains(id) {
                    wraps && paragraph.breaks_at(at)
                } else {
                    pieces
                        .last()
                        .is_some_and(|last| last.break_after && !last.forced_break_after)
                };
                pieces.push(Piece {
                    kind: PieceKind::Placeholder(*id),
                    style,
                    width: 0.0,
                    min_width: 0.0,
                    trailing_space: 0.0,
                    break_after,
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    bidi: level,
                    edges: spacing::Edges::default(),
                    broken: None,
                });
            }
            InlineItem::Open { style, node } => {
                let index = boxes.len();
                // Blink keeps an inline box's margins and padding in layout units (1/64 px),
                // cut toward zero from the computed pixels (`LayoutUnit(float)`), which counts
                // where an atomic inline is as wide as its content.
                let units = |px: f32| (px * 64.0).trunc() / 64.0;
                let margin = |value: &LengthPercentageAuto| units(value.resolve_or(basis, 0.0));
                let padding = |value: &LengthPercentage| units(value.resolve(basis));
                let inline_box = InlineBox {
                    style: Arc::clone(style),
                    node: *node,
                    margin: (margin(&style.margin.left), margin(&style.margin.right)),
                    frame: (
                        style.border_width.left + padding(&style.padding.left),
                        style.border_width.right + padding(&style.padding.right),
                    ),
                    frame_top: style.border_width.top + padding(&style.padding.top),
                    frame_bottom: style.border_width.bottom + padding(&style.padding.bottom),
                };
                // The box's start is on its right in right-to-left text.
                let start = if style.is_rtl() {
                    inline_box.margin.1 + inline_box.frame.1
                } else {
                    inline_box.margin.0 + inline_box.frame.0
                };
                boxes.push(inline_box);
                open.push(index);
                pieces.push(Piece {
                    kind: PieceKind::Open(index),
                    style: Arc::clone(style),
                    width: start,
                    min_width: start,
                    trailing_space: 0.0,
                    // Glued to what follows.
                    break_after: false,
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    bidi: level,
                    edges: spacing::Edges::default(),
                    broken: None,
                });
            }
            InlineItem::Close => {
                let Some(index) = open.pop() else { continue };
                let inline_box = &boxes[index];
                // A break after the box's last piece moves behind the box's end, with the hyphen
                // a break there shows.
                let (moved, trailing_space, hyphen, last_word_hyphenation) = match pieces.last_mut() {
                    Some(last) if !last.forced_break_after => {
                        let moved = (
                            last.break_after,
                            last.trailing_space,
                            last.hyphen,
                            last.last_word_hyphenation,
                        );
                        last.break_after = false;
                        moved
                    }
                    _ => (false, 0.0, 0.0, false),
                };
                // Where a box whose lines do not wrap ends in one whose lines do, the line may
                // break at its end where the text of the paragraph allows, though not after a
                // space (as Blink's `HandleCloseTag` decides).
                let parent_wraps = open
                    .last()
                    .map_or(paragraph_wraps, |&open| boxes[open].style.white_space.wraps());
                let break_after = moved
                    || parent_wraps
                        && !inline_box.style.white_space.wraps()
                        && paragraph.breaks_at(at)
                        && !paragraph.text[..at].ends_with([' ', '\t']);
                let end = if inline_box.style.is_rtl() {
                    inline_box.margin.0 + inline_box.frame.0
                } else {
                    inline_box.margin.1 + inline_box.frame.1
                };
                pieces.push(Piece {
                    kind: PieceKind::Close(index),
                    style: Arc::clone(&inline_box.style),
                    width: end,
                    min_width: end,
                    trailing_space,
                    break_after,
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: if moved { hyphen } else { 0.0 },
                    last_word_hyphenation: moved && last_word_hyphenation,
                    bidi: level,
                    edges: spacing::Edges::default(),
                    broken: None,
                });
            }
            InlineItem::Float(id) => {
                let style = Arc::clone(&layouter.tree.get(*id).style);
                let wraps_itself = style.white_space.wraps();
                let (kind, piece_width, min_width) = if measure {
                    // For intrinsic widths a float counts like an unbreakable piece on the line,
                    // no narrower than nothing where negative margins pull it in (as in Chrome);
                    // an initial letter as wide as it is set.
                    let (min, max) = letter_block
                        .zip(style.initial_letter)
                        .and_then(|(container, lines)| {
                            initial_letter::initial_letter(layouter, *id, container, lines, basis)
                        })
                        .map_or_else(|| layouter.intrinsic(*id), |letter| (letter.width, letter.width));
                    let (min, max) = (min.max(Au::ZERO), max.max(Au::ZERO));
                    if let Some(last) = pieces.last_mut()
                        && !matches!(last.kind, PieceKind::Open(_))
                    {
                        last.break_after = last.style.white_space.wraps();
                    }
                    (PieceKind::Measured, max.to_px(), min.to_px())
                } else {
                    let atomic = container
                        .zip(style.initial_letter)
                        .and_then(|(container, lines)| {
                            initial_letter::initial_letter(layouter, *id, container, lines, basis)
                        })
                        .unwrap_or_else(|| layouter.atomic(*id, width));
                    (
                        PieceKind::Float {
                            id: *id,
                            fragment: Some(Box::new(atomic.fragment)),
                            size: (atomic.width, atomic.height),
                            side: style.float,
                            clear: style.clear,
                        },
                        0.0,
                        0.0,
                    )
                };
                pieces.push(Piece {
                    kind,
                    style,
                    width: piece_width,
                    min_width,
                    trailing_space: 0.0,
                    break_after: measure && wraps_itself,
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    bidi: level,
                    edges: spacing::Edges::default(),
                    broken: None,
                });
            }
            InlineItem::Atomic(id) => {
                // Lines may break before and after an atomic inline (UAX #14, class CB) where
                // the content around it wraps, as in Blink: before it after text that wraps, also
                // where inline boxes open or end between them, and after it where the box it is
                // in wraps, whatever its own `white-space`. For the min-content width, Blink
                // breaks before it only where the text allows (not after an opening bracket).
                // A ruby column breaks like the text of its base (Blink's
                // `CanBreakAfterRubyColumn`).
                let ruby = layouter.tree.ruby_columns.contains_key(id);
                if !measure
                    && !ruby
                    && let Some(last) = pieces
                        .iter()
                        .rposition(|piece| !matches!(piece.kind, PieceKind::Open(_)))
                {
                    let before = pieces[..=last]
                        .iter()
                        .rposition(|piece| !matches!(piece.kind, PieceKind::Close(_)));
                    if let Some(before) = before {
                        let breaks = pieces[before].style.white_space.wraps()
                            && matches!(
                                pieces[before].kind,
                                PieceKind::Text { .. } | PieceKind::Placeholder(_)
                            );
                        if breaks {
                            pieces[before].break_after = false;
                            pieces[last].break_after = true;
                        }
                    }
                }
                let style = Arc::clone(&layouter.tree.get(*id).style);
                let (kind, width, min_width) = if measure {
                    let (min, max) = layouter.intrinsic(*id);
                    (PieceKind::Measured, max.to_px(), min.to_px())
                } else {
                    let atomic = layouter.atomic(*id, width);
                    let atomic_width = atomic.width.to_px();
                    (
                        PieceKind::Atomic {
                            fragment: Box::new(atomic.fragment),
                            height: atomic.height.to_px(),
                            baseline: atomic.baseline.to_px(),
                            ruby: crate::ruby::Part::of(layouter.tree, *id)
                                .filter(|part| wraps && part.breaks())
                                .map(Box::new),
                        },
                        atomic_width,
                        atomic_width,
                    )
                };
                pieces.push(Piece {
                    kind,
                    style,
                    width,
                    min_width,
                    trailing_space: 0.0,
                    break_after: wraps && (!ruby || paragraph.breaks_at(paragraph.ranges[index].end)),
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    bidi: level,
                    edges: crate::ruby::base_ends(layouter.tree, *id)
                        .filter(|_| ruby)
                        .map(|(first, last)| spacing::Edges::ruby(*id, first, last))
                        .unwrap_or_default(),
                    broken: None,
                });
            }
            InlineItem::Text { text, style } => {
                // The last word of the paragraph breaks at its hyphenation points only where it
                // starts a line. Blink looks for the word's end in the whole paragraph, so a word
                // that goes on in the next element (a footnote reference, a closing period in a
                // link) is the last one too; but it looks for the word's start only from the
                // start of the item, so the part of the word in an item that starts inside it
                // breaks anywhere (`ShouldHyphenate` in `ShapingLineBreaker`).
                let range = &paragraph.ranges[index];
                let unhyphenated = paragraph
                    .last_word_start()
                    .filter(|&start| range.start < start && start < range.end)
                    .map(|start| start - range.start);
                // Characters without a script of their own take theirs from the text around
                // them in the whole paragraph, across elements, as Blink segments it.
                let first = pieces.len();
                let script = (style.letter_spacing != 0.0)
                    .then(|| {
                        letter_spacing::ends_cursive(&paragraph.text[..range.start]).or_else(|| {
                            paragraph.text[range.start..]
                                .chars()
                                .find_map(fagus_text::cursive_script)
                        })
                    })
                    .flatten();
                let mut breaks = paragraph.breaks(index, style, measure);
                // A line may break between a space and a tab that starts text whose lines do
                // not wrap, where it cannot hang, as Blink breaks before such a tab as before a
                // space (`IsBreakableSpace`), which UAX #14 does not.
                breaks.1 |= style.white_space.wraps()
                    && text.ends_with(' ')
                    && items[index + 1..]
                        .iter()
                        .find(|item| !matches!(item, InlineItem::Open { .. } | InlineItem::Close))
                        .is_some_and(|item| {
                            matches!(item, InlineItem::Text { text, style }
                                if text.starts_with('\t') && !style.white_space.wraps())
                        });
                let breaks_at_end = breaks.1;
                shape_text_hyphenated(
                    fonts,
                    text,
                    style,
                    (levels_of(index), breaks),
                    (
                        unhyphenated,
                        script,
                        around.get(index).unwrap_or(&Around::default()),
                    ),
                    &mut pieces,
                );
                // The end of a text whose lines do not wrap may break where the box around it
                // and what follows wraps (CSS Text 3, §5.1: the nearest common ancestor decides).
                if breaks_at_end
                    && ends_wrap[index]
                    && !style.white_space.wraps()
                    && pieces.len() > first
                    && let Some(last) = pieces.last_mut()
                    && !last.forced_break_after
                {
                    last.break_after = true;
                }
                kern_across_boxes(fonts, &mut pieces, first);
            }
            InlineItem::PageNumber { style, .. }
            | InlineItem::TargetPage { style, .. }
            | InlineItem::NamedString { style, .. } => {
                if let Some(text) = &generated[index] {
                    shape_text(
                        fonts,
                        text,
                        style,
                        (levels_of(index), paragraph.breaks(index, style, measure)),
                        &mut pieces,
                    );
                }
            }
            InlineItem::Leader { fill, style } => {
                let mut shaped = Vec::new();
                shape_text(
                    fonts,
                    fill,
                    style,
                    (levels_of(index), own_breaks(fill)),
                    &mut shaped,
                );
                if let Some(Piece {
                    kind:
                        PieceKind::Text {
                            face,
                            glyphs,
                            text,
                            caps,
                        },
                    width,
                    ..
                }) = shaped.into_iter().next()
                    && width > 0.0
                {
                    // Glued to what precedes and follows it.
                    if let Some(last) = pieces.last_mut() {
                        last.break_after = false;
                    }
                    pieces.push(Piece {
                        kind: PieceKind::Leader {
                            face,
                            glyphs,
                            text,
                            caps,
                            unit: width,
                        },
                        style: Arc::clone(style),
                        width: 0.0,
                        min_width: 0.0,
                        trailing_space: 0.0,
                        break_after: false,
                        forced_break_after: false,
                        break_clear: fagus_style::values::Clear::None,
                        hyphen: 0.0,
                        last_word_hyphenation: false,
                        bidi: level,
                        edges: spacing::Edges::default(),
                        broken: None,
                    });
                }
            }
        }
    }
    if let Some(last) = pieces.last_mut() {
        last.break_after = true;
    }
    hang_spaces_together(&mut pieces);
    spacing::space(fonts, &mut pieces, container, &layouter.han_fonts);
    // Ruby columns with the space of `text-autospace` before their base are laid out again.
    for piece in &mut pieces {
        let lead = piece.edges.ruby_lead;
        let Some(id) = piece.edges.ruby_column().filter(|_| lead > 0.0) else {
            continue;
        };
        let Some(column) = layouter.tree.ruby_columns.get(&id) else {
            continue;
        };
        match &mut piece.kind {
            PieceKind::Measured => {
                let width = crate::ruby::intrinsic(layouter, id, column, lead).to_px();
                (piece.width, piece.min_width) = (width, width);
            }
            PieceKind::Atomic {
                fragment,
                height,
                baseline,
                ruby,
            } => {
                if let Some(part) = ruby {
                    part.lead = lead;
                }
                let atomic = crate::ruby::layout(layouter, id, column, lead);
                **fragment = atomic.fragment;
                *height = atomic.height.to_px();
                *baseline = atomic.baseline.to_px();
                (piece.width, piece.min_width) = (atomic.width.to_px(), atomic.width.to_px());
            }
            _ => {}
        }
    }
    (pieces, boxes)
}

/// The text of an item of generated content that is known only at layout: a page number, the
/// page of a target, a named string.
fn generated_text(layouter: &Layouter<'_>, item: &InlineItem) -> Option<String> {
    let number = |value: i32, format| counter_text(value, format, &layouter.tree.counter_styles);
    match item {
        InlineItem::PageNumber { counter, format, .. } => {
            let value = match *counter {
                PageCounter::Footnote { value, ordinal } => match layouter.footnote_numbering.get() {
                    Some((first, start)) if ordinal >= first => {
                        let after = i32::try_from(ordinal - first).unwrap_or(i32::MAX);
                        start.saturating_add(after).saturating_add(1)
                    }
                    _ => value,
                },
                // The values of the page counter, where pages reset, set or increment it.
                counter => crate::tree::page_counter_value(
                    counter,
                    layouter.page_number.get(),
                    &layouter.page_values.borrow(),
                    layouter.margin_page_value.get(),
                ),
            };
            Some(number(value, format))
        }
        // Unknown until a first layout found the page.
        InlineItem::TargetPage { id, format, .. } => {
            layouter.tree.target_page(id).map(|page| number(page, format))
        }
        InlineItem::NamedString { name, position, .. } => {
            let position = layouter.string_position.get().unwrap_or(*position);
            // Page counters in the value count the page of the element that set it, as in the
            // margin boxes (see `page::Marks::counted`): an earlier page, or else this one, whose
            // assignments have no page yet.
            let values = layouter.page_values.borrow();
            let (here, total) = layouter.page_number.get();
            Some(
                layouter
                    .tree
                    .string_at(name, position)
                    .map_or_else(String::new, |(at, value)| {
                        let page = match layouter.string_pages.borrow().get(at) {
                            Some(&page) if (1..here).contains(&page) => page,
                            _ => here,
                        };
                        value.resolve(
                            |counter| crate::tree::page_counter_value(counter, (page, total), &values, None),
                            &layouter.tree.counter_styles,
                        )
                    }),
            )
        }
        _ => None,
    }
}

/// What an inline item is to the bidi algorithm, with the text of a generated one.
fn bidi_content<'a>(item: &'a InlineItem, generated: Option<&'a str>) -> crate::bidi::Content<'a> {
    use crate::bidi::Content;
    match item {
        InlineItem::Text { text, .. } | InlineItem::Leader { fill: text, .. } => Content::Text(text),
        InlineItem::PageNumber { .. } | InlineItem::TargetPage { .. } | InlineItem::NamedString { .. } => {
            generated.map_or(Content::None, Content::Text)
        }
        InlineItem::Open { style, .. } => Content::Open(style),
        InlineItem::Close => Content::Close,
        InlineItem::LineBreak { .. } => Content::LineBreak,
        InlineItem::Atomic(_) | InlineItem::Float(_) | InlineItem::OutOfFlow(_) => Content::Object,
        InlineItem::Marker { inside: true, .. } => Content::Object,
        InlineItem::Marker { .. } => Content::None,
    }
}

/// The most glyphs a leader repeats its pattern to: more than the widest PDF page (200 inches)
/// holds of the narrowest pattern a document would use.
const MAX_LEADER_PATTERNS: usize = 10_000;

/// The character drawn where a word is hyphenated (U+2010, as browsers use).
const HYPHEN: &str = "\u{2010}";

/// What is drawn where a word is hyphenated in the face `face`: `hyphenate-character`, or the
/// hyphen, or where the face has none the hyphen-minus (as Chrome does).
fn hyphen_text<'s>(fonts: &FontDb, style: &'s ComputedStyle, face: FaceId) -> &'s str {
    match style.hyphenate_character.as_deref() {
        Some(text) => text,
        None if fonts.face(face).has_glyph('\u{2010}') => HYPHEN,
        None => "-",
    }
}

/// Shapes text into pieces between its break opportunities (`breaks`: those inside it, and
/// whether a line may break at its end). Soft hyphens and, with `hyphens: auto`, the
/// hyphenation points of the text's language are break opportunities too, which show a hyphen
/// only where a line ends there. A word is shaped as a whole, as in browsers, and cut into
/// pieces at the opportunities inside it, so that the kerning across them stays. Where the
/// lines of `style` do not wrap, the pieces allow no breaks but forced ones.
fn shape_text(
    fonts: &FontDb,
    text: &str,
    style: &Arc<ComputedStyle>,
    shaping: (TextLevels<'_>, (Vec<fagus_text::BreakOpportunity>, bool)),
    pieces: &mut Vec<Piece>,
) {
    shape_text_hyphenated(
        fonts,
        text,
        style,
        shaping,
        (None, None, &Around::default()),
        pieces,
    );
}

/// [`shape_text`], where the paragraph's last word starts at byte `unhyphenated` of the text,
/// `script` says whether the text of the paragraph around it is of a cursive script, for
/// letter spacing: that before it, or where there is none with a script, that after it; and
/// `around` is the text around it that its letters join with.
fn shape_text_hyphenated(
    fonts: &FontDb,
    text: &str,
    style: &Arc<ComputedStyle>,
    (levels, breaks): (TextLevels<'_>, (Vec<fagus_text::BreakOpportunity>, bool)),
    (unhyphenated, script, around): (Option<usize>, Option<bool>, &Around),
    pieces: &mut Vec<Piece>,
) {
    let Some(face) = face_for(fonts, style) else {
        return;
    };
    if style.text_combine_upright && style.is_vertical() {
        pieces.push(combined(fonts, text, style, face));
        return;
    }
    // `word-break: break-all` breaks words without hyphens, and phrases are not hyphenated.
    // Blink hyphenates each word between two break opportunities on its own.
    let hyphenation = match (style.hyphens, style.lang.as_deref()) {
        (Hyphens::Auto, Some(lang))
            if !matches!(style.word_break, WordBreak::BreakAll | WordBreak::AutoPhrase) =>
        {
            Some(lang)
        }
        _ => None,
    };

    let (opportunities, breaks_at_end) = breaks;
    let wraps = style.white_space.wraps();
    let mut parts: Vec<Part> = Vec::new();
    let mut start = 0;
    for opportunity in opportunities {
        let end = opportunity.offset;
        // Other space separators that hang at the end of a line stay together, also after a
        // preserved space (which UAX #14 lets a line break after), so that they hang together;
        // after a collapsible space, a line breaks before them, as in Chrome.
        let preserved = !style.white_space.collapses_spaces();
        if end == start
            || !opportunity.mandatory
                && wraps
                && style.white_space.collapse != fagus_style::values::WhiteSpaceCollapse::BreakSpaces
                && text[end..].starts_with(hangs)
                && text[..end].ends_with(|character| hangs(character) && (character != ' ' || preserved))
        {
            continue;
        }
        let segment_start = start;
        let points = hyphenation
            .map(|lang| fagus_text::hyphenate_word(&text[start..end], lang, style.hyphenate_limit_chars))
            .unwrap_or_default();
        for point in points
            .into_iter()
            .map(|point| segment_start + point)
            .filter(|&point| point > segment_start && point < end)
        {
            parts.push(Part {
                range: start..point,
                end: PartEnd::Hyphenation,
            });
            start = point;
        }
        let end_kind = if opportunity.mandatory {
            PartEnd::Forced
        } else if text[start..end].ends_with('\u{AD}') {
            PartEnd::SoftHyphen
        } else {
            PartEnd::Break
        };
        parts.push(Part {
            range: start..end,
            end: end_kind,
        });
        start = end;
    }
    if wraps && (style.word_break == WordBreak::BreakAll || InnerBreaks::Overflow.allows(style)) {
        parts = cut_long_parts(text, parts, style.word_break == WordBreak::BreakAll);
    }
    let mut hyphen_width = None;
    let mut hyphen = || {
        *hyphen_width.get_or_insert_with(|| {
            shape_segment(
                fonts,
                style,
                face,
                hyphen_text(fonts, style, face),
                TextLevels::LTR,
            )
            .iter()
            .map(|piece| piece.width)
            .sum::<f32>()
        })
    };
    // Soft hyphens that start the item leave no piece of their own: a line breaks after what
    // comes before them (not before boxes that open there), with a hyphen, as Blink breaks
    // after the soft hyphen in the text of the paragraph.
    if wraps
        && style.hyphens != Hyphens::None
        && style.word_break != WordBreak::AutoPhrase
        && parts.first().is_some_and(|part| {
            part.end == PartEnd::SoftHyphen
                && part.range.start == 0
                && text[part.range.clone()]
                    .chars()
                    .all(|character| character == '\u{AD}')
        })
        && let Some(before) = pieces
            .iter_mut()
            .rev()
            .find(|piece| !matches!(piece.kind, PieceKind::Open(_)))
        && !before.forced_break_after
    {
        before.break_after = true;
        before.hyphen = hyphen();
    }
    // Words end after spaces and at forced breaks.
    let mut first = 0;
    // The faces that characters fell back to, which later ones of the same script take first.
    let mut fallbacks = fagus_text::FallbackRun::default();
    fallbacks.start_in_context(text, script);
    // A space or tab that is a word of its own (`break-spaces` breaks after each) after the same
    // one, at the same level, is shaped as the one before where that took the item's own face: a
    // megabyte of spaces would otherwise be shaped space by space.
    let mut last_space: Option<((&str, Level), ShapedWord)> = None;
    for (index, part) in parts.iter().enumerate() {
        // A space that a mark is on is shaped with it, as its base.
        if index + 1 < parts.len()
            && part.end != PartEnd::Forced
            && (!text[..part.range.end].ends_with(' ') || text[part.range.end..].starts_with(is_mark))
        {
            continue;
        }
        let word = &parts[first..=index];
        first = index + 1;
        let space = match word {
            [only] if around.is_empty() && matches!(&text[only.range.clone()], " " | "\t") => {
                Some((&text[only.range.clone()], levels.at(only.range.start)))
            }
            _ => None,
        };
        let shaped = match (&last_space, space) {
            (Some((last, shaped)), Some(space)) if *last == space => shaped.clone(),
            _ => {
                let shaped = shape_word(fonts, style, face, (text, levels, around), word, &mut fallbacks);
                let own_face = shaped.iter().all(
                    |(piece, _)| matches!(piece.kind, PieceKind::Text { face: used, .. } if used == face),
                );
                last_space = space.filter(|_| own_face).map(|space| (space, shaped.clone()));
                shaped
            }
        };
        let mut word_start = true;
        for (mut piece, ends) in shaped {
            if std::mem::take(&mut word_start)
                && let Some(before) = pieces.last_mut()
            {
                kern_after_space(fonts, before, &mut piece);
            }
            match ends.map(|ended| word[ended].end) {
                Some(PartEnd::Break) => piece.break_after = wraps,
                Some(PartEnd::Forced) => {
                    piece.break_after = true;
                    piece.forced_break_after = true;
                }
                Some(PartEnd::Hyphenation) if wraps => {
                    piece.break_after = true;
                    piece.hyphen = hyphen();
                    piece.last_word_hyphenation = ends
                        .zip(unhyphenated)
                        .is_some_and(|(ended, start)| word[ended].range.end > start);
                }
                // `hyphens: none` does not break at soft hyphens either, nor does a phrase of
                // `word-break: auto-phrase` but where it does not fit on a line on its own
                // (Blink sets `hyphens: none` for phrases, see `InnerBreaks::Phrase`).
                Some(PartEnd::SoftHyphen) if wraps && style.hyphens != Hyphens::None => {
                    piece.break_after = style.word_break != WordBreak::AutoPhrase;
                    piece.hyphen = hyphen();
                }
                _ => {}
            }
            // A zero width space (as `<wbr>` is) draws nothing.
            if let PieceKind::Text { glyphs, text, .. } = &mut piece.kind
                && text.contains(ZERO_WIDTH_SPACE)
            {
                glyphs.retain(|glyph| {
                    text.get(glyph.cluster.0 as usize..glyph.cluster.1 as usize) != Some(ZERO_WIDTH_SPACE)
                });
            }
            pieces.push(piece);
        }
    }
    // The end of an item is a break opportunity where the text of the paragraph has one there:
    // the next item may continue the word.
    if let Some(last) = pieces.last_mut()
        && !breaks_at_end
        && !last.forced_break_after
    {
        last.break_after = false;
        last.hyphen = 0.0;
    }
}

/// For each item, whether the lines of the nearest common ancestor of its end and the next text
/// or atomic inline wrap (where the item is a text followed by one): its `white-space` decides
/// whether a line may break between them (CSS Text 3, §5.1). The container is the root.
fn ends_wrap(items: &[InlineItem], container: Option<&ComputedStyle>) -> Vec<bool> {
    let mut ends = vec![false; items.len()];
    let mut stack: Vec<Option<&ComputedStyle>> = vec![container];
    // The last text item, and the fewest boxes around it and what follows since.
    let mut pending: Option<(usize, usize)> = None;
    for (index, item) in items.iter().enumerate() {
        match item {
            InlineItem::Open { style, .. } => stack.push(Some(style)),
            InlineItem::Close => {
                if stack.len() > 1 {
                    stack.pop();
                }
                if let Some((_, depth)) = &mut pending {
                    *depth = (*depth).min(stack.len());
                }
            }
            InlineItem::Text { .. } | InlineItem::Atomic(_) => {
                if let Some((earlier, depth)) = pending.take() {
                    ends[earlier] = stack[depth - 1].is_some_and(|style| style.white_space.wraps());
                }
                if matches!(item, InlineItem::Text { .. }) {
                    pending = Some((index, stack.len()));
                }
            }
            InlineItem::LineBreak { .. } => pending = None,
            _ => {}
        }
    }
    ends
}

/// Kerns the space that ends piece `before` with the first character of the word `after`,
/// which Fagus shapes apart: Chrome shapes the text of an item at once, so that the pairs of a
/// space and a letter that fonts such as Arial, Helvetica and Times kern (a space and "T" or
/// "A") kern across words. The space gets narrower or wider by the kerning, which it keeps
/// where it hangs at the end of a line.
fn kern_after_space(fonts: &FontDb, before: &mut Piece, after: &mut Piece) {
    let (
        PieceKind::Text {
            face,
            glyphs,
            text,
            caps,
        },
        PieceKind::Text {
            face: next_face,
            text: next_text,
            caps: next_caps,
            ..
        },
    ) = (&mut before.kind, &after.kind)
    else {
        return;
    };
    let Some(next) = next_text.chars().next() else {
        return;
    };
    if *face != *next_face
        || *caps != Caps::NONE
        || *next_caps != Caps::NONE
        || !text.ends_with(' ')
        || next == ' '
        || before.bidi.is_rtl()
        || after.bidi.is_rtl()
        || !Arc::ptr_eq(&before.style, &after.style)
        || !fonts.face(*face).axes.is_empty()
    {
        return;
    }
    let style = &before.style;
    let mut features = variant_features(fonts, *face, style);
    features.extend_from_slice(&style.font_feature_settings);
    let advance = |text: &str| fonts.shape_with(*face, text, &features).advance;
    let mut buffer = [0; 4];
    let alone = next.encode_utf8(&mut buffer);
    let pair = format!(" {alone}");
    let kerning = (advance(&pair) - advance(" ") - advance(alone))
        * fonts.face(*face).scale(used_font_size(fonts, style));
    if kerning == 0.0 {
        return;
    }
    let Some(space) = glyphs.last_mut() else {
        return;
    };
    space.advance += kerning;
    before.width += kerning;
    before.trailing_space += kerning;
    after.edges.kerned_before = true;
}

/// Whether the start of an inline box (or its end) has a margin, border or padding by its
/// style, however thin: one that its layout units cut to nothing still separates text.
fn has_frame(piece: &Piece) -> bool {
    let style = &piece.style;
    let (margin, padding, border) = match piece.kind {
        PieceKind::Open(_) => (&style.margin.left, &style.padding.left, style.border_width.left),
        _ => (
            &style.margin.right,
            &style.padding.right,
            style.border_width.right,
        ),
    };
    let zero = |length: &LengthPercentage| matches!(length, LengthPercentage::Px(px) | LengthPercentage::Percent(px) if *px == 0.0);
    border != 0.0 || !zero(padding) || !matches!(margin, LengthPercentageAuto::Length(length) if zero(length))
}

/// Kerns the last character of the text before an item's first piece (`pieces[first]`) with
/// the first one of the piece, across the edges of inline boxes between them, which Fagus shapes
/// apart: Chrome shapes the text of adjacent items at once where their font is the same and no
/// margin, border or padding or a vertical alignment separates them (`InlineNode::ShapeText`),
/// so that, say, a closing quote of `<q>` kerns with the full stop after it.
fn kern_across_boxes(fonts: &FontDb, pieces: &mut [Piece], first: usize) {
    if first == 0 || first >= pieces.len() {
        return;
    }
    let mut before = first - 1;
    while matches!(pieces[before].kind, PieceKind::Open(_) | PieceKind::Close(_))
        && pieces[before].width == 0.0
        && !has_frame(&pieces[before])
        && pieces[before].style.vertical_align == VerticalAlign::Baseline
    {
        let Some(earlier) = before.checked_sub(1) else {
            return;
        };
        before = earlier;
    }
    // Nor across a forced line break.
    if pieces[before..first].iter().any(|piece| piece.forced_break_after) {
        return;
    }
    let (left, right) = pieces.split_at_mut(first);
    let (before, after) = (&mut left[before], &mut right[0]);
    let (
        PieceKind::Text {
            face,
            glyphs,
            text,
            caps,
        },
        PieceKind::Text {
            face: next_face,
            text: next_text,
            caps: next_caps,
            ..
        },
    ) = (&mut before.kind, &after.kind)
    else {
        return;
    };
    let (Some(last), Some(next)) = (text.chars().last(), next_text.chars().next()) else {
        return;
    };
    let (style, next_style) = (&before.style, &after.style);
    // The same font, as Blink compares fonts, and text that kerns.
    let features = |style: &ComputedStyle| {
        let mut features = variant_features(fonts, *face, style);
        features.extend_from_slice(&style.font_feature_settings);
        features
    };
    if *face != *next_face
        || *caps != Caps::NONE
        || *next_caps != Caps::NONE
        || before.bidi != after.bidi
        || before.bidi.is_rtl()
        || !fonts.face(*face).axes.is_empty()
        // Blink compares the whole font description: its spacing, and the weight, style and
        // stretch that ask for (or synthesize) the face, however equal the face they get.
        || style.letter_spacing.to_bits() != next_style.letter_spacing.to_bits()
        || (style.font_weight - next_style.font_weight).abs() > 1e-3
        || style.font_style != next_style.font_style
        || (style.font_stretch - next_style.font_stretch).abs() > 1e-3
        || (used_font_size(fonts, style) - used_font_size(fonts, next_style)).abs() > 1e-4
        || features(style) != features(next_style)
        // The kerning of a space with a word of the same style is `kern_after_space`'s.
        || (last == ' ' && Arc::ptr_eq(style, next_style))
        // Letters that join take their forms from the text around them, which shaping them
        // apart and together tells apart from kerning: they are shaped with that text.
        || fagus_text::joins(last)
        || fagus_text::joins(next)
    {
        return;
    }
    let features = features(style);
    let advance = |text: &str| fonts.shape_with(*face, text, &features).advance;
    let (mut one, mut two) = ([0; 4], [0; 4]);
    let (last, next) = (&*last.encode_utf8(&mut one), &*next.encode_utf8(&mut two));
    let pair = fonts.shape_with(*face, &format!("{last}{next}"), &features);
    let scale = fonts.face(*face).scale(used_font_size(fonts, style));
    let kerning = (pair.advance - advance(last) - advance(next)) * scale;
    if kerning == 0.0 {
        return;
    }
    // A `kern` or `kerx` table's kerning goes half to each glyph, the second moved by its half
    // too (HarfBuzz's `hb_kern_machine_t`, which halves it in 1/65536 pixel, the first half
    // rounded down); `GPOS` kerning all to the first. Blink rounds the width of each item up
    // by itself, so where the halves go counts.
    let second = match pair.glyphs.as_slice() {
        [_, glyph]
            if glyph.x_offset != 0.0
                && (glyph.x_offset - (glyph.x_advance - glyph.own_advance)).abs() < 1e-3 =>
        {
            #[allow(clippy::cast_possible_truncation, reason = "kerning in 1/65536 pixel")]
            let units = (f64::from(kerning) * 65536.0).round() as i64;
            #[allow(clippy::cast_precision_loss, reason = "kerning in 1/65536 pixel")]
            let second = (units - (units >> 1)) as f32 / 65536.0;
            second
        }
        _ => 0.0,
    };
    let (
        Some(glyph),
        PieceKind::Text {
            glyphs: next_glyphs, ..
        },
    ) = (glyphs.last_mut(), &mut after.kind)
    else {
        return;
    };
    let second = match next_glyphs.first_mut() {
        Some(next_glyph) if second != 0.0 => {
            next_glyph.advance += second;
            next_glyph.x_offset += second;
            second
        }
        _ => 0.0,
    };
    let first = kerning - second;
    glyph.advance += first;
    before.width += first;
    before.edges.kern_after = first;
    if last == " " {
        before.trailing_space += first;
    }
    after.width += second;
    after.edges.kerned_before = true;
    after.edges.kern_before = second;
}

/// Whether text in `style` and in `other` has the same font, as Blink compares fonts to shape
/// the text of adjacent items at once: the same face at the same size, with the same features,
/// spacing and language.
fn same_font(fonts: &FontDb, style: &ComputedStyle, other: &ComputedStyle) -> bool {
    std::ptr::eq(style, other)
        || face_for(fonts, style) == face_for(fonts, other)
            && (used_font_size(fonts, style) - used_font_size(fonts, other)).abs() < 1e-4
            && style.font_features() == other.font_features()
            && style.font_feature_settings == other.font_feature_settings
            && style.font_variation_settings == other.font_variation_settings
            && (style.font_weight - other.font_weight).abs() < 1e-3
            && style.letter_spacing.to_bits() == other.letter_spacing.to_bits()
            && style.word_spacing.to_bits() == other.word_spacing.to_bits()
            && style.lang == other.lang
            && style.writing_mode == other.writing_mode
}

/// Text set horizontally in the room of one character of a vertical line
/// (`text-combine-upright: all`, CSS Writing Modes 4, §9.1): one piece, 1em long, as Chrome sets
/// it. Glyphs wider than 1.1em together take the face's half-, third- or quarter-width forms
/// where those fit, or else are squeezed to fit when painted. Letter and word spacing do not
/// apply inside (Blink's `StyleAdjuster::AdjustStyleForCombinedText`).
fn combined(fonts: &FontDb, text: &str, style: &Arc<ComputedStyle>, face: FaceId) -> Piece {
    const NARROWER: [&[([u8; 4], u32)]; 3] = [&[(*b"hwid", 1)], &[(*b"twid", 1)], &[(*b"qwid", 1)]];
    let unspaced;
    let style = if style.letter_spacing.to_bits() == 0 && style.word_spacing.to_bits() == 0 {
        style
    } else {
        let mut plain = ComputedStyle::clone(style);
        plain.letter_spacing = 0.0;
        plain.word_spacing = 0.0;
        unspaced = Arc::new(plain);
        &unspaced
    };
    let size = used_font_size(fonts, style);
    let room = size * COMBINED_ROOM;
    let shape = |features| {
        let caps = Caps {
            features,
            ..Caps::NONE
        };
        shape_run(
            fonts,
            style,
            (face, Level::LTR),
            text,
            caps,
            (None, &Around::default()),
        )
    };
    let mut piece = shape(&[]);
    if piece.width > room {
        let narrower = NARROWER
            .iter()
            .filter(|features| fonts.face(face).substitutes(features[0].0))
            .map(|features| shape(features))
            .find(|narrower| narrower.width <= room);
        if let Some(narrower) = narrower {
            piece = narrower;
        }
    }
    Piece {
        width: size,
        min_width: size,
        trailing_space: 0.0,
        break_after: true,
        ..piece
    }
}

/// How wide text combined upright may be, in ems, before it is set narrower (as in Chrome).
pub const COMBINED_ROOM: f32 = 1.1;

/// A part of a text between two places where a line may break.
#[derive(Clone)]
struct Part {
    range: Range<usize>,
    end: PartEnd,
}

/// How a line may break after a part of a text.
#[derive(Clone, Copy, PartialEq)]
enum PartEnd {
    Break,
    Forced,
    Hyphenation,
    SoftHyphen,
    /// No break opportunity: a word of thousands of letters that `overflow-wrap` breaks
    /// anywhere is cut in parts (see [`cut_long_parts`]).
    Inside,
}

/// `parts` with those longer than [`LONG_TEXT`] bytes cut into parts about half as long, so that
/// the line breaker looks at no more than such a part where it breaks inside one (it looks at
/// and shapes again the rest of the piece it breaks, for each line): with `break_all`, at the
/// breaks between letters (soft wrap opportunities like any other), else (`overflow-wrap`,
/// where lines may break anywhere but only where nothing else fits) at a character. The word is
/// still shaped whole and cut where the parts end.
fn cut_long_parts(text: &str, parts: Vec<Part>, break_all: bool) -> Vec<Part> {
    if parts.iter().all(|part| part.range.len() <= LONG_TEXT) {
        return parts;
    }
    let mut cut = Vec::with_capacity(parts.len());
    for part in parts {
        if part.range.len() <= LONG_TEXT {
            cut.push(part);
            continue;
        }
        let Range { start, end } = part.range;
        let places: Vec<usize> = if break_all {
            fagus_text::break_all_opportunities(&text[start..end])
                .into_iter()
                .map(|offset| start + offset)
                .collect()
        } else {
            // Where a place lies inside a cluster, the part ends before the cluster.
            text[start..end]
                .char_indices()
                .map(|(offset, _)| start + offset)
                .collect()
        };
        let mut from = start;
        for place in places {
            if place - from >= LONG_TEXT / 2 && end - place >= LONG_TEXT / 4 {
                cut.push(Part {
                    range: from..place,
                    end: if break_all {
                        PartEnd::Break
                    } else {
                        PartEnd::Inside
                    },
                });
                from = place;
            }
        }
        cut.push(Part {
            range: from..end,
            end: part.end,
        });
    }
    cut
}

/// The pieces of a word, shaped, each with the index of the part of the word it ends, if it ends
/// one (see [`shape_word`]).
type ShapedWord = Vec<(Piece, Option<usize>)>;

/// Shapes the parts of a word as a whole and cuts the result where they end, so that the word
/// is as wide as set whole while it does not break, as in Chrome. Each piece comes with the
/// index of the part it ends, if it ends one (it may end where the face changes). Soft hyphens
/// are not drawn and left out. Where a part ends inside a cluster (a ligature), its piece ends
/// before the cluster; a piece that ends at a hyphenation point inside a cluster or between
/// glyphs that shaping set together gets the form it takes where the line breaks there
/// ([`Broken`]). `around` is the text around `text` that its letters join with.
fn shape_word(
    fonts: &FontDb,
    style: &Arc<ComputedStyle>,
    face: FaceId,
    (text, levels, around): (&str, TextLevels<'_>, &Around),
    parts: &[Part],
    fallbacks: &mut fagus_text::FallbackRun,
) -> ShapedWord {
    let word_around = match (parts.first(), parts.last()) {
        (Some(first), Some(last))
            if !around.is_empty() || fagus_text::has_joining(&text[first.range.start..last.range.end]) =>
        {
            around.of(text, first.range.start..last.range.end)
        }
        _ => Around::NONE,
    };
    let mut word = String::new();
    // The bidi level of each byte of the word.
    let mut word_levels = Vec::new();
    let mut ends = Vec::with_capacity(parts.len());
    for part in parts {
        for (offset, character) in text[part.range.clone()].char_indices() {
            if character != '\u{AD}' {
                word.push(character);
                if !levels.is_uniform() {
                    let at = part.range.start + offset;
                    word_levels.extend((at..at + character.len_utf8()).map(|byte| levels.at(byte).level));
                }
            }
        }
        ends.push(word.len());
    }
    let word_levels = if levels.is_uniform() {
        levels
    } else {
        TextLevels::new(&word_levels, levels.at(0).paragraph)
    };
    let mut result = Vec::new();
    let mut part = 0;
    let mut run_start = 0;
    for piece in shape_segment_in(fonts, style, face, (&word, word_levels), &word_around, fallbacks) {
        let PieceKind::Text {
            text: run, glyphs, ..
        } = &piece.kind
        else {
            continue;
        };
        let run_end = run_start + run.len();
        // Where the run is cut, all at once (cutting the rest again for each part would take
        // time growing with the square of the parts, as in text without spaces): at the start of
        // the cluster each part ends in, with the end of the part and its index.
        let mut starts: Vec<usize> = glyphs.iter().map(|glyph| glyph.cluster.0 as usize).collect();
        starts.sort_unstable();
        starts.dedup();
        let mut cuts: Vec<(usize, usize, usize)> = Vec::new();
        let mut offset = 0;
        while part < ends.len() && ends[part] < run_end {
            let end = ends[part] - run_start;
            part += 1;
            // A part of nothing but soft hyphens, or one that ends in the cluster the rest of
            // the piece starts with, ends nowhere.
            let Some(cut) = starts
                .partition_point(|start| *start <= end)
                .checked_sub(1)
                .map(|at| starts[at])
                .filter(|cut| *cut > offset)
            else {
                continue;
            };
            cuts.push((cut, end, part - 1));
            offset = cut;
        }
        let run = if cuts.is_empty() {
            String::new()
        } else {
            run.clone()
        };
        let at: Vec<usize> = cuts.iter().map(|(cut, ..)| *cut).collect();
        let Some(cut_pieces) = cut_piece_at(piece, &at) else {
            return shape_parts(fonts, style, face, (text, levels, around), parts, fallbacks);
        };
        let mut cut_pieces = cut_pieces.into_iter();
        for (&(cut, end, index), mut first) in cuts.iter().zip(&mut cut_pieces) {
            let hyphenates = matches!(parts[index].end, PartEnd::Hyphenation | PartEnd::SoftHyphen);
            // Where the line breaks after a hyphen, Chrome shapes the text before the break
            // again if shaping set the hyphen together with what follows: kerned with the
            // letter after it (`B2B-V`), it ends the line as wide as it is alone.
            let after_hyphen = matches!(parts[index].end, PartEnd::Break)
                && run
                    .get(..end)
                    .is_some_and(|before| before.ends_with(['-', '\u{2010}']));
            if cut < end || hyphenates || after_hyphen {
                first.broken = broken_form(fonts, &first, &run[cut..end]);
            }
            result.push((first, Some(index)));
        }
        let Some(piece) = cut_pieces.next() else {
            continue;
        };
        let ended = (part < ends.len() && ends[part] == run_end).then(|| {
            part += 1;
            part - 1
        });
        result.push((piece, ended));
        run_start = run_end;
    }
    result
}

/// What `piece` becomes where a line breaks after it, `carried` (the start of the next piece)
/// being the rest of its text before the break: shaped again with it, as Chrome shapes the text
/// before a break that is not safe to break at. `None` where it is no different.
fn broken_form(fonts: &FontDb, piece: &Piece, carried: &str) -> Option<Box<Broken>> {
    let PieceKind::Text {
        face,
        text,
        caps,
        glyphs,
    } = &piece.kind
    else {
        return None;
    };
    let end = shape_run(
        fonts,
        &piece.style,
        (*face, piece.bidi),
        &format!("{text}{carried}"),
        *caps,
        (None, &Around::default()),
    );
    let PieceKind::Text { glyphs: shaped, .. } = &end.kind else {
        return None;
    };
    let same = carried.is_empty()
        && glyphs.len() == shaped.len()
        && glyphs
            .iter()
            .zip(shaped)
            .all(|(glyph, again)| glyph.id == again.id && (glyph.advance - again.advance).abs() < 0.001);
    (!same).then(|| {
        Box::new(Broken {
            end,
            into_next: carried.len(),
        })
    })
}

/// Shapes the parts of a word one by one.
fn shape_parts(
    fonts: &FontDb,
    style: &Arc<ComputedStyle>,
    face: FaceId,
    (text, levels, around): (&str, TextLevels<'_>, &Around),
    parts: &[Part],
    fallbacks: &mut fagus_text::FallbackRun,
) -> ShapedWord {
    let mut result = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let mut part_text = String::new();
        let mut part_levels = Vec::new();
        for (offset, character) in text[part.range.clone()].char_indices() {
            if character != '\u{AD}' {
                part_text.push(character);
                if !levels.is_uniform() {
                    let at = part.range.start + offset;
                    part_levels.extend((at..at + character.len_utf8()).map(|byte| levels.at(byte).level));
                }
            }
        }
        let part_levels = if levels.is_uniform() {
            levels
        } else {
            TextLevels::new(&part_levels, levels.at(0).paragraph)
        };
        let shaped = shape_segment_in(
            fonts,
            style,
            face,
            (&part_text, part_levels),
            &around.of(text, part.range.clone()),
            fallbacks,
        );
        let last = shaped.len().saturating_sub(1);
        result.extend(
            shaped
                .into_iter()
                .enumerate()
                .map(|(position, piece)| (piece, (position == last).then_some(index))),
        );
    }
    result
}

/// Cuts a shaped text piece at bytes `at` of its text (in increasing order), keeping the glyphs
/// as they were shaped (in either direction), in one pass. `None` if one of them lies inside a
/// cluster or at none. Without cuts, the piece stays as it is.
fn cut_piece_at(piece: Piece, at: &[usize]) -> Option<Vec<Piece>> {
    if at.is_empty() {
        return Some(vec![piece]);
    }
    let PieceKind::Text {
        face,
        glyphs,
        text,
        caps,
    } = piece.kind
    else {
        return None;
    };
    let mut buckets: Vec<Vec<Glyph>> = vec![Vec::new(); at.len() + 1];
    let mut found = vec![false; at.len()];
    for mut glyph in glyphs {
        let (start, end) = (glyph.cluster.0 as usize, glyph.cluster.1 as usize);
        // The cuts at or before the start of the glyph's cluster; the next must not lie inside it.
        let bucket = at.partition_point(|cut| *cut <= start);
        if at.get(bucket).is_some_and(|cut| *cut < end) {
            return None;
        }
        if let Some(cut) = bucket.checked_sub(1) {
            found[cut] |= at[cut] == start;
            let shift = at[cut] as u32;
            glyph.cluster = (glyph.cluster.0 - shift, glyph.cluster.1 - shift);
        }
        buckets[bucket].push(glyph);
    }
    if found.contains(&false) {
        return None;
    }
    let bounds = std::iter::once(0)
        .chain(at.iter().copied())
        .zip(at.iter().copied().chain([text.len()]));
    Some(
        buckets
            .into_iter()
            .zip(bounds)
            .map(|(glyphs, (start, end))| {
                let text = &text[start..end];
                let width = glyphs.iter().map(|glyph| glyph.advance).sum();
                Piece {
                    trailing_space: trailing_space(&glyphs, text, &piece.style),
                    kind: PieceKind::Text {
                        face,
                        glyphs,
                        text: text.to_owned(),
                        caps,
                    },
                    style: Arc::clone(&piece.style),
                    width,
                    min_width: width,
                    break_after: false,
                    forced_break_after: false,
                    break_clear: fagus_style::values::Clear::None,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    bidi: piece.bidi,
                    edges: spacing::Edges::default(),
                    broken: None,
                }
            })
            .collect(),
    )
}

/// The width of the spaces at the end of a shaped text, which hang at the end of a line. Spaces
/// of `white-space: break-spaces` do not hang (CSS Text 3, §4.1.3): they take room on the line
/// like other characters, so a word whose space after it does not fit goes to the next line.
/// Nor do the preserved spaces of `white-space: pre`, which never wraps: Chrome counts them in
/// the width of the line (a button labelled " " is as wide as a space).
fn trailing_space(glyphs: &[Glyph], text: &str, style: &ComputedStyle) -> f32 {
    use fagus_style::values::WhiteSpaceCollapse;
    let collapse = style.white_space.collapse;
    if collapse == WhiteSpaceCollapse::BreakSpaces
        || (collapse == WhiteSpaceCollapse::Preserve && style.white_space.nowrap)
    {
        return 0.0;
    }
    let end = text.trim_end_matches(hangs).len();
    glyphs
        .iter()
        .filter(|glyph| glyph.cluster.0 as usize >= end)
        .map(|glyph| glyph.advance)
        .sum()
}

/// Lets the spaces at the end of a run of pieces between two break opportunities hang together,
/// where they are pieces of their own (in a face of their own, of another inline box): a piece
/// of nothing but spaces takes the spaces that hang before it in the run as its own. Of other
/// space separators that start a run (after a space, where a line may break), the first does not
/// hang, as in Chrome: they go to the next line where it does not fit, the others hang.
fn hang_spaces_together(pieces: &mut [Piece]) {
    // The width of the spaces that hang at the end of the run so far, and whether it has
    // anything else.
    let mut carried = 0.0;
    let mut content = false;
    // The width of the spaces that hang at the end of the pieces so far.
    let mut hanging = 0.0;
    for piece in pieces.iter_mut() {
        match &piece.kind {
            PieceKind::Text { text, glyphs, .. } if text.chars().all(hangs) => {
                piece.trailing_space = if content || carried > 0.0 {
                    piece.trailing_space + carried
                } else if text.starts_with(' ') {
                    measured_trailing_space(piece)
                } else {
                    let first: f32 = glyphs
                        .iter()
                        .filter(|glyph| glyph.cluster.0 == 0)
                        .map(|glyph| glyph.advance)
                        .sum();
                    (piece.trailing_space - first).max(measured_trailing_space(piece))
                };
                carried = piece.trailing_space;
            }
            PieceKind::Text { .. } => {
                content = true;
                carried = piece.trailing_space;
            }
            PieceKind::Open(_) | PieceKind::Close(_) => {
                // A box's end takes the spaces before it, which hang after it.
                if piece.trailing_space > 0.0 {
                    piece.trailing_space = carried;
                }
            }
            // An out-of-flow box that a line may end with takes the spaces that hang before it
            // (past the starts of inline boxes that take no room), which hang after it.
            PieceKind::Placeholder(_) if piece.break_after => piece.trailing_space = hanging,
            PieceKind::Placeholder(_) | PieceKind::Float { .. } => {}
            _ => {
                content = true;
                carried = 0.0;
            }
        }
        hanging = match piece.kind {
            PieceKind::Open(_) | PieceKind::Placeholder(_) if piece.width == 0.0 => hanging,
            _ => piece.trailing_space,
        };
        if piece.break_after || piece.forced_break_after {
            carried = 0.0;
            content = false;
        }
    }
}

/// The width of the preserved spaces that wrap (`white-space: pre-wrap`) at the end of a line
/// of `pieces`, which hang only as far as they overflow it at a forced break.
fn preserved_trailing_space(pieces: &[Piece]) -> f32 {
    let mut width = 0.0;
    for piece in pieces.iter().rev() {
        match &piece.kind {
            PieceKind::Open(_) | PieceKind::Close(_) => {}
            PieceKind::Text { glyphs, text, .. } if hangs_if_overflowing(&piece.style) => {
                let end = text
                    .trim_end_matches(|character| hangs(character) || character == '\t')
                    .len();
                width += glyphs
                    .iter()
                    .filter(|glyph| glyph.cluster.0 as usize >= end)
                    .map(|glyph| glyph.advance)
                    .sum::<f32>();
                if end > 0 {
                    break;
                }
            }
            _ => break,
        }
    }
    width
}

/// Whether the texts of `pieces` are nothing but spaces that hang at the end of a line: a zero
/// width space among them is a character like others, which the line breaks before.
fn only_spaces<'p>(mut pieces: impl Iterator<Item = &'p Piece>) -> bool {
    pieces.all(|piece| match &piece.kind {
        PieceKind::Text { text, .. } => text.chars().all(hangs),
        _ => true,
    })
}

/// The width of the collapsible spaces at the end of a line of `pieces`, which are removed
/// there: those at the end of its last text, and of the texts of nothing but such spaces before
/// it. After another space separator, which hangs, they hang with it, as in Chrome.
fn removed_spaces(pieces: &[Piece]) -> f32 {
    let mut removed = 0.0;
    for piece in pieces.iter().rev() {
        match &piece.kind {
            PieceKind::Open(_) | PieceKind::Close(_) | PieceKind::Placeholder(_) => {}
            PieceKind::Text { glyphs, text, .. } if piece.style.white_space.collapses_spaces() => {
                let end = text.trim_end_matches(' ').len();
                if text[..end].ends_with(hangs) {
                    return 0.0;
                }
                removed += glyphs
                    .iter()
                    .filter(|glyph| glyph.cluster.0 as usize >= end)
                    .map(|glyph| glyph.advance)
                    .sum::<f32>();
                if end > 0 {
                    break;
                }
            }
            PieceKind::Text { text, .. } if text.is_empty() => {}
            PieceKind::Text { text, .. } if text.ends_with(hangs) => return 0.0,
            _ => break,
        }
    }
    removed
}

/// Whether `character` is a space that hangs at the end of a line where it is not preserved
/// unbreakably: a space or the ideographic space, as in Chrome (157, measured), where CSS Text 3
/// (§4.1.3) hangs the other space separators too: an em, thin or Ogham space at the end of a
/// line takes room there, and a line that has no room for it breaks before it.
pub(crate) fn hangs(character: char) -> bool {
    matches!(character, ' ' | '\u{3000}')
}

/// Whether the spaces that end a line of text in `style` before a forced break or at the end of
/// the paragraph hang only as far as they overflow the line: preserved spaces where lines wrap
/// (`white-space: pre-wrap`).
fn hangs_if_overflowing(style: &ComputedStyle) -> bool {
    style.white_space.collapse == fagus_style::values::WhiteSpaceCollapse::Preserve
        && style.white_space.wraps()
}

/// The hyphen at the end of a line where a word is broken. It looks like U+2010, as in
/// browsers, but stands for a soft hyphen (U+00AD) in the text, which readers and copying leave
/// out (PDF 1.7, §14.8.2.5).
fn hyphen_piece(fonts: &FontDb, piece: &Piece) -> Vec<Piece> {
    let face = match &piece.kind {
        PieceKind::Text { face, .. } => Some(face.to_owned()),
        _ => face_for(fonts, &piece.style),
    };
    let mut pieces = face
        .map(|face| {
            shape_segment(
                fonts,
                &piece.style,
                face,
                hyphen_text(fonts, &piece.style, face),
                TextLevels::uniform(piece.bidi),
            )
        })
        .unwrap_or_default();
    for piece in &mut pieces {
        if let PieceKind::Text { glyphs, text, .. } = &mut piece.kind {
            SOFT_HYPHEN.clone_into(text);
            for glyph in glyphs {
                glyph.cluster = (0, SOFT_HYPHEN.len() as u32);
            }
        }
    }
    pieces
}

const SOFT_HYPHEN: &str = "\u{AD}";

const ZERO_WIDTH_SPACE: &str = "\u{200B}";

/// Where a piece of text may break inside, besides its break opportunities (CSS Text 3, §5.2 and
/// §5.5): between letters with `word-break: break-all`, at any character where nothing else on
/// the line fits with `overflow-wrap: anywhere` or `break-word`.
#[derive(Clone, Copy, PartialEq)]
enum InnerBreaks {
    Letters,
    Overflow,
    /// The break opportunities of `word-break: normal` inside a phrase of `auto-phrase`, and
    /// its soft hyphens (between pieces), which Blink takes where the phrase does not fit on a
    /// line on its own.
    Phrase,
}

impl InnerBreaks {
    /// Whether text in `style` may break inside in this way.
    fn allows(self, style: &ComputedStyle) -> bool {
        style.white_space.wraps()
            && match self {
                Self::Letters => style.word_break == WordBreak::BreakAll,
                Self::Overflow => {
                    style.overflow_wrap != OverflowWrap::Normal || style.word_break == WordBreak::BreakWord
                }
                Self::Phrase => style.word_break == WordBreak::AutoPhrase,
            }
    }

    /// The positions inside the text of `piece` where it may break in this way: the starts of
    /// its clusters (the characters a font shapes together), between letters only for
    /// `break-all`.
    fn positions(self, piece: &Piece) -> Vec<usize> {
        let PieceKind::Text { glyphs, text, .. } = &piece.kind else {
            return Vec::new();
        };
        if !self.allows(&piece.style) {
            return Vec::new();
        }
        let end = before_hanging_spaces(piece);
        if self == Self::Phrase {
            return fagus_text::break_opportunities_with(text, crate::breaks::strictness(&piece.style))
                .into_iter()
                .map(|opportunity| opportunity.offset)
                .filter(|&offset| offset > 0 && offset < end)
                .collect();
        }
        let mut starts: Vec<usize> = glyphs
            .iter()
            .map(|glyph| glyph.cluster.0 as usize)
            .filter(|&start| start > 0 && start < end)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        if self == Self::Letters {
            let letters = fagus_text::break_all_opportunities(text);
            starts.retain(|start| letters.binary_search(start).is_ok());
        }
        starts
    }
}

/// The byte offset in the text of `piece` where the spaces that hang at its end start: a line
/// needs no break before them, as they hang at the end of the line before. Spaces of
/// `break-spaces`, which take room, do not hang.
fn before_hanging_spaces(piece: &Piece) -> usize {
    let PieceKind::Text { text, .. } = &piece.kind else {
        return 0;
    };
    if piece.style.white_space.collapse == fagus_style::values::WhiteSpaceCollapse::BreakSpaces {
        text.len()
    } else {
        text.trim_end_matches(' ').len()
    }
}

/// Whether the breaks that `overflow-wrap: anywhere` and `word-break: break-all` (or
/// `break-word`) add inside the words of `style` count for its min-content width (CSS Text 3,
/// §5.5): those of `overflow-wrap: break-word` do not.
fn breaks_inside_for_min_content(style: &ComputedStyle) -> bool {
    style.white_space.wraps()
        && (style.overflow_wrap == OverflowWrap::Anywhere
            || matches!(style.word_break, WordBreak::BreakAll | WordBreak::BreakWord))
}

/// The width of the glyphs of a text piece before byte `offset` of its text.
fn width_before(piece: &Piece, offset: usize) -> f32 {
    match &piece.kind {
        PieceKind::Text { glyphs, .. } => glyphs
            .iter()
            .filter(|glyph| (glyph.cluster.0 as usize) < offset)
            .map(|glyph| glyph.advance)
            .sum(),
        _ => piece.width,
    }
}

/// The pieces of a paragraph not yet taken into the queue of [`lines_in_flow`].
#[derive(Clone)]
struct Later<'a> {
    pieces: &'a [Piece],
    /// How far outside markers stand out of the line box at its left and right, and whether
    /// they stand on the right (the list item is right to left).
    insets: Option<(Au, Au)>,
    rtl: bool,
}

impl Later<'_> {
    /// How many pieces the queue holds at least while there are more.
    const AHEAD: usize = 64;

    /// Takes pieces into `queue` until it holds `count` (or all).
    fn fill(&mut self, queue: &mut VecDeque<Piece>, count: usize) {
        while queue.len() < count
            && let Some((piece, rest)) = self.pieces.split_first()
        {
            self.pieces = rest;
            let mut piece = piece.clone();
            if let Some(insets) = self.insets
                && let PieceKind::Marker {
                    start, inside: false, ..
                } = &mut piece.kind
            {
                let inset = if self.rtl { insets.1 } else { insets.0 };
                *start -= inset.to_px();
            }
            queue.push_back(piece);
        }
    }

    /// Takes into `queue` the pieces of the run from piece `from` that cannot break, so that
    /// [`first_run_width`] and [`first_run_height`] see all of it.
    fn take_run(&mut self, queue: &mut VecDeque<Piece>, from: usize) {
        let mut index = from;
        while let Some(piece) = self.get(queue, index) {
            if piece.forced_break_after || piece.break_after {
                break;
            }
            index += 1;
        }
    }

    /// Piece `index` of `queue`, taken into it if it is not there yet.
    fn get<'q>(&mut self, queue: &'q mut VecDeque<Piece>, index: usize) -> Option<&'q Piece> {
        if index >= queue.len() {
            self.fill(queue, index + 1 + Self::AHEAD);
        }
        queue.get(index)
    }
}

/// Parts of words longer than this many bytes that lines may break inside are cut into shorter
/// ones (see [`cut_long_parts`]).
const LONG_TEXT: usize = 4096;

/// [`width_before`] for offsets in ascending order, summed as they go: the glyphs are walked once
/// for all of them, not once for each (a word of thousands of letters that breaks between any
/// two of them).
struct WidthsBefore<'a> {
    piece: &'a Piece,
    /// Whether the glyphs stand in the order of their text (left-to-right text), so that those
    /// before an offset are the first ones, in the order [`width_before`] sums them.
    ordered: bool,
    /// The glyphs summed so far, and their width.
    next: usize,
    sum: f32,
}

impl<'a> WidthsBefore<'a> {
    fn new(piece: &'a Piece) -> Self {
        let ordered = match &piece.kind {
            PieceKind::Text { glyphs, .. } => glyphs
                .windows(2)
                .all(|pair| pair[0].cluster.0 <= pair[1].cluster.0),
            _ => false,
        };
        Self {
            piece,
            ordered,
            next: 0,
            sum: 0.0,
        }
    }

    /// The width of the glyphs before `offset`, which is no smaller than the offset before.
    fn at(&mut self, offset: usize) -> f32 {
        let PieceKind::Text { glyphs, .. } = &self.piece.kind else {
            return self.piece.width;
        };
        if !self.ordered {
            return width_before(self.piece, offset);
        }
        while let Some(glyph) = glyphs.get(self.next)
            && (glyph.cluster.0 as usize) < offset
        {
            self.sum += glyph.advance;
            self.next += 1;
        }
        self.sum
    }
}

/// Whether a line may break after a piece.
fn is_opportunity(piece: &Piece) -> bool {
    piece.forced_break_after || piece.break_after
}

/// How much wider a piece is where a line breaks after it than in the middle of a line (see
/// [`Broken`]).
/// The width of the first run of `queue` from `from` on that cannot break: what a line that
/// starts there must have room for.
fn first_run_width(queue: &VecDeque<Piece>, from: usize) -> f32 {
    let mut width = 0.0;
    for piece in queue.iter().skip(from) {
        width += piece.width;
        if piece.forced_break_after || piece.break_after {
            width += piece.hyphen - piece.trailing_space + broken_delta(piece);
            break;
        }
    }
    width
}

/// The height of the tallest atomic inline in the first run of `queue` from `from` that cannot
/// break, margins included; 0 for text alone.
/// The height of the tallest atomic inline among the first `count` pieces of `queue`.
fn line_atomic_height(queue: &VecDeque<Piece>, count: usize) -> f32 {
    queue
        .iter()
        .take(count)
        .filter_map(|piece| match &piece.kind {
            PieceKind::Atomic { height, .. } => Some(*height),
            _ => None,
        })
        .fold(0.0, f32::max)
}

fn first_run_height(queue: &VecDeque<Piece>, from: usize) -> f32 {
    let mut height: f32 = 0.0;
    for piece in queue.iter().skip(from) {
        if let PieceKind::Atomic { height: atomic, .. } = &piece.kind {
            height = height.max(*atomic);
        }
        if piece.forced_break_after || piece.break_after {
            break;
        }
    }
    height
}

fn broken_delta(piece: &Piece) -> f32 {
    piece
        .broken
        .as_ref()
        .map_or(0.0, |broken| broken.end.width - piece.width)
}

/// Shapes a word again where a line breaks after piece `end - 1` of `queue` (see [`Broken`]):
/// the piece takes its broken form and the next piece loses the text that form took from it.
/// Returns how many bytes that was, where the next line starts in the next piece's text.
fn take_broken(
    fonts: &FontDb,
    cache: &spacing::FontCache,
    queue: &mut VecDeque<Piece>,
    end: usize,
) -> Option<usize> {
    let into = queue[end - 1].broken.as_ref()?.into_next;
    if into > 0
        && !matches!(&queue[end].kind, PieceKind::Text { text, .. } if text.len() > into && text.is_char_boundary(into))
    {
        return None;
    }
    let before = &mut queue[end - 1];
    let broken = before.broken.take()?;
    *before = Piece {
        break_after: before.break_after,
        forced_break_after: before.forced_break_after,
        break_clear: before.break_clear,
        hyphen: before.hyphen,
        last_word_hyphenation: before.last_word_hyphenation,
        edges: before.edges,
        ..broken.end
    };
    if into > 0 {
        queue[end] = split_piece(fonts, cache, &queue[end], into).1;
    }
    Some(into)
}

/// A position on a line in layout units. Positions on a line add up widths in layout units in
/// Chrome, which single precision may miss by a little: a position within a rounding error of a
/// unit is that unit, before the rest of a unit is cut as [`Au::from_px`] cuts it.
fn line_units(px: f32) -> Au {
    Au::from_px(px + px.signum() / 4096.0)
}

/// Whether `next` goes on with the text of `piece` in one of Blink's inline items: both are
/// text of the same style (a text node, or the text of an element between its children) at the
/// same bidi level, with no forced break between them. Blink lays out each item (the part of it
/// on a line) as wide as its glyphs, rounded up to layout units (`ShapeResult::SnappedWidth`),
/// so what follows the item starts there.
fn same_item(piece: &Piece, next: &Piece) -> bool {
    matches!(piece.kind, PieceKind::Text { .. })
        && matches!(next.kind, PieceKind::Text { .. })
        && Arc::ptr_eq(&piece.style, &next.style)
        && piece.bidi == next.bidi
        && !piece.forced_break_after
}

/// A width rounded up to layout units (1/64 pixel), as Blink rounds the width of an inline item
/// (`ShapeResult::SnappedWidth`). A width a rounding error of single precision past a unit
/// stays at the unit.
fn snapped_width(width: f32) -> f32 {
    (width * 64.0 - 1.0 / 256.0).ceil() / 64.0
}

/// Widens the last piece of each of Blink's inline items on a line (see [`same_item`]), and its
/// last glyph, by what rounding the item's width up to layout units adds, so that what follows
/// the item starts where it does in Chrome; right to left, where its first piece stands on the
/// right, that piece at its right end, after which Blink's item ends too. The spaces that hang
/// at the end of the line do not
/// count; with `before_hanging`, the room goes before them, where Blink's item ends (which tells
/// how far a justified line reaches). Returns the pieces widened, by how much and the spaces
/// the room went before (see [`unsnap_items`]).
fn snap_items(pieces: &mut [Piece], before_hanging: bool) -> Vec<(usize, f32, f32)> {
    let last = pieces.len().saturating_sub(1);
    let mut snaps = Vec::new();
    let mut width = 0.0;
    // The first piece of the item, once one has started.
    let mut start = None;
    for index in 0..pieces.len() {
        if !matches!(pieces[index].kind, PieceKind::Text { .. }) {
            width = 0.0;
            start = None;
            continue;
        }
        let first = *start.get_or_insert(index);
        width += pieces[index].width;
        if index < last && same_item(&pieces[index], &pieces[index + 1]) {
            continue;
        }
        let hanging = if index == last {
            pieces[index].trailing_space
        } else {
            0.0
        };
        let snap = snapped_width(width - hanging) - (width - hanging);
        // Left-to-right text in a right-to-left paragraph loses the spaces that hang at its end
        // to the other side of the line (UAX #9, rule L1; see `split_hanging`): the room goes
        // before them, where Blink's item ends, or it would move the text with them.
        let bidi = pieces[index].bidi;
        let moved = bidi.paragraph_rtl() && !bidi.is_rtl();
        let hanging = if before_hanging || moved { hanging } else { 0.0 };
        let target = if bidi.is_rtl() { first } else { index };
        widen_item_end(&mut pieces[target], snap, hanging);
        snaps.push((target, snap, hanging));
        width = 0.0;
        start = None;
    }
    snaps
}

/// Takes back what [`snap_items`] added, for the items to be rounded again once justification
/// has widened them: Blink rounds the width of each item up to layout units after it has
/// expanded its glyphs (`JustifyResults`), so that an item can end up to a layout unit further
/// than the room justification gave it, and a line of several items reach past its box (which
/// makes Chrome shrink a printed page by that much).
fn unsnap_items(pieces: &mut [Piece], snaps: &[(usize, f32, f32)]) {
    for &(index, snap, hanging) in snaps {
        widen_item_end(&mut pieces[index], -snap, hanging);
    }
}

/// Widens a piece by `by` at the end of its item: after its last glyph, where the line places
/// what follows by the glyphs' advances, or, where `hanging` of spaces hang at its end, after
/// the last glyph before them, where Blink's item ends (left to right).
fn widen_item_end(piece: &mut Piece, by: f32, hanging: f32) {
    piece.width += by;
    let rtl = piece.bidi.is_rtl();
    if let PieceKind::Text { glyphs, .. } = &mut piece.kind {
        let mut target = glyphs.len().checked_sub(1);
        if hanging > 0.0 && !rtl {
            // The glyphs of the hanging spaces, from the end.
            let mut spaces = 0.0;
            let mut index = glyphs.len();
            while index > 0 && spaces + FIT_TOLERANCE < hanging {
                index -= 1;
                spaces += glyphs[index].advance;
            }
            if index > 0 {
                target = Some(index - 1);
            }
        }
        if let Some(glyph) = target.and_then(|index| glyphs.get_mut(index)) {
            glyph.advance += by;
        }
    }
}

/// Splits the room `free` that justification adds to a line over `count` places as Blink does
/// (`ShapeResultSpacing`): the room in layout units, an equal share in units of 1/65536 pixel
/// cut toward zero for each place but the last, which takes what is left.
fn expansion_shares(free: f32, count: usize) -> (f32, f32) {
    const UNIT: f64 = 65536.0;
    let total = (f64::from(free) * 64.0).round() * (UNIT / 64.0);
    let count = count.max(1) as f64;
    let share = (total / count).trunc();
    let last = total - share * (count - 1.0);
    ((share / UNIT) as f32, (last / UNIT) as f32)
}

/// The width of a line of pieces as breaking lines measures it: without the spaces that hang at
/// its end, with the hyphen of a break at a hyphenation point, and with each of Blink's inline
/// items but the last rounded up to layout units (see [`snap_items`]).
fn line_width(pieces: &[&Piece]) -> f32 {
    let last = pieces.last().map_or(0.0, |piece| {
        piece.hyphen - piece.trailing_space + broken_delta(piece)
    });
    let mut width = 0.0;
    let mut item = 0.0;
    for (index, piece) in pieces.iter().enumerate() {
        width += piece.width;
        if !matches!(piece.kind, PieceKind::Text { .. }) {
            item = 0.0;
            continue;
        }
        item += piece.width;
        if let Some(next) = pieces.get(index + 1)
            && !same_item(piece, next)
        {
            width += snapped_width(item) - item;
            item = 0.0;
        }
    }
    width + last
}

/// Greedy lines at break opportunities alone of the pieces in `range`, the first of them line
/// `first_line` of a block whose lines have `room`: for each line the index after its last
/// piece and its width. `None` where a word does not fit, a float other than the one the block
/// starts with takes room or a break is forced before the end of the range: such paragraphs
/// keep greedy lines.
fn greedy_breaks(
    pieces: &VecDeque<Piece>,
    range: Range<usize>,
    room: &score::Room,
    first_line: usize,
) -> Option<Vec<(usize, f32)>> {
    let mut lines = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let available = room.available(first_line + lines.len());
        let (mut used, mut end) = (0.0, start);
        let mut run = 0.0;
        // Each of Blink's inline items is rounded up to layout units, as breaking lines does,
        // but the one a line ends with.
        let mut item = 0.0;
        for (index, piece) in pieces.range(start..range.end).enumerate() {
            let index = start + index;
            if matches!(piece.kind, PieceKind::Float { .. }) && room.float != Some(index) {
                return None;
            }
            run += piece.width;
            let mut snap = 0.0;
            if matches!(piece.kind, PieceKind::Text { .. }) {
                item += piece.width;
                if !pieces.get(index + 1).is_some_and(|next| same_item(piece, next)) {
                    snap = snapped_width(item) - item;
                    run += snap;
                    item = 0.0;
                }
            } else {
                item = 0.0;
            }
            // The last word of the paragraph is hyphenated only where it starts the line.
            let after_words = end > start && !pieces[end - 1].last_word_hyphenation;
            let breakable = is_opportunity(piece) && !(piece.last_word_hyphenation && after_words);
            if breakable || index + 1 == range.end {
                if used + run - snap - piece.trailing_space + piece.hyphen + broken_delta(piece)
                    > available + FIT_TOLERANCE
                {
                    break;
                }
                used += run;
                run = 0.0;
                end = index + 1;
                if piece.forced_break_after && end < range.end {
                    return None;
                }
            }
        }
        if end == start {
            return None;
        }
        lines.push((end, line_width(&pieces.range(start..end).collect::<Vec<_>>())));
        start = end;
    }
    Some(lines)
}

/// Hyphenates the rest of a word again where a line broke inside it at a hyphenation point,
/// as Chrome hyphenates the word that starts a line from there (Blink's `LineBreaker::Hyphenate`
/// looks at the text from the line's start): the points of the whole word that the rest of it
/// alone has not are no longer places to break, and those that only the rest has are, splitting
/// its pieces there. `last` is the last piece of the line before, `queue` the pieces from the
/// start of the line.
fn rehyphenate(
    fonts: &FontDb,
    cache: &spacing::FontCache,
    last: Option<&Piece>,
    queue: &mut VecDeque<Piece>,
) {
    let text_of = |piece: &Piece| match &piece.kind {
        PieceKind::Text { text, .. } => Some(text.clone()),
        _ => None,
    };
    let hyphenated = |piece: &Piece| {
        piece.style.hyphens == Hyphens::Auto
            && !matches!(
                piece.style.word_break,
                WordBreak::BreakAll | WordBreak::AutoPhrase
            )
    };
    let Some(before) = last.filter(|piece| hyphenated(piece)).and_then(text_of) else {
        return;
    };
    let Some(lang) = last.and_then(|piece| piece.style.lang.clone()) else {
        return;
    };
    if !before.chars().next_back().is_some_and(char::is_alphabetic) {
        return;
    }
    // The pieces of the rest of the word: text that goes on with letters on both sides.
    let mut word = String::new();
    let mut ends = Vec::new();
    let mut previous = before;
    for piece in queue.iter() {
        let Some(text) = text_of(piece).filter(|_| hyphenated(piece)) else {
            break;
        };
        let joined = previous.chars().next_back().is_some_and(char::is_alphabetic)
            && text.chars().next().is_some_and(char::is_alphabetic);
        if !joined {
            break;
        }
        word.push_str(&text);
        ends.push(word.len());
        previous = text;
    }
    let Some(first) = queue.front() else {
        return;
    };
    let points = fagus_text::hyphenation_points_limited(&word, &lang, first.style.hyphenate_limit_chars);
    for (piece, end) in queue.iter_mut().zip(&ends).take(ends.len().saturating_sub(1)) {
        if piece.break_after && !points.contains(end) {
            piece.break_after = false;
            piece.hyphen = 0.0;
        }
    }
    // The points inside pieces, from the last piece back so that the indexes stay.
    for index in (0..ends.len()).rev() {
        let start = index.checked_sub(1).map_or(0, |before| ends[before]);
        let inside: Vec<usize> = points
            .iter()
            .filter(|&&point| point > start && point < ends[index])
            .map(|point| point - start)
            .collect();
        let Some(mut rest) = (!inside.is_empty()).then(|| queue.remove(index)).flatten() else {
            continue;
        };
        let hyphen = hyphen_piece(fonts, &rest)
            .iter()
            .map(|piece| piece.width)
            .sum::<f32>();
        let mut parts = Vec::with_capacity(inside.len() + 1);
        let mut done = 0;
        for at in inside {
            let (mut first, second) = split_piece(fonts, cache, &rest, at - done);
            first.hyphen = hyphen;
            first.last_word_hyphenation = false;
            parts.push(first);
            rest = second;
            done = at;
        }
        parts.push(rest);
        for (offset, part) in parts.into_iter().enumerate() {
            queue.insert(index + offset, part);
        }
    }
}

/// Splits a text piece at byte `offset` of its text, shaping both parts again with its face and
/// spacing them as the piece was (`text-spacing-trim`, `text-autospace`). The first part ends
/// the line; the second keeps what the piece allowed after it.
fn split_piece(fonts: &FontDb, cache: &spacing::FontCache, piece: &Piece, offset: usize) -> (Piece, Piece) {
    let PieceKind::Text { face, text, caps, .. } = &piece.kind else {
        unreachable!("only text pieces break inside");
    };
    let mut first = shape_run(
        fonts,
        &piece.style,
        (*face, piece.bidi),
        &text[..offset],
        *caps,
        (None, &Around::default()),
    );
    first.break_after = true;
    let before = letter_spacing::ends_cursive(&text[..offset]);
    let mut rest = shape_run(
        fonts,
        &piece.style,
        (*face, piece.bidi),
        &text[offset..],
        *caps,
        (before, &Around::default()),
    );
    rest.break_after = piece.break_after;
    rest.forced_break_after = piece.forced_break_after;
    rest.break_clear = piece.break_clear;
    rest.hyphen = piece.hyphen;
    rest.last_word_hyphenation = piece.last_word_hyphenation;
    let mut halves = [first, rest];
    spacing::respace_split(fonts, cache, piece, &mut halves);
    let [first, mut rest] = halves;
    // Where the piece itself is shaped again to end a line, so is the rest of it.
    rest.broken = piece.broken.as_ref().and_then(|broken| {
        let PieceKind::Text { text: whole, .. } = &broken.end.kind else {
            return None;
        };
        broken_form(fonts, &rest, whole.get(text.len()..)?)
    });
    (first, rest)
}

/// Whether a line breaks after `piece` at a hyphenation point inside a word (not at a soft
/// hyphen, which ends a word as the other break opportunities do).
fn hyphenation_point(piece: &Piece) -> bool {
    piece.hyphen > 0.0 && matches!(&piece.kind, PieceKind::Text { text, .. } if !text.ends_with('\u{AD}'))
}

/// The width of the punctuation (anything but letters, digits and spaces) at the end of the
/// word that `piece` ends, before its trailing spaces, where it is two characters or more.
fn trailing_punctuation(piece: &Piece) -> Option<f32> {
    let PieceKind::Text { text, glyphs, .. } = &piece.kind else {
        return None;
    };
    let word = text.trim_end_matches(char::is_whitespace);
    let start = word
        .trim_end_matches(|character: char| !character.is_alphanumeric())
        .len();
    (word[start..].chars().count() >= 2).then(|| {
        glyphs
            .iter()
            .filter(|glyph| (start..word.len()).contains(&(glyph.cluster.0 as usize)))
            .map(|glyph| glyph.advance)
            .sum()
    })
}

/// Where a run of pieces that does not fit in `room` breaks inside a word: the index of a piece
/// and a byte offset in its text (0 for a break before the piece). Between letters with
/// `word-break: break-all`, as late as fits; where nothing before the run on the line can break
/// (`alone`), also anywhere `overflow-wrap` allows. Alone, the line holds at least the start of
/// the run up to the first such place, even if that does not fit.
fn break_run(
    fonts: &FontDb,
    pieces: &VecDeque<Piece>,
    run: Range<usize>,
    room: f32,
    alone: bool,
) -> Option<(usize, usize)> {
    let alone_inside = |how, force| {
        alone
            .then(|| break_inside(fonts, pieces, run.clone(), room, how, force))
            .flatten()
    };
    // A phrase that does not fit breaks as with `word-break: normal`, where it fits, else where
    // `overflow-wrap` lets it, else at its first opportunity.
    break_inside(fonts, pieces, run.clone(), room, InnerBreaks::Letters, alone)
        .or_else(|| alone_inside(InnerBreaks::Phrase, false))
        .or_else(|| alone_inside(InnerBreaks::Overflow, true))
        .or_else(|| alone_inside(InnerBreaks::Phrase, true))
}

/// Whether a line may break between two pieces of a phrase of `word-break: auto-phrase` that
/// does not fit (see [`InnerBreaks::Phrase`]): where `word-break: normal` breaks between them,
/// or at a soft hyphen (whose piece has the width of its hyphen, but no break after it).
fn breaks_between_as_normal(before: &Piece, after: &Piece) -> bool {
    if before.hyphen > 0.0 && !before.break_after && before.style.word_break == WordBreak::AutoPhrase {
        return true;
    }
    let (PieceKind::Text { text: first, .. }, PieceKind::Text { text: next, .. }) =
        (&before.kind, &after.kind)
    else {
        return false;
    };
    let (Some(last), Some(start)) = (first.chars().next_back(), next.chars().next()) else {
        return false;
    };
    let pair = format!("{last}{start}");
    fagus_text::break_opportunities_with(&pair, crate::breaks::strictness(&after.style))
        .iter()
        .any(|opportunity| opportunity.offset == last.len_utf8())
}

/// The last place in `run` where the line can break in the way `how` so that what comes before
/// fits in `room`, or with `force`, the first place if none fits.
fn break_inside(
    fonts: &FontDb,
    pieces: &VecDeque<Piece>,
    run: Range<usize>,
    room: f32,
    how: InnerBreaks,
    force: bool,
) -> Option<(usize, usize)> {
    let mut used = 0.0;
    let mut best = None;
    let mut first = None;
    for index in run.clone() {
        let piece = &pieces[index];
        // `overflow-wrap` breaks at any point, between pieces too, but not before a piece of
        // nothing but spaces that hang: they stay at the end of the line before, which breaks
        // after them, as in Chrome, instead of making a line of their own.
        let mut offsets = how.positions(piece);
        let spaces = matches!(&piece.kind, PieceKind::Text { text, .. } if !text.is_empty())
            && before_hanging_spaces(piece) == 0;
        if how == InnerBreaks::Overflow
            && index > run.start
            && matches!(piece.kind, PieceKind::Text { .. })
            && !spaces
            && how.allows(&piece.style)
        {
            offsets.insert(0, 0);
        }
        if how == InnerBreaks::Phrase
            && index > run.start
            && how.allows(&piece.style)
            && breaks_between_as_normal(&pieces[index - 1], piece)
        {
            offsets.insert(0, 0);
        }
        let mut widths = WidthsBefore::new(piece);
        for offset in offsets {
            first.get_or_insert((index, offset));
            // The space of `text-autospace` before the break goes with the end of the line.
            let before = widths.at(offset) - spacing::autospace_at(fonts, piece, offset);
            if used + before > room + FIT_TOLERANCE {
                break;
            }
            best = Some((index, offset));
        }
        used += piece.width;
        // Forced, the line holds the run up to the first place, in whichever piece.
        if used > room + FIT_TOLERANCE && (!force || first.is_some()) {
            break;
        }
    }
    best.or(if force { first } else { None })
}

fn first_style(items: &[InlineItem]) -> Option<&Arc<ComputedStyle>> {
    items.iter().find_map(|item| match item {
        InlineItem::Text { style, .. }
        | InlineItem::Open { style, .. }
        | InlineItem::NamedString { style, .. }
        | InlineItem::PageNumber { style, .. }
        | InlineItem::TargetPage { style, .. }
        | InlineItem::Leader { style, .. } => Some(style),
        _ => None,
    })
}

fn empty_piece(fonts: &FontDb, style: &Arc<ComputedStyle>) -> Piece {
    Piece {
        kind: PieceKind::Text {
            face: face_for(fonts, style).unwrap_or_else(|| {
                fonts
                    .query(&[], 400.0, fagus_text::FontStyle::Normal)
                    .expect("a font")
            }),
            glyphs: Vec::new(),
            text: String::new(),
            caps: Caps::NONE,
        },
        style: Arc::clone(style),
        width: 0.0,
        min_width: 0.0,
        trailing_space: 0.0,
        break_after: true,
        forced_break_after: true,
        break_clear: fagus_style::values::Clear::None,
        hyphen: 0.0,
        last_word_hyphenation: false,
        bidi: Level::LTR,
        edges: spacing::Edges::default(),
        broken: None,
    }
}

/// A run of text shaped with one face, on a line of its own (see [`shape_line`]).
pub(crate) struct ShapedRun {
    pub face: FaceId,
    /// The size its glyphs are set at, in pixels.
    pub size: f32,
    pub glyphs: Vec<Glyph>,
    pub text: String,
    pub width: f32,
}

/// Shapes `text` of `style` as one line that does not break, in the style's direction, with the
/// faces that have its characters: the text of a `MathML` token element, which Chrome lays out
/// as a line of its own. The runs stand in the order they go on the page, left to right.
pub(crate) fn shape_line(fonts: &FontDb, style: &Arc<ComputedStyle>, text: &str) -> Vec<ShapedRun> {
    fagus_dom::budget::check_time();
    let Some(face) = face_for(fonts, style) else {
        return Vec::new();
    };
    let size = used_font_size(fonts, style);
    let rtl = style.is_rtl();
    crate::bidi::visual_runs(text, rtl)
        .into_iter()
        .flat_map(|(range, own)| {
            let level = Level {
                level: own,
                paragraph: u8::from(rtl),
            };
            // Right to left, the last piece of a run stands leftmost.
            let mut pieces = shape_segment(fonts, style, face, &text[range], TextLevels::uniform(level));
            if level.is_rtl() {
                pieces.reverse();
            }
            pieces
        })
        .filter_map(|piece| match piece.kind {
            PieceKind::Text {
                face,
                glyphs,
                text,
                caps,
            } => Some(ShapedRun {
                face,
                size: caps.size(size),
                glyphs,
                text,
                width: piece.width,
            }),
            _ => None,
        })
        .collect()
}

/// Whether a control character is drawn with a glyph that font fallback finds, as Chrome on
/// macOS draws a line tabulation or a form feed with the blank third of an em that Hiragino maps
/// the C0 controls to: those but tabs, line feeds and carriage returns.
fn drawn_control(character: char) -> bool {
    matches!(character, '\u{1}'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}')
}

/// Shapes a segment, splitting it into runs of the faces that have its characters and of its
/// bidi levels.
fn shape_segment(
    fonts: &FontDb,
    style: &Arc<ComputedStyle>,
    preferred: FaceId,
    segment: &str,
    levels: TextLevels<'_>,
) -> Vec<Piece> {
    let mut fallbacks = fagus_text::FallbackRun::default();
    fallbacks.start(segment);
    shape_segment_in(
        fonts,
        style,
        preferred,
        (segment, levels),
        &Around::default(),
        &mut fallbacks,
    )
}

/// [`shape_segment`] for a segment of a longer text, whose earlier characters fell back to the
/// faces in `fallbacks` (see [`FontDb::fallback_in_run`]), with the text `around` it that its
/// letters join with.
fn shape_segment_in(
    fonts: &FontDb,
    style: &Arc<ComputedStyle>,
    preferred: FaceId,
    (segment, levels): (&str, TextLevels<'_>),
    around: &Around,
    fallbacks: &mut fagus_text::FallbackRun,
) -> Vec<Piece> {
    let weight = style.font_weight;
    let font_style = font_style(style);
    let families: Vec<&str> = style.font_family.iter().map(String::as_str).collect();
    // Runs of characters by face, level and, in vertical lines, whether they stand upright.
    let vertical = style.is_vertical();
    // Each with its start and end, and whether the text before it is of a cursive script, for
    // letter spacing.
    let mut runs: Vec<(RunKey, usize, usize, Option<bool>)> = Vec::new();
    let request = (weight, font_style, style.font_stretch);
    // The face of the emoji sequence that the characters up to this offset belong to.
    let mut sequence: Option<(usize, FaceId)> = None;
    // The face of the base character and combining marks that the characters up to this
    // offset belong to.
    let mut cluster: Option<(usize, FaceId)> = None;
    for (offset, character) in segment.char_indices() {
        if sequence.is_some_and(|(end, _)| offset >= end) {
            sequence = None;
        }
        if cluster.is_some_and(|(end, _)| offset >= end) {
            cluster = None;
        }
        if sequence.is_none()
            && !character.is_ascii_alphabetic()
            && let Some(len) = fagus_text::sequence_len(&segment[offset..])
        {
            let face = fonts
                .fallback_sequence_in(
                    &families,
                    preferred,
                    &segment[offset..offset + len],
                    request,
                    style.lang.as_deref(),
                )
                .unwrap_or(preferred);
            // An emoji shown as text is text to fallback, as it is in Chrome's runs.
            if face != preferred
                && fagus_text::presentation(&segment[offset..offset + len]) == fagus_text::Presentation::Text
            {
                fallbacks.add(face);
            }
            sequence = Some((offset + len, face));
        }
        let face = if let Some((_, face)) = sequence.or(cluster) {
            face
        } else if (character.is_control() && !drawn_control(character))
            || (fagus_text::is_ignorable(character) && !fonts.face(preferred).has_glyph(character))
        {
            // Characters that need no glyph stay in the run before them, as `HarfBuzz` hides
            // them in any font that lacks them.
            runs.last().map_or(preferred, |run| run.0.0)
        } else {
            let face = fonts
                .fallback_in_run(
                    fallbacks,
                    &families,
                    preferred,
                    character,
                    request,
                    style.lang.as_deref(),
                )
                .unwrap_or(preferred);
            let end = offset + character.len_utf8() + marks_len(&segment[offset + character.len_utf8()..]);
            if end > offset + character.len_utf8() {
                let found = cluster_face(fonts, (&families, preferred, face), &segment[offset..end], style);
                cluster = found.map(|found| (end, found));
                found.unwrap_or(face)
            } else {
                face
            }
        };
        let upright = vertical && crate::writing::upright(character, style);
        let key = (face, levels.at(offset), upright);
        match runs.last_mut() {
            Some(run) if run.0 == key => run.2 = offset + character.len_utf8(),
            _ => runs.push((key, offset, offset + character.len_utf8(), fallbacks.cursive())),
        }
        fallbacks.note(character);
    }
    // Text without letters that join, as Latin text, has nothing around it to shape with.
    let joining = !around.is_empty() || fagus_text::has_joining(segment);
    runs.into_iter()
        .flat_map(|((face, level, upright), start, end, before)| {
            let text = &segment[start..end];
            caps_runs(fonts, face, style, text)
                .into_iter()
                .map(move |(range, caps)| {
                    let before = letter_spacing::ends_cursive(&text[..range.start]).or(before);
                    let joined = if joining {
                        around.of(segment, start + range.start..start + range.end)
                    } else {
                        Around::NONE
                    };
                    shape_run(
                        fonts,
                        style,
                        (face, level),
                        &text[range],
                        Caps { upright, ..caps },
                        (before, &joined),
                    )
                })
        })
        .collect()
}

/// The length in bytes of the combining marks at the start of `text`, which belong to the
/// grapheme cluster of the character before them.
fn marks_len(text: &str) -> usize {
    text.chars()
        .take_while(|&character| is_mark(character))
        .map(char::len_utf8)
        .sum()
}

/// Whether a character is a combining mark (general category M); none comes before U+0300, so
/// Latin text is told without looking the category up.
pub(crate) fn is_mark(character: char) -> bool {
    use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};
    character >= '\u{300}' && character.general_category_group() == GeneralCategoryGroup::Mark
}

/// The face that draws a base character and the combining marks after it together, as Chrome
/// keeps a grapheme cluster in one font: that of the base character (`base`, found for it among
/// `families`) where it has the whole cluster, else the first face found for one of the marks
/// that has it (`s` and a combining acute that a Latin-only web font lacks are drawn as `ś` of
/// the next family). `None` where no such face has them all.
fn cluster_face(
    fonts: &FontDb,
    (families, preferred, base): (&[&str], FaceId, FaceId),
    cluster: &str,
    style: &ComputedStyle,
) -> Option<FaceId> {
    if fonts.face(base).has_cluster(cluster) {
        return Some(base);
    }
    let request = (style.font_weight, font_style(style), style.font_stretch);
    // Each face is tried once, for the marks that the base character's face lacks.
    let mut tried = vec![base];
    for mark in cluster.chars().skip(1) {
        if fagus_text::is_variation_selector(mark)
            || fagus_text::is_ignorable(mark)
            || fonts.face(base).has_glyph(mark)
        {
            continue;
        }
        let face = fonts.fallback_in(families, preferred, mark, request, style.lang.as_deref())?;
        if tried.contains(&face) {
            continue;
        }
        if fonts.face(face).has_cluster(cluster) {
            return Some(face);
        }
        tried.push(face);
    }
    None
}

/// The OpenType features text of `style` is shaped with in `face`, before those for capitals
/// and of `font-feature-settings`: those of the face's `@font-face` rule, then those of
/// `font-variant` and `font-kerning`, which override them (CSS Fonts 4, §7.2).
pub(crate) fn variant_features(fonts: &FontDb, face: FaceId, style: &ComputedStyle) -> Vec<([u8; 4], u32)> {
    let mut features = fonts.face(face).features.clone();
    features.extend(style.font_features());
    features
}

/// What keeps the characters of a run of text together: its face, its bidi level, and whether
/// its glyphs stand upright in a vertical line.
type RunKey = (FaceId, Level, bool);

/// Shapes `text` with one face into a piece, set as `caps` says, with the OpenType features of
/// the style: those of `font-variant` and `font-kerning`, then those for capitals, then
/// `font-feature-settings`. `before` says whether the text before it is of a cursive script,
/// where known, which letter spacing leaves out (see [`letter_spacing`]); `around` is the text
/// around it that its letters join with.
fn shape_run(
    fonts: &FontDb,
    style: &Arc<ComputedStyle>,
    (face, level): (FaceId, Level),
    text: &str,
    caps: Caps,
    (before, around): (Option<bool>, &Around),
) -> Piece {
    let size = caps.size(used_font_size(fonts, style));
    let mut features = variant_features(fonts, face, style);
    features.extend_from_slice(caps.features);
    features.extend_from_slice(&style.font_feature_settings);
    // Faces that collapse fullwidth punctuation themselves do so where `text-spacing-trim` lets
    // punctuation collapse.
    if let Some(contextual) = spacing::contextual_feature(style, caps.upright)
        && fonts.face(face).positions(contextual)
    {
        features.push((contextual, 1));
    }
    // Made-up small capitals keep the optical size of the text they stand in for, as in
    // Chrome, which only scales the font down.
    let variations = font_variations(fonts, face, style, used_font_size(fonts, style));
    // The text in the case it is set in, with the offsets of its bytes in `text`.
    let cased = cased(text, caps.case);
    let shaped_text = cased.as_ref().map_or(text, |(cased, _)| cased.as_str());
    let original = |offset: u32| {
        cased.as_ref().map_or(offset, |(_, offsets)| {
            offsets.get(offset as usize).copied().unwrap_or(text.len() as u32)
        })
    };
    // Tabs are shaped as spaces; the line breaker widens them to the next tab stop.
    let rtl = level.is_rtl() && !caps.upright;
    let flow = if caps.upright {
        fagus_text::Flow::TopToBottom
    } else if rtl {
        fagus_text::Flow::RightToLeft
    } else {
        fagus_text::Flow::LeftToRight
    };
    // Text is shaped in its language, which picks a face's forms for it (`locl`).
    let shape = |text: &str| {
        fonts.shape_in_context(
            face,
            text,
            &features,
            &variations,
            (flow, style.lang.as_deref()),
            around.pair(),
        )
    };
    let shaped = if shaped_text.contains('\t') {
        shape(&shaped_text.replace('\t', " "))
    } else if (!around.prefix.is_empty() || !around.suffix.is_empty()) && cased.is_none() {
        // Shaped with the rest of its word in other items, keeping the glyphs of its own
        // characters.
        let (prefix, suffix) = (&around.prefix, &around.suffix);
        let whole = shape(&format!("{prefix}{shaped_text}{suffix}"));
        let offset = |at: usize| u32::try_from(at).unwrap_or(u32::MAX);
        let own = offset(prefix.len())..offset(prefix.len() + shaped_text.len());
        let glyphs: Vec<fagus_text::ShapedGlyph> = whole
            .glyphs
            .iter()
            .filter(|glyph| own.contains(&glyph.cluster))
            .map(|glyph| fagus_text::ShapedGlyph {
                cluster: glyph.cluster - own.start,
                ..*glyph
            })
            .collect();
        let advance = glyphs.iter().map(|glyph| glyph.x_advance).sum();
        Arc::new(fagus_text::ShapedText { glyphs, advance })
    } else {
        shape(shaped_text)
    };
    let scale = fonts.face(face).scale(size);
    let letter_spacing = style.letter_spacing;
    let word_spacing = style.word_spacing;
    // Whether each byte of the text is in a run of a cursive script.
    let cursive = if letter_spacing == 0.0 {
        Vec::new()
    } else {
        letter_spacing::cursive_bytes(text, before)
    };
    // Upright glyphs are placed from the alphabetic baseline of the run (that of the style's
    // first face): the middle of their face's ascent and descent lies on the middle of the line,
    // as in Chrome. A face without vertical metrics advances its upright glyphs by its ascent
    // and descent, and hangs them from its ascent.
    let upright = caps.upright.then(|| face_metrics(fonts, face, size));
    let across = upright.map_or(0.0, |metrics| (metrics.ascent - metrics.descent) / 2.0);
    let synthesized = upright.filter(|_| !fonts.face(face).vertical_metrics);
    let tracking = apple_emoji_tracking(fonts.face(face), size);
    // The tracking of faces with a `trak` table at the size, which shaping applied at 12pt.
    let trak = if caps.upright {
        0.0
    } else {
        fonts.face(face).tracking(size)
    };
    let mut glyphs: Vec<Glyph> = Vec::with_capacity(shaped.glyphs.len());
    // Right to left, the glyphs come last character first: a cluster ends where the one
    // before it in the glyphs starts. All glyphs of a cluster of several (a conjunct with a
    // vowel sign drawn before it) span all of its text, which the PDF gives them together.
    // Found for all glyphs in one pass: a cluster of thousands of marks has as many glyphs.
    let count = shaped.glyphs.len();
    let mut next_clusters: Vec<Option<u32>> = vec![None; count];
    let order: Box<dyn Iterator<Item = (usize, usize)>> = if rtl {
        Box::new((1..count).map(|index| (index, index - 1)))
    } else {
        Box::new((0..count.saturating_sub(1)).rev().map(|index| (index, index + 1)))
    };
    for (index, next) in order {
        next_clusters[index] = if shaped.glyphs[next].cluster == shaped.glyphs[index].cluster {
            next_clusters[next]
        } else {
            Some(shaped.glyphs[next].cluster)
        };
    }
    let next_cluster = |index: usize| next_clusters[index].map_or(text.len() as u32, original);
    for (index, glyph) in shaped.glyphs.iter().enumerate() {
        let cluster = original(glyph.cluster);
        let cluster_end = next_cluster(index);
        // Word spacing goes to spaces and no-break spaces, as in Blink (`TreatAsSpace`).
        let is_space = text
            .get(cluster as usize..)
            .is_some_and(|rest| rest.starts_with([' ', '\u{a0}']));
        let (natural, along) = match synthesized {
            Some(metrics) => (metrics.ascent + metrics.descent, metrics.ascent),
            None if caps.upright => (glyph.x_advance * scale, glyph.x_offset * scale),
            None => (
                fonts.face(face).layout_advance(glyph, size),
                glyph.x_offset * scale,
            ),
        };
        let natural = match tracking {
            Some(tracking) if natural > 0.0 => (natural + tracking + 0.5).floor(),
            _ => natural,
        };
        // Tracking goes to the first glyph of each cluster.
        let first_of_cluster = index == 0 || shaped.glyphs[index - 1].cluster != glyph.cluster;
        let natural = if first_of_cluster { natural + trak } else { natural };
        // Letter spacing goes after the last glyph of a cluster (in the order of the glyphs), as
        // in Blink, where its character takes it.
        let last_of_cluster = shaped
            .glyphs
            .get(index + 1)
            .is_none_or(|next| next.cluster != glyph.cluster);
        let mut advance = natural;
        if letter_spacing != 0.0
            && last_of_cluster
            && text
                .get(cluster as usize..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|character| {
                    letter_spacing::spaced(character, cursive.get(cluster as usize).copied().unwrap_or(false))
                })
        {
            advance += letter_spacing;
        }
        // Word spacing too, after the marks on a space (its grapheme cluster in Blink's
        // `ApplySpacing`): a mark on a space stays above it.
        let space_mark = !rtl
            && text
                .get(cluster as usize..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(is_mark)
            && text[..cluster as usize]
                .trim_end_matches(is_mark)
                .ends_with([' ', '\u{a0}']);
        let marked_next = |index: usize| {
            !rtl && shaped.glyphs.get(index + 1).is_some_and(|next| {
                text.get(original(next.cluster) as usize..)
                    .and_then(|rest| rest.chars().next())
                    .is_some_and(is_mark)
            })
        };
        if (is_space || space_mark) && last_of_cluster && !marked_next(index) {
            advance += word_spacing;
        }
        glyphs.push(Glyph {
            id: glyph.id,
            advance,
            x_offset: along,
            y_offset: glyph.y_offset * scale + across,
            cluster: (cluster, cluster_end.max(cluster)),
        });
    }
    let width: f32 = glyphs.iter().map(|glyph| glyph.advance).sum();
    let trailing_space = trailing_space(&glyphs, text, style);
    Piece {
        kind: PieceKind::Text {
            face,
            glyphs,
            text: text.to_owned(),
            caps,
        },
        style: Arc::clone(style),
        width,
        min_width: width,
        trailing_space,
        break_after: false,
        forced_break_after: false,
        break_clear: fagus_style::values::Clear::None,
        hyphen: 0.0,
        last_word_hyphenation: false,
        bidi: level,
        edges: spacing::Edges::default(),
        broken: None,
    }
}

/// The tracking that Chrome on macOS gives the glyphs of Apple Color Emoji at `size`, whose
/// advances it then rounds to whole pixels (Core Text's advances for the font, measured): a
/// quarter em up to 16px, falling to none at 24px. `None` for other faces.
fn apple_emoji_tracking(face: &fagus_text::Face, size: f32) -> Option<f32> {
    if !face.color || face.family != "Apple Color Emoji" {
        return None;
    }
    Some(if size <= 16.0 {
        size / 4.0
    } else {
        ((24.0 - size) / 2.0).max(0.0)
    })
}

/// A node of a line's box tree: the root inline box of the container, or an inline box.
struct Node {
    /// The inline box, or `None` for the root.
    inline_box: Option<usize>,
    /// Left and right edges of the margin box on the line.
    start: f32,
    end: f32,
    /// Whether the box starts or ends on this line.
    first: bool,
    last: bool,
    children: Vec<Child>,
    /// The baseline relative to the parent's (positive = lower), then relative to the root's.
    shift: f32,
    total_shift: f32,
    /// Whether the box's font and line height make room on the line: always, except with the
    /// line height quirk (see [`LineBuilder::quirk_boxes`]).
    has_metrics: bool,
}

enum Child {
    Node(usize),
    Text {
        x: f32,
        width: f32,
        face: FaceId,
        style: Arc<ComputedStyle>,
        glyphs: Vec<Glyph>,
        text: String,
        marker: bool,
        caps: Caps,
        symbol: Option<MarkerSymbol>,
    },
    Atomic {
        x: f32,
        fragment: Box<Fragment>,
        height: f32,
        baseline: f32,
        /// The baseline relative to the parent's, then relative to the root's.
        shift: f32,
        total_shift: f32,
    },
    Placeholder {
        x: f32,
        id: BoxId,
    },
}

/// Whether a line is the first formatted line of its paragraph and whether it is the last, for
/// the punctuation that hangs at its edges (`hanging-punctuation`).
#[derive(Clone, Copy, Debug, Default)]
struct Formatted {
    first: bool,
    last: bool,
}

struct LineBuilder<'a> {
    fonts: &'a FontDb,
    container: &'a ComputedStyle,
    boxes: &'a [InlineBox],
    strut: Metrics,
    /// Whether the line height calculation quirk applies (quirks and limited-quirks mode).
    quirk: bool,
}

impl LineBuilder<'_> {
    fn style_of(&self, node: &Node) -> &ComputedStyle {
        node.inline_box
            .map_or(self.container, |index| &self.boxes[index].style)
    }

    fn metrics_of(&self, node: &Node) -> Metrics {
        match node.inline_box {
            Some(index) => metrics(self.fonts, &self.boxes[index].style),
            None => self.strut,
        }
    }

    /// The boxes of a line (`None` for the root inline box) whose font and line height make room
    /// on it under the line height calculation quirk (Quirks Mode, §3.3), as in Blink: a box
    /// counts where it holds text itself (not the collapsible spaces at the end of the line) or
    /// has a margin, border or padding at a side that is on the line, the root where it holds a
    /// list marker or is a list item; and a forced break counts for the box it is in where the
    /// line holds nothing else. Atomic inlines count as always.
    fn quirk_boxes(&self, pieces: &[Piece], continuing: &[usize]) -> Vec<Option<usize>> {
        let mut stack: Vec<Option<usize>> = std::iter::once(None)
            .chain(continuing.iter().map(|&index| Some(index)))
            .collect();
        let mut boxes = Vec::new();
        if matches!(
            self.container.display,
            fagus_style::values::Display::Normal { list_item: true, .. }
        ) {
            boxes.push(None);
        }
        let blank_end = pieces.last().is_some_and(|piece| {
            matches!(&piece.kind, PieceKind::Text { text, .. } if text.chars().all(|c| c == ' '))
                && piece.style.white_space.collapses_spaces()
        });
        let mut other = false;
        let mut forced_in = None;
        for (index, piece) in pieces.iter().enumerate() {
            let top = stack.last().copied().flatten();
            match &piece.kind {
                PieceKind::Open(inline) | PieceKind::Left { index: inline, .. } => {
                    stack.push(Some(*inline));
                    let edge = !matches!(piece.kind, PieceKind::Left { edge: false, .. });
                    if edge && self.boxes[*inline].margin.0 + self.boxes[*inline].frame.0 != 0.0 {
                        boxes.push(Some(*inline));
                        other = true;
                    }
                }
                PieceKind::Close(inline) | PieceKind::Right { index: inline, .. } => {
                    let edge = !matches!(piece.kind, PieceKind::Right { edge: false, .. });
                    if edge && self.boxes[*inline].margin.1 + self.boxes[*inline].frame.1 != 0.0 {
                        boxes.push(Some(*inline));
                        other = true;
                    }
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                PieceKind::Text { glyphs, .. }
                    if !(glyphs.is_empty() || blank_end && index + 1 == pieces.len()) =>
                {
                    boxes.push(top);
                    other = true;
                }
                PieceKind::Marker { .. } => {
                    boxes.push(None);
                    other = true;
                }
                PieceKind::Atomic { .. } | PieceKind::Leader { .. } => other = true,
                _ => {}
            }
            if piece.forced_break_after {
                forced_in = Some(stack.last().copied().flatten());
            }
        }
        if !other && let Some(inside) = forced_in {
            boxes.push(inside);
        }
        boxes
    }

    /// How far a box reaches above and below its baseline: its ascent and descent with half the
    /// leading each. With `line-height: normal` the faces its text falls back to reach as far
    /// as their own metrics and line height take them, as in Chrome.
    fn reach_of(&self, node: &Node) -> (f32, f32) {
        let own = self.metrics_of(node);
        let (mut above, mut below) = (
            own.ascent + own.leading_above(),
            own.descent + own.leading_below(),
        );
        let style = self.style_of(node);
        if style.line_height == LineHeight::Normal {
            let primary = face_for(self.fonts, style);
            let size = used_font_size(self.fonts, style);
            for child in &node.children {
                if let Child::Text { face, .. } = child
                    && Some(*face) != primary
                {
                    let used = on_baseline(face_metrics(self.fonts, *face, size), style);
                    above = above.max(used.ascent + used.leading_above());
                    below = below.max(used.descent + used.leading_below());
                }
            }
        }
        (above, below)
    }

    /// The style of the ellipsis of the container's lines: the container's, without emphasis
    /// marks or room for them, as in Chrome. Lines place the pieces of this style on their
    /// baseline, outside the inline boxes they are cut in.
    fn ellipsis_style(&self) -> Arc<ComputedStyle> {
        Arc::new(ComputedStyle::from(ComputedValues {
            text_emphasis_style: None,
            ..ComputedValues::from(self.container.clone())
        }))
    }

    /// The text of the ellipsis: the string of `text-overflow` (not for a clamp), or a
    /// horizontal ellipsis where the first available font of the container has one, three
    /// periods where it has not, as in Blink (`LineTruncator::ComputeEllipsisText`).
    fn ellipsis_text(&self, clamp: bool) -> String {
        if let TextOverflow::String(text) = &self.container.text_overflow
            && !clamp
        {
            return text.to_string();
        }
        let ellipsis = face_for(self.fonts, self.container)
            .is_none_or(|face| self.fonts.face(face).has_glyph('\u{2026}'));
        if ellipsis { "\u{2026}" } else { "..." }.to_owned()
    }

    /// Cuts the content of a line that overflows its box with `text-overflow: ellipsis` or a
    /// string (CSS Overflow 4, §5.1): what does not fit before the ellipsis goes, text at the
    /// end of a cluster, atomic inlines whole, and the ellipsis, in `style` (see
    /// [`Inline::ellipsis_style`]), takes its place. As in Chrome, the first character or
    /// atomic inline of the line stays even where only the ellipsis fits, the ellipsis after it
    /// (Blink's `LineTruncator`). `always` ends the line with an ellipsis even if it fits (the
    /// last line of a `line-clamp`). Returns whether the line was cut to make room for it.
    fn truncate(
        &self,
        pieces: &mut Vec<Piece>,
        available: f32,
        always: bool,
        style: &Arc<ComputedStyle>,
    ) -> bool {
        // The ellipsis of a clamp follows the last word, without the spaces after it.
        if always
            && let Some(piece) = pieces.last_mut()
            && piece.trailing_space > 0.0
            && let PieceKind::Text { glyphs, text, .. } = &mut piece.kind
        {
            let end = text.trim_end_matches(' ').len();
            glyphs.retain(|glyph| (glyph.cluster.0 as usize) < end);
            text.truncate(end);
            piece.width -= piece.trailing_space;
            piece.trailing_space = 0.0;
        }
        let hanging = pieces.last().map_or(0.0, |piece| piece.trailing_space);
        let width: f32 = pieces.iter().map(|piece| piece.width).sum::<f32>() - hanging;
        if width <= available + 0.01 && !always {
            return false;
        }
        let mut ellipsis = Vec::new();
        let paragraph = pieces.first().map_or(Level::LTR, |piece| Level {
            level: piece.bidi.paragraph,
            ..piece.bidi
        });
        let text = self.ellipsis_text(always);
        shape_text(
            self.fonts,
            &text,
            style,
            (TextLevels::uniform(paragraph), own_breaks(&text)),
            &mut ellipsis,
        );
        let room = available - ellipsis.iter().map(|piece| piece.width).sum::<f32>();
        // A clamp's ellipsis that fits after the line just follows it.
        if always && width <= room + 0.01 {
            pieces.extend(ellipsis);
            return false;
        }
        // Pieces that take no room (the starts of inline boxes) stay even where the ellipsis
        // alone overflows.
        let mut used = 0.0;
        let mut kept = 0;
        for piece in pieces.iter() {
            if piece.width > 0.0 && used + piece.width > room + 0.01 {
                break;
            }
            used += piece.width;
            kept += 1;
        }
        let content = |piece: &Piece| match &piece.kind {
            PieceKind::Text { glyphs, .. } => !glyphs.is_empty(),
            PieceKind::Atomic { .. } | PieceKind::Measured | PieceKind::Leader { .. } => true,
            _ => false,
        };
        // An empty inline box comes first on a line as Blink lays it out before the rest
        // (`LineTruncator::EllipsizeChild`): what follows is not the first content then.
        let empty_box = pieces[..kept].windows(2).any(|pair| {
            matches!((&pair[0].kind, &pair[1].kind), (PieceKind::Open(open), PieceKind::Close(close)) if open == close)
        });
        let content_kept = empty_box || pieces[..kept].iter().any(content);
        // The first atomic inline of the line stays whole, the ellipsis after it.
        let first_atomic = !content_kept
            && pieces
                .get(kept)
                .is_some_and(|piece| matches!(piece.kind, PieceKind::Atomic { .. } | PieceKind::Measured));
        if first_atomic {
            kept += 1;
        }
        // Of the piece that overflows, the clusters of text that fit.
        let mut cut = None;
        let text_kept = content_kept;
        if !first_atomic
            && let Some(piece) = pieces.get(kept)
            && let PieceKind::Text {
                face,
                glyphs,
                text,
                caps,
            } = &piece.kind
        {
            // Right to left, the glyphs that come first in the text come last.
            let rtl = piece.bidi.is_rtl();
            let glyphs: Vec<Glyph> = if rtl {
                glyphs.iter().rev().copied().collect()
            } else {
                glyphs.clone()
            };
            let mut width = 0.0;
            let mut end = 0;
            while end < glyphs.len() {
                let cluster = glyphs[end].cluster;
                let next = glyphs[end..]
                    .iter()
                    .position(|glyph| glyph.cluster != cluster)
                    .map_or(glyphs.len(), |offset| end + offset);
                let advance: f32 = glyphs[end..next].iter().map(|glyph| glyph.advance).sum();
                if used + width + advance > room + 0.01 && (end > 0 || text_kept) {
                    break;
                }
                width += advance;
                end = next;
            }
            if end > 0 {
                let bytes = glyphs[end - 1].cluster.1 as usize;
                let mut kept_glyphs = glyphs[..end].to_vec();
                if rtl {
                    kept_glyphs.reverse();
                }
                cut = Some(Piece {
                    kind: PieceKind::Text {
                        face: *face,
                        glyphs: kept_glyphs,
                        text: text[..bytes.min(text.len())].to_owned(),
                        caps: *caps,
                    },
                    width,
                    min_width: width,
                    trailing_space: 0.0,
                    hyphen: 0.0,
                    last_word_hyphenation: false,
                    ..piece.clone()
                });
            }
        }
        pieces.truncate(kept);
        pieces.extend(cut);
        // Without text or an atomic inline before it, the ellipsis has nothing to follow and is
        // left out, as in Blink.
        if pieces.iter().any(content) {
            pieces.extend(ellipsis);
        }
        true
    }

    fn build(
        &self,
        mut pieces: Vec<Piece>,
        continuing: &[usize],
        available: f32,
        indent: f32,
        (last, clamped): (bool, bool),
        formatted: Formatted,
    ) -> Line {
        // The last line of a clamp ends with an ellipsis, cut to make room for it if need be.
        // As in Chrome, a line that was not cut is aligned without it, one that was with it.
        // Its style is made only for a line that may get one: it is a copy of the container's.
        let ellipsizes =
            self.container.text_overflow.ellipsizes() && self.container.overflow_x != Overflow::Visible;
        let ellipsis = (clamped || ellipsizes).then(|| self.ellipsis_style());
        let appended = match &ellipsis {
            Some(ellipsis) if clamped && !self.truncate(&mut pieces, available - indent, true, ellipsis) => {
                pieces.last().map_or(0.0, |piece| piece.width)
            }
            _ => 0.0,
        };
        // A line cut for an ellipsis overflowed: it starts at the start, however it is aligned.
        let cut = !clamped
            && ellipsizes
            && ellipsis
                .as_ref()
                .is_some_and(|ellipsis| self.truncate(&mut pieces, available - indent, false, ellipsis));
        // Preserved spaces that wrap (`pre-wrap`) hang at the end of a line that wraps after
        // them, but at a forced break and at the end of the paragraph only as far as they do not
        // fit (CSS Text 3, §4.1.3), as in Blink: they take room for the alignment there. Spaces
        // before them that hang anyway (collapsible or ideographic ones) hang only where all of
        // the preserved ones overflow.
        if last
            && let Some(piece) = pieces.last_mut()
            && hangs_if_overflowing(&piece.style)
        {
            let width: f32 = pieces.iter().map(|piece| piece.width).sum();
            let overflow = (width - (available - indent)).max(0.0);
            let preserved = preserved_trailing_space(&pieces);
            if let Some(piece) = pieces.last_mut()
                && overflow + FIT_TOLERANCE < preserved
            {
                piece.trailing_space = piece.trailing_space.min(overflow);
            }
        }
        let snaps = snap_items(&mut pieces, false);
        // The trailing spaces of the last piece hang.
        let hanging = pieces.last().map_or(0.0, |piece| piece.trailing_space);
        // Of them, collapsible spaces are removed (CSS Text 3, §4.1.3): the boxes that go on to
        // the next line end before them, but take the others (preserved spaces, other space
        // separators) along, which they overflow the line with, as in Chrome.
        let removed = removed_spaces(&pieces).min(hanging);
        // Boxes that clone their decorations have them at both ends of each line: those that
        // go on from the line before and those that go on to the next.
        let mut still_open = continuing.to_vec();
        for piece in &pieces {
            match piece.kind {
                PieceKind::Open(index) => still_open.push(index),
                PieceKind::Close(_) => {
                    still_open.pop();
                }
                _ => {}
            }
        }
        let cloned = |index: &&usize| self.boxes[**index].style.clones_decorations();
        let start_frames: f32 = continuing
            .iter()
            .filter(cloned)
            .map(|index| self.boxes[*index].margin.0 + self.boxes[*index].frame.0)
            .sum();
        let end_frames: f32 = still_open
            .iter()
            .filter(cloned)
            .map(|index| self.boxes[*index].margin.1 + self.boxes[*index].frame.1)
            .sum();
        let content_width: f32 =
            pieces.iter().map(|piece| piece.width).sum::<f32>() - hanging + start_frames + end_frames;
        // Punctuation that hangs at the start or the end of the line is not measured for its
        // alignment or justification (CSS Text 3, §8.2): it stands outside the line box.
        let start_hang = if formatted.first {
            hanging::start(&pieces, self.boxes)
        } else {
            0.0
        };
        let end_hang = if cut || clamped || end_frames != 0.0 {
            0.0
        } else {
            hanging::end(pieces.iter().rev(), self.boxes).map_or(0.0, |hang| {
                hang.hanging(content_width - start_hang - (available - indent), formatted.last)
            })
        };
        let content_width = content_width - start_hang - end_hang;
        let mut free = if cut {
            0.0
        } else {
            (available - indent - content_width + appended).max(0.0)
        };
        // A leader takes the room left, so that what follows it ends at the end of the line.
        if let Some(leader) = pieces
            .iter_mut()
            .find(|piece| matches!(piece.kind, PieceKind::Leader { .. }))
        {
            leader.width = free;
            free = 0.0;
        }

        // The last line, and a line before a forced break, is aligned by `text-align-last`; by
        // default as the others, unless they are justified.
        let container = self.container;
        let align = match (last, container.text_align_last, container.text_align) {
            (true, Some(align), _) => align,
            (true, None, TextAlign::Justify) => TextAlign::Start,
            (_, _, align) => align,
        };
        // Justification: distribute free space over the spaces between words, or between all
        // characters.
        let justify =
            align == TextAlign::Justify && free > 0.0 && container.text_justify != TextJustify::None;
        if justify {
            // The items are rounded to layout units again once widened.
            unsnap_items(&mut pieces, &snaps);
            if container.text_justify == TextJustify::InterCharacter {
                justify_characters(&mut pieces, free);
            } else {
                justify_spaces(&mut pieces, free);
            }
            snap_items(&mut pieces, true);
        }
        // `start` and `end` by the direction of the line's paragraph. Right to left, the indent
        // is on the right and the spaces that hang at the end of the line on the left.
        let paragraph = pieces.first().map_or(Level::LTR, |piece| piece.bidi);
        let rtl = paragraph.paragraph_rtl();
        let align = match (align, rtl) {
            (TextAlign::Start, false) | (TextAlign::End, true) => TextAlign::Left,
            (TextAlign::Start, true) | (TextAlign::End, false) => TextAlign::Right,
            (align, _) => align,
        };
        // Right to left, a line too wide for its box spills out on the left whatever its
        // alignment, as in Blink (`LineOffsetForTextAlign`).
        let space = available - indent - content_width + appended;
        let aligned = if justify {
            0.0
        } else if rtl && space < 0.0 && !cut {
            space
        } else if cut {
            // A line cut for an ellipsis starts at its start.
            if rtl {
                available - indent - content_width
            } else {
                0.0
            }
        } else {
            match align {
                TextAlign::Center => free / 2.0,
                TextAlign::Right => free,
                _ => 0.0,
            }
        };
        // Right to left, the spaces that hang stand before the line, as wide as Blink's item of
        // them, in layout units (see `split_hanging`).
        let offset = if rtl {
            aligned - snapped_width(hanging) - end_hang
        } else {
            indent + aligned - start_hang
        };
        let forced = pieces.iter().any(|piece| piece.forced_break_after);
        let quirk_boxes = self.quirk.then(|| self.quirk_boxes(&pieces, continuing));
        // A line with bidi text, or with inline boxes of right-to-left text, is put in the order
        // it stands on the page.
        let reordered = rtl
            || pieces.iter().any(|piece| piece.bidi.level > 0)
            || continuing
                .iter()
                .copied()
                .chain(pieces.iter().filter_map(|piece| match piece.kind {
                    PieceKind::Open(index) => Some(index),
                    _ => None,
                }))
                .any(|index| self.boxes[index].style.is_rtl());
        let (pieces, continuing) = if reordered {
            (self.reorder(pieces, continuing, paragraph.paragraph), &[][..])
        } else {
            (pieces, continuing)
        };

        // The line's box tree, with horizontal positions.
        let mut nodes = vec![Node {
            inline_box: None,
            start: 0.0,
            end: available,
            first: true,
            last: true,
            children: Vec::new(),
            shift: 0.0,
            total_shift: 0.0,
            has_metrics: true,
        }];
        let mut stack = vec![0];
        let mut x = offset;
        for &index in continuing {
            let id = nodes.len();
            let clones = self.boxes[index].style.clones_decorations();
            nodes.push(Node {
                inline_box: Some(index),
                start: x,
                end: x,
                first: clones,
                last: false,
                children: Vec::new(),
                shift: 0.0,
                total_shift: 0.0,
                has_metrics: true,
            });
            if clones {
                x += self.boxes[index].margin.0 + self.boxes[index].frame.0;
            }
            let parent = *stack.last().expect("root");
            nodes[parent].children.push(Child::Node(id));
            stack.push(id);
        }
        let mut has_content = forced;
        // Out-of-flow boxes after the last content of the line stand before the collapsible
        // spaces removed at its end, as in Blink.
        let trailing_from = pieces
            .iter()
            .rposition(|piece| {
                !matches!(
                    piece.kind,
                    PieceKind::Open(_) | PieceKind::Close(_) | PieceKind::Placeholder(_)
                )
            })
            .map_or(0, |last| last + 1);
        for (position, piece) in pieces.into_iter().enumerate() {
            // The ellipsis stands on the line's baseline, outside the boxes it is cut in, as in
            // Blink (`LineTruncator::PlaceEllipsisNextTo`).
            let parent = if ellipsis
                .as_ref()
                .is_some_and(|ellipsis| Arc::ptr_eq(&piece.style, ellipsis))
            {
                0
            } else {
                *stack.last().expect("root")
            };
            match piece.kind {
                PieceKind::Open(index) | PieceKind::Left { index, .. } => {
                    let first = !matches!(piece.kind, PieceKind::Left { edge: false, .. });
                    // A box that starts after the last content of the line, taking no room,
                    // starts before the spaces removed at its end.
                    let start = if position >= trailing_from && piece.width == 0.0 {
                        x - removed
                    } else {
                        x
                    };
                    let id = nodes.len();
                    nodes.push(Node {
                        inline_box: Some(index),
                        start,
                        end: start,
                        first,
                        last: false,
                        children: Vec::new(),
                        shift: 0.0,
                        total_shift: 0.0,
                        has_metrics: true,
                    });
                    nodes[parent].children.push(Child::Node(id));
                    stack.push(id);
                    // An inline box with a margin, border or padding at its start is content,
                    // even a negative margin, as in Blink (`IsInlineBoxStartEmpty`).
                    has_content |=
                        first && (self.boxes[index].margin.0 != 0.0 || self.boxes[index].frame.0 != 0.0);
                }
                PieceKind::Close(index) | PieceKind::Right { index, .. } => {
                    let last = !matches!(piece.kind, PieceKind::Right { edge: false, .. });
                    x += piece.width;
                    if stack.len() > 1 {
                        let node = stack.pop().expect("open box");
                        nodes[node].end = x;
                        nodes[node].last = last;
                    }
                    has_content |=
                        last && (self.boxes[index].margin.1 != 0.0 || self.boxes[index].frame.1 != 0.0);
                    continue;
                }
                PieceKind::Text {
                    face,
                    glyphs,
                    text,
                    caps,
                } => {
                    // Text combined upright takes 1em, whatever its glyphs.
                    let combined = piece.style.text_combine_upright && piece.style.is_vertical();
                    let width: f32 = if combined {
                        piece.width
                    } else {
                        glyphs.iter().map(|glyph| glyph.advance).sum()
                    };
                    // A zero width space (`<wbr>`) draws nothing but makes the line, as in Blink.
                    has_content |= !glyphs.is_empty() || text.contains(ZERO_WIDTH_SPACE);
                    // Merge with the previous run of the same face, style and setting.
                    if !combined
                        && let Some(Child::Text {
                            width: previous_width,
                            face: previous_face,
                            style: previous_style,
                            glyphs: previous_glyphs,
                            text: previous_text,
                            marker: false,
                            caps: previous_caps,
                            ..
                        }) = nodes[parent].children.last_mut()
                        && *previous_face == face
                        // The parts of generated content are items of their own, each with a
                        // style of its own, but one run (see `content_items`).
                        && (Arc::ptr_eq(previous_style, &piece.style) || **previous_style == *piece.style)
                        && *previous_caps == caps
                    {
                        let base = previous_text.len() as u32;
                        previous_glyphs.extend(glyphs.iter().map(|glyph| Glyph {
                            cluster: (glyph.cluster.0 + base, glyph.cluster.1 + base),
                            ..*glyph
                        }));
                        previous_text.push_str(&text);
                        *previous_width += width;
                    } else {
                        nodes[parent].children.push(Child::Text {
                            x,
                            width,
                            face,
                            style: Arc::clone(&piece.style),
                            glyphs,
                            text,
                            marker: false,
                            caps,
                            symbol: None,
                        });
                    }
                    x += width;
                    continue;
                }
                PieceKind::Atomic {
                    fragment,
                    height,
                    baseline,
                    ..
                } => {
                    nodes[parent].children.push(Child::Atomic {
                        x,
                        fragment,
                        height,
                        baseline,
                        shift: 0.0,
                        total_shift: 0.0,
                    });
                    has_content = true;
                }
                PieceKind::Placeholder(id) => {
                    let x = if position >= trailing_from { x - removed } else { x };
                    nodes[parent].children.push(Child::Placeholder { x, id });
                }
                PieceKind::Marker {
                    runs,
                    start,
                    inside,
                    image,
                    symbol,
                } => {
                    // Outside the box, the marker is placed from where the content box starts:
                    // on its right in a right-to-left block (whatever the marker's own
                    // direction, as in Chrome), mirrored, as inside its room on the line.
                    let width = image.as_ref().map_or_else(
                        || {
                            let width = runs.iter().map(|run| run.3).sum();
                            // The box of an outside marker's text (see where it starts).
                            if inside || symbol.is_some() {
                                width
                            } else {
                                snapped_width(width)
                            }
                        },
                        |image| image.1,
                    );
                    let rtl = if inside {
                        piece.style.is_rtl()
                    } else {
                        container.is_rtl()
                    };
                    let (mut marker_x, node) = match (inside, rtl) {
                        (true, false) => (x + start, parent),
                        (true, true) => (x + piece.width - start - width, parent),
                        (false, false) => (start, 0),
                        (false, true) => (available - start - width, 0),
                    };
                    // An image on the baseline, like an inline image.
                    if let Some((image, width, height)) = image {
                        nodes[node].children.push(Child::Atomic {
                            x: marker_x,
                            fragment: Box::new(Fragment {
                                rect: Rect::new(Au::ZERO, Au::ZERO, Au::from_px(width), Au::from_px(height)),
                                kind: FragmentKind::Image(image),
                                children: Children::new(),
                            }),
                            height,
                            baseline: height,
                            shift: 0.0,
                            total_shift: 0.0,
                        });
                    }
                    for (face, glyphs, text, run_width, caps) in runs {
                        nodes[node].children.push(Child::Text {
                            x: marker_x,
                            width: run_width,
                            face,
                            style: Arc::clone(&piece.style),
                            glyphs,
                            text,
                            marker: true,
                            caps,
                            symbol,
                        });
                        marker_x += run_width;
                    }
                    has_content = true;
                }
                PieceKind::Leader {
                    face,
                    glyphs,
                    text,
                    caps,
                    unit,
                } => {
                    // Whole patterns, ending where the leader ends; at most enough to fill the
                    // widest page there is, however wide the line (a leader in a line made huge by
                    // extreme lengths would otherwise need more memory than there is).
                    let count = ((piece.width / unit).floor() as usize)
                        .min(MAX_LEADER_PATTERNS / glyphs.len().max(1));
                    if count > 0 {
                        let mut repeated = Vec::with_capacity(glyphs.len() * count);
                        let mut repeated_text = String::with_capacity(text.len() * count);
                        for _ in 0..count {
                            let base = repeated_text.len() as u32;
                            repeated.extend(glyphs.iter().map(|glyph| Glyph {
                                cluster: (glyph.cluster.0 + base, glyph.cluster.1 + base),
                                ..*glyph
                            }));
                            repeated_text.push_str(&text);
                        }
                        nodes[parent].children.push(Child::Text {
                            x: x + piece.width - count as f32 * unit,
                            width: count as f32 * unit,
                            face,
                            style: Arc::clone(&piece.style),
                            glyphs: repeated,
                            text: repeated_text,
                            marker: false,
                            caps,
                            symbol: None,
                        });
                        has_content = true;
                    }
                }
                PieceKind::Measured | PieceKind::Float { .. } => {}
            }
            x += piece.width;
        }
        // A box that goes on on the next line ends before the spaces that hang at the end of this
        // one, as in browsers, with its end decorations if it clones them, inside out.
        let mut end = x - removed;
        for &open in stack[1..].iter().rev() {
            if let Some(index) = nodes[open].inline_box
                && self.boxes[index].style.clones_decorations()
            {
                end += self.boxes[index].margin.1 + self.boxes[index].frame.1;
                nodes[open].last = true;
            }
            nodes[open].end = end;
        }

        if let Some(boxes) = quirk_boxes {
            for node in &mut nodes {
                node.has_metrics = boxes.contains(&node.inline_box);
            }
        }
        let (top, bottom) = self.align(&mut nodes);
        // A line without text, atomic inlines, inline box edges or a forced break at its end takes
        // no space (CSS 2.1, §9.4.2).
        // Its inline boxes then hang down from its top, as in Blink: the containing block an
        // empty positioned one makes starts there.
        let empty_baseline = -top;
        let (top, bottom) = if has_content { (top, bottom) } else { (0.0, 0.0) };
        let baseline = -top;
        let height = Au::from_px(bottom - top);
        let mut children = Vec::new();
        self.emit(
            &mut nodes,
            0,
            if has_content { baseline } else { empty_baseline },
            (0.0, 0.0),
            &mut children,
        );
        if !reordered {
            drop_hanging_glyphs(&mut children);
        }
        Line {
            height,
            fragment: Fragment {
                rect: Rect::new(Au::ZERO, Au::ZERO, Au::from_px(available), height),
                kind: FragmentKind::Line {
                    baseline: Au::from_px(baseline),
                },
                children: children.into(),
            },
            top: Au::ZERO,
            end: (0, 0),
            floats: Vec::new(),
            floats_bottom: Au::ZERO,
            float_count: 0,
            clearance_after: Au::ZERO,
            annotations_under: Au::ZERO,
            annotations_over: Au::ZERO,
            phantom: !has_content,
            text_scale: 1.0,
        }
    }

    /// The pieces of a line in the order they stand on the page (UAX #9, rules L1 and L2), with
    /// the sides of the fragments of its inline boxes around theirs, those that go on from the
    /// line before (`continuing`) too. The spaces that hang at the end of the line stand at the
    /// end of its paragraph's direction (`paragraph` is its level), outside the boxes.
    fn reorder(&self, mut pieces: Vec<Piece>, continuing: &[usize], paragraph: u8) -> Vec<Piece> {
        use crate::bidi::{Logical, Visual};
        let hanging = split_hanging(&mut pieces, paragraph);
        // The boxes whose content is isolated from the content around them.
        let isolates = |index: usize| {
            matches!(
                self.boxes[index].style.unicode_bidi,
                fagus_style::values::UnicodeBidi::Isolate
                    | fagus_style::values::UnicodeBidi::IsolateOverride
                    | fagus_style::values::UnicodeBidi::Plaintext
            )
        };
        let logical: Vec<Logical> = pieces
            .iter()
            .enumerate()
            .map(|(index, piece)| match piece.kind {
                PieceKind::Open(index) if isolates(index) => Logical::IsolateOpen {
                    index,
                    level: piece.bidi.level,
                },
                PieceKind::Close(index) if isolates(index) => Logical::IsolateClose {
                    index,
                    level: piece.bidi.level,
                },
                PieceKind::Open(inline_box) => Logical::Open(inline_box),
                PieceKind::Close(inline_box) => Logical::Close(inline_box),
                _ => Logical::Leaf {
                    level: piece.bidi.level,
                    hanging: Some(index) == hanging,
                },
            })
            .collect();
        let visual = crate::bidi::reorder(&logical, continuing, paragraph, |index| {
            let style = &self.boxes[index].style;
            (style.is_rtl(), style.clones_decorations())
        });
        let mut slots: Vec<Option<Piece>> = pieces.into_iter().map(Some).collect();
        let mut ordered = Vec::with_capacity(visual.len());
        for item in visual {
            let (kind, index, edge) = match item {
                Visual::Leaf(index) => {
                    if let Some(piece) = slots[index].take()
                        && !matches!(piece.kind, PieceKind::Open(_) | PieceKind::Close(_))
                    {
                        ordered.push(piece);
                    }
                    continue;
                }
                Visual::Left { index, edge } => (PieceKind::Left { index, edge }, index, edge),
                Visual::Right { index, edge } => (PieceKind::Right { index, edge }, index, edge),
            };
            let inline_box = &self.boxes[index];
            let width = match (edge, &kind) {
                (false, _) => 0.0,
                (true, PieceKind::Left { .. }) => inline_box.margin.0 + inline_box.frame.0,
                (true, _) => inline_box.margin.1 + inline_box.frame.1,
            };
            ordered.push(Piece {
                kind,
                style: Arc::clone(&inline_box.style),
                width,
                min_width: width,
                trailing_space: 0.0,
                break_after: false,
                forced_break_after: false,
                break_clear: fagus_style::values::Clear::None,
                hyphen: 0.0,
                last_word_hyphenation: false,
                bidi: Level::LTR,
                edges: spacing::Edges::default(),
                broken: None,
            });
        }
        ordered
    }

    /// The baseline shift of a box with `vertical-align`, relative to its parent's baseline, for
    /// a box that reaches `above` over and `below` under its own baseline. Like Blink's, the
    /// shifts are whole layout units (1/64 px): the font size is cut to units before it is
    /// divided, divisions cut toward zero, and half the x-height of `middle` is rounded.
    fn shift(
        align: VerticalAlign,
        parent: &ComputedStyle,
        parent_metrics: Metrics,
        above: f32,
        below: f32,
    ) -> f32 {
        let units = |px: f32| (px * 64.0).trunc();
        match align {
            VerticalAlign::Baseline | VerticalAlign::Top | VerticalAlign::Bottom => 0.0,
            VerticalAlign::Sub => (units(parent.font_size) / 5.0).trunc() / 64.0 + 1.0,
            VerticalAlign::Super => -((units(parent.font_size) / 3.0).trunc() / 64.0 + 1.0),
            VerticalAlign::TextTop => -parent_metrics.ascent + above,
            VerticalAlign::TextBottom => parent_metrics.descent - below,
            VerticalAlign::Middle => {
                (units(above - below) / 2.0).trunc() / 64.0
                    - (parent_metrics.x_height / 2.0 * 64.0).round() / 64.0
            }
            VerticalAlign::BaselineMiddle => (units(above - below) / 2.0).trunc() / 64.0,
            VerticalAlign::Length(length) => -units(length.resolve(parent_metrics.line_height)) / 64.0,
        }
    }

    /// Computes the baseline shifts of all boxes and returns the top and bottom of the line box
    /// relative to the root baseline.
    fn align(&self, nodes: &mut [Node]) -> (f32, f32) {
        // Shifts relative to the parent, from the parent's metrics.
        for parent in 0..nodes.len() {
            // The style lives in the builder, not in the nodes changed below.
            let parent_style = self.style_of(&nodes[parent]);
            let parent_metrics = self.metrics_of(&nodes[parent]);
            let children: Vec<usize> = nodes[parent]
                .children
                .iter()
                .filter_map(|child| match child {
                    Child::Node(id) => Some(*id),
                    _ => None,
                })
                .collect();
            for id in children {
                let (above, below) = self.reach_of(&nodes[id]);
                let align = self.style_of(&nodes[id]).vertical_align.clone();
                nodes[id].shift = Self::shift(align, parent_style, parent_metrics, above, below);
            }
            for child in &mut nodes[parent].children {
                if let Child::Atomic {
                    fragment,
                    height,
                    baseline,
                    shift,
                    ..
                } = child
                {
                    *shift = Self::shift(
                        atomic_alignment(fragment),
                        parent_style,
                        parent_metrics,
                        *baseline,
                        *height - *baseline,
                    );
                }
            }
        }
        // Totals from the root down, and the extent of everything not aligned to the line box.
        let mut top = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        let mut line_aligned: Vec<(usize, bool, f32, f32)> = Vec::new();
        self.extent(nodes, 0, 0.0, &mut top, &mut bottom, &mut line_aligned);
        if !top.is_finite() {
            (top, bottom) = (0.0, 0.0);
        }
        // Boxes aligned to the top or bottom of the line box, with their subtrees.
        for (id, at_top, sub_top, sub_bottom) in line_aligned {
            let height = sub_bottom - sub_top;
            if height > bottom - top {
                if at_top {
                    bottom = top + height;
                } else {
                    top = bottom - height;
                }
            }
            let shift = if at_top {
                top - sub_top
            } else {
                bottom - sub_bottom
            };
            move_subtree(nodes, id, shift);
        }
        // Atomic inlines aligned to the line box: they make it taller if needed, then sit at
        // its top or bottom.
        for node in nodes.iter() {
            for child in &node.children {
                if let Child::Atomic { fragment, height, .. } = child {
                    match atomic_alignment(fragment) {
                        VerticalAlign::Top if *height > bottom - top => bottom = top + *height,
                        VerticalAlign::Bottom if *height > bottom - top => top = bottom - *height,
                        _ => {}
                    }
                }
            }
        }
        place_line_aligned_atomics(nodes, top, bottom);
        (top, bottom)
    }

    /// Adds the extent of node `id` (with its baseline `total` below the root's) and its subtree
    /// to `top` and `bottom`, setting total shifts. Inline boxes aligned to the line box are
    /// collected with their own extent instead.
    fn extent(
        &self,
        nodes: &mut [Node],
        id: usize,
        total: f32,
        top: &mut f32,
        bottom: &mut f32,
        line_aligned: &mut Vec<(usize, bool, f32, f32)>,
    ) {
        nodes[id].total_shift = total;
        if nodes[id].has_metrics {
            let (above, below) = self.reach_of(&nodes[id]);
            *top = top.min(total - above);
            *bottom = bottom.max(total + below);
        }
        let children: Vec<usize> = nodes[id]
            .children
            .iter()
            .filter_map(|child| match child {
                Child::Node(child) => Some(*child),
                _ => None,
            })
            .collect();
        for child in children {
            let align = self.style_of(&nodes[child]).vertical_align.clone();
            if matches!(align, VerticalAlign::Top | VerticalAlign::Bottom) {
                let (mut sub_top, mut sub_bottom) = (f32::INFINITY, f32::NEG_INFINITY);
                self.extent(nodes, child, 0.0, &mut sub_top, &mut sub_bottom, line_aligned);
                line_aligned.push((child, align == VerticalAlign::Top, sub_top, sub_bottom));
            } else {
                let shift = nodes[child].shift;
                self.extent(nodes, child, total + shift, top, bottom, line_aligned);
            }
        }
        for child in &mut nodes[id].children {
            if let Child::Atomic {
                fragment,
                height,
                baseline,
                shift,
                total_shift,
                ..
            } = child
            {
                *total_shift = total + *shift;
                // Atomic inlines aligned to the line box are placed when its extent is known.
                if !matches!(
                    atomic_alignment(fragment),
                    VerticalAlign::Top | VerticalAlign::Bottom
                ) {
                    *top = top.min(*total_shift - *baseline);
                    *bottom = bottom.max(*total_shift + *height - *baseline);
                }
            }
        }
    }

    /// Turns the children of node `id` into fragments, positioned relative to `origin` (the
    /// position of their parent fragment within the line). `baseline` is the root baseline's
    /// distance from the top of the line.
    fn emit(
        &self,
        nodes: &mut [Node],
        id: usize,
        baseline: f32,
        origin: (f32, f32),
        out: &mut Vec<Fragment>,
    ) {
        let children = std::mem::take(&mut nodes[id].children);
        for child in children {
            match child {
                Child::Node(child) => {
                    let node = &nodes[child];
                    let index = node.inline_box.expect("inline box");
                    let inline_box = &self.boxes[index];
                    let own = metrics(self.fonts, &inline_box.style);
                    let left = if node.first { inline_box.margin.0 } else { 0.0 };
                    let right = if node.last { inline_box.margin.1 } else { 0.0 };
                    // The border box: the content area of the box's font plus padding and borders,
                    // trimmed to the edges `text-box-edge` names at the sides `text-box-trim`
                    // names (CSS Inline 3, §5).
                    let (ascent, descent) = trimmed_content_area(self.fonts, &inline_box.style, own);
                    let x = node.start + left;
                    let width = (node.end - right - x).max(0.0);
                    let y = baseline + node.total_shift - ascent - inline_box.frame_top;
                    let height = ascent + descent + inline_box.frame_top + inline_box.frame_bottom;
                    let (first, last) = (node.first, node.last);
                    let mut grandchildren = Vec::new();
                    self.emit(nodes, child, baseline, (x, y), &mut grandchildren);
                    out.push(Fragment {
                        rect: Rect::new(
                            line_units(x - origin.0),
                            Au::from_px(y - origin.1),
                            Au::from_px(width),
                            Au::from_px(height),
                        ),
                        kind: FragmentKind::Box {
                            style: Arc::clone(&inline_box.style),
                            node: inline_box.node,
                            sides: VisibleSides {
                                left: first,
                                right: last,
                                ..VisibleSides::ALL
                            },
                        },
                        children: grandchildren.into(),
                    });
                }
                Child::Text {
                    x,
                    width,
                    face,
                    style,
                    glyphs,
                    text,
                    marker,
                    caps,
                    symbol,
                } => {
                    let run_metrics = metrics(self.fonts, &style);
                    let y = baseline + nodes[id].total_shift - run_metrics.ascent;
                    let (orientation, glyph_baseline) = orientation(self.fonts, &style, caps, run_metrics);
                    out.push(Fragment {
                        rect: Rect::new(
                            line_units(x - origin.0),
                            Au::from_px(y - origin.1),
                            Au::from_px(width),
                            Au::from_px(run_metrics.ascent + run_metrics.descent),
                        ),
                        kind: FragmentKind::Text(TextRun {
                            face,
                            size: caps.size(used_font_size(self.fonts, &style)),
                            style,
                            glyphs,
                            text,
                            baseline: Au::from_px(glyph_baseline),
                            marker,
                            symbol,
                            orientation,
                            exact: false,
                        }),
                        children: Children::new(),
                    });
                }
                Child::Atomic {
                    x,
                    mut fragment,
                    baseline: atomic_baseline,
                    total_shift,
                    ..
                } => {
                    // The fragment sits at its margins within the margin box.
                    let top = baseline + total_shift - atomic_baseline;
                    fragment.rect.x += line_units(x - origin.0);
                    fragment.rect.y += Au::from_px(top - origin.1);
                    out.push(*fragment);
                }
                Child::Placeholder { x, id: placeholder } => out.push(Fragment {
                    rect: Rect::new(
                        line_units(x - origin.0),
                        Au::from_px(-origin.1),
                        Au::ZERO,
                        Au::ZERO,
                    ),
                    kind: FragmentKind::Placeholder(placeholder),
                    children: Children::new(),
                }),
            }
        }
    }
}

/// How far the content area of an inline box of `style` with font metrics `own` reaches over
/// and under its baseline: to the edges of `text-box-edge` at the sides `text-box-trim` trims,
/// else the ascent and descent of its first available font.
fn trimmed_content_area(fonts: &FontDb, style: &ComputedStyle, own: Metrics) -> (f32, f32) {
    let trim = style.text_box_trim;
    if !(trim.start || trim.end) {
        return (own.ascent, own.descent);
    }
    let (over, under) = crate::text_box::edges(fonts, style);
    (
        if trim.start { over } else { own.ascent },
        if trim.end { under } else { own.descent },
    )
}

/// How the glyphs of a run in a style stand, and where their baseline lies below the top of
/// the run's box: in vertical lines, the alphabetic baseline of the style's first face, its
/// rounded ascent below the box's top (the over side), as in Chrome; upright glyphs are
/// placed from there.
fn orientation(
    fonts: &FontDb,
    style: &ComputedStyle,
    caps: Caps,
    metrics: Metrics,
) -> (GlyphOrientation, f32) {
    if !style.is_vertical() {
        return (GlyphOrientation::Horizontal, metrics.ascent);
    }
    let alphabetic = || {
        face_for(fonts, style).map_or(metrics, |face| {
            face_metrics(fonts, face, used_font_size(fonts, style))
        })
    };
    // Combined text stands upright in its 1em, its ascent and descent centered in it.
    if style.text_combine_upright {
        let own = alphabetic();
        let size = used_font_size(fonts, style);
        return (
            GlyphOrientation::Combined,
            (size - own.ascent - own.descent) / 2.0 + own.ascent,
        );
    }
    let orientation = if caps.upright {
        GlyphOrientation::Upright
    } else if style.writing_mode == fagus_style::values::WritingMode::SidewaysLr {
        GlyphOrientation::SidewaysLeft
    } else {
        GlyphOrientation::Sideways
    };
    if !central_baseline(style) {
        return (orientation, metrics.ascent);
    }
    (orientation, alphabetic().ascent)
}

/// Splits the spaces that hang at the end of a line off its last text: they take the level of
/// the line's paragraph (UAX #9, rule L1) and, as in Chrome, which drops them, stand outside the
/// inline boxes. Right to left, where they stand before the line, they are as wide as Blink's
/// item of them, in layout units (its `HangWidth`), so that the text after them starts where
/// the line's alignment puts it. Returns the index of the piece they make, at the end of the
/// line.
fn split_hanging(pieces: &mut Vec<Piece>, paragraph: u8) -> Option<usize> {
    // Widens spaces that hang before a right-to-left line to layout units.
    let snap = |glyphs: &mut [Glyph], width: f32| {
        let snapped = if paragraph % 2 == 1 {
            snapped_width(width)
        } else {
            width
        };
        if let Some(glyph) = glyphs.last_mut() {
            glyph.advance += snapped - width;
            return snapped;
        }
        width
    };
    // (Past out-of-flow boxes that the line ends with after the spaces.)
    let last = pieces
        .iter()
        .rposition(|piece| !matches!(piece.kind, PieceKind::Close(_) | PieceKind::Placeholder(_)))?;
    let piece = &mut pieces[last];
    let PieceKind::Text {
        face,
        glyphs,
        text,
        caps,
    } = &mut piece.kind
    else {
        return None;
    };
    let end = text.trim_end_matches(' ').len();
    if end == text.len() {
        return None;
    }
    // A text of nothing but spaces (between or in inline boxes) takes the level where it is.
    if end == 0 {
        piece.bidi.level = paragraph;
        let width = snap(glyphs, piece.width);
        piece.trailing_space += width - piece.width;
        piece.width = width;
        piece.min_width = width;
        return None;
    }
    let (kept, spaces): (Vec<Glyph>, Vec<Glyph>) = std::mem::take(glyphs)
        .into_iter()
        .partition(|glyph| (glyph.cluster.0 as usize) < end);
    *glyphs = kept;
    let mut spaces: Vec<Glyph> = spaces
        .into_iter()
        .map(|glyph| Glyph {
            cluster: (glyph.cluster.0 - end as u32, glyph.cluster.1 - end as u32),
            ..glyph
        })
        .collect();
    let space_text = text.split_off(end);
    let measured: f32 = spaces.iter().map(|glyph| glyph.advance).sum();
    piece.width -= measured;
    let width = snap(&mut spaces, measured);
    let hanging = Piece {
        kind: PieceKind::Text {
            face: *face,
            glyphs: spaces,
            text: space_text,
            caps: *caps,
        },
        style: Arc::clone(&piece.style),
        width,
        min_width: width,
        trailing_space: width,
        break_after: piece.break_after,
        forced_break_after: false,
        break_clear: fagus_style::values::Clear::None,
        hyphen: 0.0,
        last_word_hyphenation: false,
        bidi: Level {
            level: paragraph,
            ..piece.bidi
        },
        edges: spacing::Edges::default(),
        broken: None,
    };
    piece.min_width = piece.width;
    piece.trailing_space = 0.0;
    pieces.push(hanging);
    Some(pieces.len() - 1)
}

/// Drops the glyphs of the collapsible spaces at the end of a line of left-to-right text (of
/// its last text, past the ends and starts of inline boxes and out-of-flow boxes there), which
/// Chrome removes from the line (Blink's `RemoveTrailingCollapsibleSpace`): they are not in
/// the PDF. The room they take stays. Returns whether the content of `fragments` ended before
/// the text was found.
fn drop_hanging_glyphs(fragments: &mut [Fragment]) -> bool {
    for fragment in fragments.iter_mut().rev() {
        match &mut fragment.kind {
            FragmentKind::Placeholder(_) => {}
            FragmentKind::Text(run) => {
                if run.style.white_space.collapses_spaces() {
                    let end = run.text.trim_end_matches(' ').len();
                    run.glyphs.retain(|glyph| (glyph.cluster.0 as usize) < end);
                }
                return true;
            }
            FragmentKind::Box { style, .. }
                if !style.is_floating()
                    && matches!(
                        style.display,
                        fagus_style::values::Display::Normal {
                            outside: fagus_style::values::DisplayOutside::Inline,
                            inside: fagus_style::values::DisplayInside::Flow,
                            ..
                        }
                    ) =>
            {
                if drop_hanging_glyphs(&mut fragment.children) {
                    return true;
                }
            }
            _ => return true,
        }
    }
    false
}

fn atomic_alignment(fragment: &Fragment) -> VerticalAlign {
    match &fragment.kind {
        FragmentKind::Box { style, .. } => style.vertical_align.clone(),
        _ => VerticalAlign::Baseline,
    }
}

fn move_subtree(nodes: &mut [Node], id: usize, shift: f32) {
    nodes[id].total_shift += shift;
    let children: Vec<usize> = nodes[id]
        .children
        .iter()
        .filter_map(|child| match child {
            Child::Node(child) => Some(*child),
            _ => None,
        })
        .collect();
    for child in &mut nodes[id].children {
        if let Child::Atomic { total_shift, .. } = child {
            *total_shift += shift;
        }
    }
    for child in children {
        move_subtree(nodes, child, shift);
    }
}

/// Puts atomic inlines with `vertical-align: top` or `bottom` at the top or bottom of the line box.
fn place_line_aligned_atomics(nodes: &mut [Node], top: f32, bottom: f32) {
    for node in nodes.iter_mut() {
        for child in &mut node.children {
            if let Child::Atomic {
                fragment,
                height,
                baseline,
                total_shift,
                ..
            } = child
            {
                match atomic_alignment(fragment) {
                    VerticalAlign::Top => *total_shift = top + *baseline,
                    VerticalAlign::Bottom => *total_shift = bottom - (*height - *baseline),
                    _ => {}
                }
            }
        }
    }
}

/// Widens the spaces between words so that the line fills the available width, and the gaps
/// around CJK ideographs, kana and symbols (`text-justify: auto`), as Blink's
/// `ShapeResultSpacing` expands them: after each space, and before and after each such
/// character, though never twice in a row, nor at the start or the end of the line. No-break
/// spaces are widened too: Blink counts every character it treats as a space
/// (`Character::TreatAsSpace`) as a place to expand.
fn justify_spaces(pieces: &mut [Piece], free: f32) {
    // The last text piece's trailing spaces hang and are not widened.
    let last_text = pieces
        .iter()
        .rposition(|piece| matches!(piece.kind, PieceKind::Text { .. }));
    // The places to expand in logical order: the piece, the glyph, and whether the gap is
    // before the glyph rather than after it.
    let mut places: Vec<(usize, usize, bool)> = Vec::new();
    let mut after_expansion = true;
    for (index, piece) in pieces.iter().enumerate() {
        let (text, glyphs) = match &piece.kind {
            PieceKind::Text { text, glyphs, .. } => (text, glyphs),
            // An atomic inline is a character of the line that does not expand.
            PieceKind::Atomic { .. } => {
                after_expansion = false;
                continue;
            }
            _ => continue,
        };
        let end = if Some(index) == last_text {
            text.trim_end_matches(JUSTIFIED_SPACES).len()
        } else {
            text.len()
        };
        let mut order: Vec<usize> = (0..glyphs.len()).collect();
        if piece.bidi.is_rtl() {
            order.reverse();
        }
        for position in order {
            let cluster = glyphs[position].cluster.0 as usize;
            if cluster >= end {
                continue;
            }
            let Some(character) = text.get(cluster..).and_then(|rest| rest.chars().next()) else {
                continue;
            };
            if JUSTIFIED_SPACES.contains(&character) {
                places.push((index, position, false));
                after_expansion = true;
            } else if crate::ruby::is_cjk(character) {
                if !after_expansion {
                    places.push((index, position, true));
                }
                places.push((index, position, false));
                after_expansion = true;
            } else {
                after_expansion = false;
            }
        }
    }
    // No gap after the last character of the line (but the spaces before a box at its end).
    if after_expansion
        && let Some(&(index, position, false)) = places.last()
        && let PieceKind::Text { text, glyphs, .. } = &pieces[index].kind
        && text
            .get(glyphs[position].cluster.0 as usize..)
            .and_then(|rest| rest.chars().next())
            .is_some_and(crate::ruby::is_cjk)
    {
        places.pop();
    }
    if places.is_empty() {
        return;
    }
    let (share, last_share) = expansion_shares(free, places.len());
    let last_place = places.len() - 1;
    for (place, (index, position, before)) in places.into_iter().enumerate() {
        let extra = if place == last_place { last_share } else { share };
        let piece = &mut pieces[index];
        if let PieceKind::Text { glyphs, .. } = &mut piece.kind {
            let glyph = &mut glyphs[position];
            glyph.advance += extra;
            if before {
                glyph.x_offset += extra;
            }
            piece.width += extra;
        }
    }
}

/// The characters that `text-align: justify` widens: spaces, preserved tabs (by as much, from
/// the tab stop they reach) and no-break spaces, as Blink's `TreatAsSpace` tells.
const JUSTIFIED_SPACES: [char; 3] = [' ', '\t', '\u{a0}'];

/// The distance between tab stops and the least distance from a tab to the stop it reaches (CSS
/// Text 3, §4.2): `tab-size` spaces of the block's font with their letter and word spacing, or a
/// length; half a space.
fn tab_stops(fonts: &FontDb, container: &ComputedStyle) -> (f32, f32) {
    // The space of the first font that has one, as Chrome measures it.
    let face = face_for(fonts, container).map(|face| {
        if fonts.face(face).has_glyph(' ') {
            return face;
        }
        let families: Vec<&str> = container.font_family.iter().map(String::as_str).collect();
        fonts
            .fallback_in(
                &families,
                face,
                ' ',
                (
                    container.font_weight,
                    font_style(container),
                    container.font_stretch,
                ),
                container.lang.as_deref(),
            )
            .unwrap_or(face)
    });
    let space = face.map_or(container.font_size / 4.0, |face| {
        let size = used_font_size(fonts, container);
        let scale = fonts.face(face).scale(size);
        let variations = font_variations(fonts, face, container, size);
        fonts
            .shape_variable(face, " ", &[], &variations)
            .glyphs
            .first()
            .map_or(0.0, |glyph| glyph.x_advance * scale)
    }) + container.letter_spacing
        + container.word_spacing;
    let stop = match container.tab_size {
        TabSize::Spaces(count) => count * space,
        TabSize::Px(px) => px,
    };
    (stop, space / 2.0)
}

/// Widens the tabs of a piece of text that starts `x` into its line to the next tab stops; a tab
/// less than half a space before a stop goes on to the stop after it.
fn expand_tabs(piece: &mut Piece, x: f32, (stop, least): (f32, f32)) {
    let PieceKind::Text { glyphs, text, .. } = &mut piece.kind else {
        return;
    };
    if stop <= 0.0 || !text.contains('\t') {
        return;
    }
    // In logical order: right to left text was shaped from its end.
    let rtl = piece.bidi.is_rtl();
    let mut pen = x;
    let mut ordered: Vec<&mut Glyph> = glyphs.iter_mut().collect();
    if rtl {
        ordered.reverse();
    }
    for glyph in ordered {
        if text.as_bytes().get(glyph.cluster.0 as usize) == Some(&b'\t') {
            let mut next = ((pen / stop).floor() + 1.0) * stop;
            if next - pen < least {
                next += stop;
            }
            glyph.advance = next - pen;
        }
        pen += glyph.advance;
    }
    let width: f32 = glyphs.iter().map(|glyph| glyph.advance).sum();
    let is_space =
        |glyph: &&Glyph| matches!(text.as_bytes().get(glyph.cluster.0 as usize), Some(b' ' | b'\t'));
    // Preserved spaces and tabs hang only where lines wrap, and not with `break-spaces` (see
    // `trailing_space`).
    let style = &piece.style;
    let hangs = !(style.white_space.collapse == fagus_style::values::WhiteSpaceCollapse::BreakSpaces
        || (style.white_space.collapse == fagus_style::values::WhiteSpaceCollapse::Preserve
            && style.white_space.nowrap));
    let trailing: f32 = if !hangs {
        0.0
    } else if rtl {
        glyphs
            .iter()
            .take_while(is_space)
            .map(|glyph| glyph.advance)
            .sum()
    } else {
        glyphs
            .iter()
            .rev()
            .take_while(is_space)
            .map(|glyph| glyph.advance)
            .sum()
    };
    piece.width = width;
    piece.min_width = width;
    piece.trailing_space = trailing;
}

/// Distributes `free` over the gaps between the characters of a line (`text-justify:
/// inter-character`), as Blink's `JustificationContext` finds them: between any two characters
/// in logical order, but not inside a run of a cursive script (Arabic), and not after the
/// spaces that hang at the end of the line. A gap goes after the character before it (to the
/// right of its glyph, or in right-to-left text to the left, as wide a glyph after it), but
/// after a letter of a cursive script before the character after it. An atomic inline is a
/// character of its own, which widens, but not beside another one. As in Chrome, the gaps between the characters of a
/// ligature (`ffi`) take their share of the room but add nothing: the line ends short of the
/// end of its box.
fn justify_characters(pieces: &mut [Piece], free: f32) {
    /// A cluster of the line: its piece, the positions of its glyphs on the left and on the
    /// right, whether it is of a cursive script, whether it is right to left, and whether it is
    /// an atomic inline.
    #[derive(Clone, Copy)]
    struct Cluster {
        piece: usize,
        left: usize,
        right: usize,
        cursive: bool,
        rtl: bool,
        atomic: bool,
    }
    let last_text = pieces
        .iter()
        .rposition(|piece| matches!(piece.kind, PieceKind::Text { .. } | PieceKind::Atomic { .. }));
    // The clusters of the line in logical order, and the gaps inside them.
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut inside = 0;
    for (index, piece) in pieces.iter().enumerate() {
        let PieceKind::Text { text, glyphs, .. } = &piece.kind else {
            if matches!(piece.kind, PieceKind::Atomic { .. }) {
                clusters.push(Cluster {
                    piece: index,
                    left: 0,
                    right: 0,
                    cursive: false,
                    rtl: piece.bidi.is_rtl(),
                    atomic: true,
                });
            }
            continue;
        };
        let end = if Some(index) == last_text {
            text.trim_end_matches(' ').len()
        } else {
            text.len()
        };
        let rtl = piece.bidi.is_rtl();
        let mut order: Vec<usize> = (0..glyphs.len()).collect();
        if rtl {
            order.reverse();
        }
        let mut previous = None;
        for position in order {
            let cluster = glyphs[position].cluster.0;
            let start = cluster as usize;
            if start >= end {
                continue;
            }
            let character = text[start..].chars().next().unwrap_or(' ');
            if previous == Some(cluster) {
                if let Some(last) = clusters.last_mut()
                    && last.piece == index
                {
                    last.left = last.left.min(position);
                    last.right = last.right.max(position);
                }
                continue;
            }
            previous = Some(cluster);
            if is_default_ignorable(character) {
                continue;
            }
            let cluster_end = (glyphs[position].cluster.1 as usize).clamp(start, end);
            inside += text[start..cluster_end]
                .chars()
                .filter(|&character| !is_mark(character) && !is_default_ignorable(character))
                .count()
                .saturating_sub(1);
            clusters.push(Cluster {
                piece: index,
                left: position,
                right: position,
                cursive: is_cursive(character),
                rtl,
                atomic: false,
            });
        }
    }
    // No gap between letters of a cursive script, nor between two atomic inlines.
    let gapless =
        |before: Cluster, after: Cluster| before.cursive && after.cursive || before.atomic && after.atomic;
    // The glyphs that widen, one for each gap, and whether the gap goes before the glyph on
    // its left (where the glyph moves right by it).
    let targets: Vec<(usize, usize, bool)> = clusters
        .windows(2)
        .filter(|pair| !gapless(pair[0], pair[1]))
        .map(|pair| {
            let (before, after) = (pair[0], pair[1]);
            if before.atomic && !after.rtl || after.atomic && after.rtl {
                // The atomic inline widens: the room goes after it on the line.
                if after.rtl {
                    (after.piece, 0, false)
                } else {
                    (before.piece, 0, false)
                }
            } else if before.cursive {
                if after.rtl {
                    (after.piece, after.right, false)
                } else {
                    (after.piece, after.left, true)
                }
            } else if after.rtl {
                (after.piece, after.right, false)
            } else {
                (before.piece, before.right, false)
            }
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let extra = free / (targets.len() + inside) as f32;
    for (index, position, shift) in targets {
        match &mut pieces[index].kind {
            PieceKind::Text { glyphs, .. } => {
                glyphs[position].advance += extra;
                if shift {
                    glyphs[position].x_offset += extra;
                }
            }
            PieceKind::Atomic { .. } => {}
            _ => continue,
        }
        pieces[index].width += extra;
    }
}

/// Whether a character is of a script whose letters join (Blink's `IsCursiveScript`), between
/// which justification adds no space.
fn is_cursive(character: char) -> bool {
    matches!(character,
        '\u{0600}'..='\u{08FF}'
        | '\u{1800}'..='\u{18AF}'
        | '\u{A840}'..='\u{A87F}'
        | '\u{FB50}'..='\u{FDFF}'
        | '\u{FE70}'..='\u{FEFF}'
        | '\u{10D00}'..='\u{10D3F}'
        | '\u{10F30}'..='\u{10F6F}'
        | '\u{1E900}'..='\u{1E95F}')
}

/// Whether a character is default ignorable (Unicode's `Default_Ignorable_Code_Point`), which
/// takes no part in justification nor in letter spacing.
pub(crate) fn is_default_ignorable(character: char) -> bool {
    matches!(character,
        '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{3164}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFF8}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E0FFF}')
}

/// The width of the widest piece that cannot be broken (min-content contribution) and the width
/// of all content on one line (max-content contribution).
pub fn intrinsic_widths(layouter: &Layouter<'_>, items: &[InlineItem]) -> (f32, f32) {
    intrinsic_widths_in(layouter, (items, None), (None, 0.0))
}

/// The intrinsic widths of inline content in a box whose lines wrap or not (`wraps`), if
/// known, and whose first line is indented by `indent` (`text-indent`, its percentages of
/// nothing): the indent counts for the first word and the first line, as in Blink. `block` is
/// the style of the box, if known, for the size of an initial letter.
pub(crate) fn intrinsic_widths_in(
    layouter: &Layouter<'_>,
    (items, block): (&[InlineItem], Option<&ComputedStyle>),
    (wraps, indent): (Option<bool>, f32),
) -> (f32, f32) {
    let (mut pieces, boxes) = shape_pieces(layouter, (items, None), Au::MAX, Purpose::Measure(wraps, block));
    let mut min_content: f32 = 0.0;
    let mut max_content: f32 = 0.0;
    // Punctuation that hangs counts for neither intrinsic width (CSS Text 3, §8.2).
    let start_hang = hanging::start(&pieces, &boxes);
    let mut word = indent - start_hang;
    let mut line = indent - start_hang;
    // Where the piece starts in the text of its style it belongs to (Blink's item), and that
    // style: Chrome measures a word for the min-content width between the places of its ends in
    // that text, each rounded up to layout units (`ShapeResult::CachedWidth`).
    let mut position = 0.0;
    let mut item: Option<*const ComputedStyle> = None;
    let snapped = |at: f32| Au::from_px_ceil(at).to_px();
    let mut last_trailing = 0.0;
    // The width of the inline item so far on the line of the max-content width: Blink adds up
    // the widths of whole items, each rounded up to layout units.
    let mut item_width = 0.0;
    // Whether the word so far has text, before which the breaks inside words that count for
    // the minimum may break it, between the texts of inline boxes too.
    let mut word_has_text = false;
    // The floats of the line so far, and the side of the last one.
    let mut floats = 0.0;
    let mut previous_float = None;
    let count = pieces.len();
    for index in 0..count {
        let continues = index + 1 < count && same_item(&pieces[index], &pieces[index + 1]);
        let piece = &mut pieces[index];
        let text =
            matches!(piece.kind, PieceKind::Text { .. }) && (piece.min_width - piece.width).abs() < 1e-6;
        let style = Arc::as_ptr(&piece.style);
        if !text || item != Some(style) {
            position = 0.0;
        }
        item = text.then_some(style);
        // Tabs reach their tab stops on the one line of the max-content width.
        if !piece.style.white_space.collapses_spaces()
            && matches!(&piece.kind, PieceKind::Text { text, .. } if text.contains('\t'))
        {
            let stops = tab_stops(layouter.fonts, &piece.style);
            expand_tabs(piece, line, stops);
        }
        let piece = &*piece;
        // Breaks inside words that count make the narrowest part between them the minimum.
        let inner = if breaks_inside_for_min_content(&piece.style) {
            let how = if piece.style.word_break == WordBreak::BreakAll {
                InnerBreaks::Letters
            } else {
                InnerBreaks::Overflow
            };
            how.positions(piece)
        } else {
            Vec::new()
        };
        // A float is as wide as it is on its own, beside the line, not in the words of the
        // line (Blink takes the min-content widths of floats apart from those of lines).
        let float = matches!(piece.kind, PieceKind::Measured)
            && piece.style.is_floating()
            && piece.style.initial_letter.is_none();
        if float {
            min_content = min_content.max(piece.min_width);
        }
        // The piece's width, and how much less the word is where it breaks after it, rounded
        // as Chrome rounds them.
        let (width, trailing) = if float {
            (0.0, 0.0)
        } else if text {
            let end = snapped(position + piece.width);
            (
                end - snapped(position),
                end - snapped(position + piece.width - measured_trailing_space(piece)),
            )
        } else {
            (piece.min_width, measured_trailing_space(piece))
        };
        position += piece.width;
        last_trailing = trailing;
        let is_text = matches!(&piece.kind, PieceKind::Text { text, .. } if !text.is_empty());
        if is_text && word_has_text && breaks_inside_for_min_content(&piece.style) {
            min_content = min_content.max(word);
            word = 0.0;
        }
        word_has_text |= is_text;
        if inner.is_empty() {
            word += width;
        } else {
            let mut before = 0.0;
            let mut widths = WidthsBefore::new(piece);
            // The spaces and preserved tabs that end each part hang, but those of
            // `break-spaces`, which take room.
            let own = match &piece.kind {
                PieceKind::Text { text, .. }
                    if piece.style.white_space.collapse
                        != fagus_style::values::WhiteSpaceCollapse::BreakSpaces =>
                {
                    Some(text.as_str())
                }
                _ => None,
            };
            let mut start = 0;
            for offset in inner {
                let content = own
                    .and_then(|text| text.get(start..offset))
                    .map_or(offset, |part| start + part.trim_end_matches([' ', '\t']).len());
                let content = widths.at(content);
                let width = widths.at(offset);
                min_content = min_content.max(word + content - before);
                word = 0.0;
                before = width;
                start = offset;
            }
            word = piece.min_width - before;
        }
        // Floats add up beside the line, those after a float they clear in a group of their own
        // (Blink's `FloatsMaxSize`).
        if float {
            use fagus_style::values::{Clear, Float};
            let clears = match previous_float {
                Some(Float::Left) => matches!(piece.style.clear, Clear::Left | Clear::Both),
                Some(Float::Right) => matches!(piece.style.clear, Clear::Right | Clear::Both),
                _ => false,
            };
            if clears {
                max_content = max_content.max(line + floats);
                floats = 0.0;
            }
            floats += piece.width;
            previous_float = Some(piece.style.float);
        } else {
            line += piece.width;
        }
        if matches!(piece.kind, PieceKind::Text { .. }) {
            item_width += piece.width;
            if !continues {
                // Spaces at the end of the line count as they do below.
                let hanging = if piece.forced_break_after || index + 1 == count {
                    ends_hanging(piece)
                } else {
                    0.0
                };
                line += snapped_width(item_width - hanging) - (item_width - hanging);
                item_width = 0.0;
            }
        } else {
            item_width = 0.0;
        }
        let (break_after, forced) = (piece.break_after, piece.forced_break_after);
        // A word that is shaped again where it breaks (see [`Broken`]) is as wide there as
        // shaped again, and what follows that much narrower.
        let (delta, ends) = (broken_delta(piece), ends_hanging(piece));
        if break_after {
            let hang = unmeasured_end(&pieces, index, &boxes, false);
            min_content = min_content.max(word - trailing + delta - hang);
            word = -delta;
            word_has_text = false;
        }
        if forced {
            let hang = unmeasured_end(&pieces, index, &boxes, true);
            max_content = max_content.max(line - ends - hang + floats);
            line = 0.0;
            (floats, previous_float) = (0.0, None);
        }
    }
    let trailing = pieces.last().map_or(0.0, ends_hanging);
    let (min_hang, max_hang) = match pieces.len().checked_sub(1) {
        Some(last) => (
            unmeasured_end(&pieces, last, &boxes, false),
            unmeasured_end(&pieces, last, &boxes, true),
        ),
        None => (0.0, 0.0),
    };
    min_content = min_content.max(word - last_trailing - min_hang);
    max_content = max_content.max(line - trailing - max_hang + floats);
    (min_content, max_content.max(min_content))
}

/// The width of the punctuation that hangs where a line breaks after piece `index` of `pieces`,
/// which the min-content width (or with `max` the max-content width) leaves out (CSS Text 3,
/// §8.2).
fn unmeasured_end(pieces: &[Piece], index: usize, boxes: &[InlineBox], max: bool) -> f32 {
    let Some(hang) = hanging::end(pieces[..=index].iter().rev(), boxes) else {
        return 0.0;
    };
    let last_line = hang.kind != hanging::EndKind::Close || hanging::nothing_after(&pieces[index + 1..]);
    hang.unmeasured(max, last_line)
}

/// The width of the spaces that hang at the end of `piece` where it ends a line of the
/// max-content width, at a forced break or at the end of the paragraph: none of preserved spaces
/// that hang only where they overflow (see [`hangs_if_overflowing`]).
fn ends_hanging(piece: &Piece) -> f32 {
    if hangs_if_overflowing(&piece.style) {
        0.0
    } else {
        measured_trailing_space(piece)
    }
}

/// The width of the spaces at the end of `piece` that intrinsic widths leave out: of spaces
/// and preserved tabs alone, as Chrome measures other space separators, which hang at the end
/// of a line, too.
fn measured_trailing_space(piece: &Piece) -> f32 {
    match &piece.kind {
        PieceKind::Text { glyphs, text, .. } => {
            let end = text.trim_end_matches([' ', '\t']).len();
            glyphs
                .iter()
                .filter(|glyph| glyph.cluster.0 as usize >= end)
                .map(|glyph| glyph.advance)
                .sum::<f32>()
                .min(piece.trailing_space)
        }
        _ => piece.trailing_space,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_marks_as_their_general_category_does() {
        use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};
        for character in ('\0'..='\u{3000}').chain(['\u{FE20}', '\u{1D167}', '\u{E0100}']) {
            assert_eq!(
                is_mark(character),
                character.general_category_group() == GeneralCategoryGroup::Mark,
                "{character:?}"
            );
        }
    }

    #[test]
    fn splits_justification_room_in_units_of_1_65536_pixel() {
        // A pixel over three places: 21845/65536 for each but the last, which takes the rest.
        let (share, last) = expansion_shares(1.0, 3);
        assert!((share - 21845.0 / 65536.0).abs() < 1e-7, "{share}");
        assert!((last - 21846.0 / 65536.0).abs() < 1e-7, "{last}");
        // The room is taken in layout units first.
        let (share, last) = expansion_shares(10.0 + 1.0 / 256.0, 2);
        assert!(
            (share - 5.0).abs() < 1e-6 && (last - 5.0).abs() < 1e-6,
            "{share} {last}"
        );
    }

    #[test]
    fn hangs_spaces_and_ideographic_spaces_alone() {
        // Chrome lets other space separators (em, thin, Ogham spaces) take room at the end of a
        // line: `a b\u{2003}c d` in 3ch breaks after `a `, not after the em space.
        assert!(hangs(' ') && hangs('\u{3000}'));
        for other in [
            '\u{1680}', '\u{2003}', '\u{2009}', '\u{205F}', '\u{A0}', '\u{2007}',
        ] {
            assert!(!hangs(other), "{other:?}");
        }
    }

    #[test]
    fn counts_the_combining_marks_of_a_cluster() {
        assert_eq!(marks_len("\u{301}x"), 2);
        assert_eq!(marks_len("\u{323}\u{302}n"), 4);
        // Devanagari vowel signs and nukta are marks; letters and joiners are not.
        assert_eq!(marks_len("\u{93C}\u{93F}क"), 6);
        assert_eq!(marks_len("x\u{301}"), 0);
        assert_eq!(marks_len("\u{200D}"), 0);
    }

    #[test]
    fn cuts_long_words_that_break_inside_into_parts() {
        let text = format!("{} {}", "ab".repeat(5000), "short");
        let parts = vec![
            Part {
                range: 0..10_001,
                end: PartEnd::Break,
            },
            Part {
                range: 10_001..text.len(),
                end: PartEnd::Break,
            },
        ];
        for break_all in [true, false] {
            let cut = cut_long_parts(&text, parts.clone(), break_all);
            assert!(cut.len() > 3);
            assert!(cut.iter().all(|part| part.range.len() <= LONG_TEXT));
            assert!(
                cut.windows(2)
                    .all(|pair| pair[0].range.end == pair[1].range.start)
            );
            assert_eq!(
                cut.last().map(|part| part.range.clone()),
                Some(10_001..text.len())
            );
            let inner = if break_all {
                PartEnd::Break
            } else {
                PartEnd::Inside
            };
            assert!(cut[..cut.len() - 2].iter().all(|part| part.end == inner));
        }
    }
}
