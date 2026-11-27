//! PDF output for Fagus, written with krilla.
//!
//! Display lists are in CSS pixels with the origin at the top left, like krilla's surfaces. Every
//! page is scaled from pixels (1/96 inch) to PDF points (1/72 inch).

#![forbid(unsafe_code)]

mod avif;
mod clip_text;
mod compact;
mod decode;
mod encrypt;
mod filtered;
pub mod foreign_layers;
mod forms;
mod layers;
mod lossy;
mod memo;
pub mod merge;
mod non_scaling;
pub use decode::alpha as image_alpha;
pub use memo::Images;
mod outline_font;
mod parse;
pub mod pdf_image;
mod problems;
mod raster;
mod rasterizer;
mod shadow;
pub mod sign;
mod small_caps;
pub mod svg;
pub mod svg_boxes;
mod svg_limits;
pub mod svg_reference;
mod tagging;
mod tainted;
mod text_anchor;
mod update;
mod view;
mod xmp;

pub use encrypt::{Encryption, Permission};
pub use forms::{ButtonAction, Field, FieldKind, FieldWidget};
pub use merge::{MergeInput, Merged, PageRanges};
pub use tagging::{Element, Numbering, Owner, Repeat, Role, Scope, Structure};
pub use view::{InitialView, PageLayout, PageMode, Zoom};
pub use xmp::Extension;

use std::sync::Arc;

use fagus_dom::NodeId;
use fagus_layout::{ImageFormat, PageOrientation};
use fagus_paint::{ClipGeometry, GradientPaint, Item, PageList, Radii, RectF};
use fagus_style::values::{BorderStyle, Rgba};
use fagus_text::{FaceId, FontDb, Outlines};
use krilla::Document;
use krilla::color::rgb;
use krilla::geom::{Path, PathBuilder, Point, Rect, Size, Transform};
use krilla::num::NormalizedF32;
use krilla::page::PageSettings;
use krilla::paint::{
    Fill, FillRule, LineCap, LinearGradient, Paint, RadialGradient, SpreadMethod, Stop, Stroke, StrokeDash,
    SweepGradient,
};
use krilla::surface::Surface;
use krilla::text::{Font, GlyphId, KrillaGlyph, Tag};
use rasterizer::Outline;
use rustc_hash::FxHashMap;

/// Pixels to points.
const SCALE: f32 = 72.0 / 96.0;

/// Pixels to points as Chrome draws a page: 1/300 inch per 0.23999999 points (0.24 in single
/// precision, the number Chrome writes), 3.125 dots per pixel, a hair less than [`SCALE`], so
/// that edges that lie exactly between two pixels of a rasterized page fall on the same side as
/// Chrome's.
const DRAW_SCALE: f32 = 0.749_999_94;

/// The slant of synthetic italics: a quarter of the height, about 14 degrees, as Chrome sets it
/// (Skia's skew of -1/4 in Blink's `FontPlatformData`).
pub(crate) const SLANT: f32 = 0.25;

/// How wide a line thickens the glyphs of synthetic bold text at `size` pixels: 1/24 of the size
/// up to 9 pixels, 1/32 from 36 pixels on, in proportion between, as Skia emboldens text in
/// Chrome (`kStdFakeBoldInterpKeys` and `kStdFakeBoldInterpValues`).
pub(crate) fn fake_bold_width(size: f32) -> f32 {
    const KEYS: (f32, f32) = (9.0, 36.0);
    const VALUES: (f32, f32) = (1.0 / 24.0, 1.0 / 32.0);
    let t = ((size - KEYS.0) / (KEYS.1 - KEYS.0)).clamp(0.0, 1.0);
    size * (VALUES.0 + t * (VALUES.1 - VALUES.0))
}

/// Document metadata.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub keywords: Vec<String>,
    pub language: Option<String>,
    pub creator: Option<String>,
    /// When the document was made; also the date of its attachments. PDF/A requires one.
    pub created: Option<Date>,
}

/// A date and time in UTC.
///
/// `#[non_exhaustive]`, so that a field (a fraction of a second, an offset from UTC) can be added
/// in a minor release: make one with [`Date::new`], [`Date::from_unix`] or [`Date::now`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Date {
    /// The date and time of the numbers, in UTC: `month` 1 to 12, `day` 1 to 31, `hour` 0 to 23,
    /// `minute` 0 to 59, `second` 0 to 60. They are written as given, not checked.
    pub const fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// The current time.
    pub fn now() -> Self {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        Self::from_unix(seconds)
    }

    /// The date of a Unix timestamp (seconds since 1970-01-01 UTC).
    pub fn from_unix(seconds: u64) -> Self {
        // Days to the civil calendar (H. Hinnant, `civil_from_days`).
        let days = (seconds / 86_400) as i64;
        let time = seconds % 86_400;
        let shifted = days + 719_468;
        let era = shifted.div_euclid(146_097);
        let day_of_era = shifted - era * 146_097;
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_index = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_index + 2) / 5 + 1;
        let month = if month_index < 10 {
            month_index + 3
        } else {
            month_index - 9
        };
        let year = year_of_era + era * 400 + i64::from(month <= 2);
        Self {
            year: year.clamp(0, 9999) as u16,
            month: month as u8,
            day: day as u8,
            hour: (time / 3600) as u8,
            minute: (time % 3600 / 60) as u8,
            second: (time % 60) as u8,
        }
    }

    fn to_krilla(self) -> krilla::metadata::DateTime {
        krilla::metadata::DateTime::new(self.year)
            .month(self.month)
            .day(self.day)
            .hour(self.hour)
            .minute(self.minute)
            .second(self.second)
            .utc_offset_hour(0)
            .utc_offset_minute(0)
    }
}

/// A PDF/A level the document conforms to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Conformance {
    /// PDF/A-1b: archivable as PDF 1.4, without transparency.
    PdfA1b,
    /// PDF/A-2b: archivable, with a reliable visual appearance.
    PdfA2b,
    /// PDF/A-2u: PDF/A-2b with all text in Unicode.
    PdfA2u,
    /// PDF/A-2a: PDF/A-2u, tagged for accessibility.
    PdfA2a,
    /// PDF/A-3b: like PDF/A-2b, with files of any type attached (ZUGFeRD and Factur-X).
    PdfA3b,
    /// PDF/A-3u: PDF/A-3b with all text in Unicode.
    PdfA3u,
    /// PDF/A-3a: PDF/A-3u, tagged for accessibility.
    PdfA3a,
    /// PDF/A-4: archivable as PDF 2.0, with all text in Unicode.
    PdfA4,
    /// PDF/A-4e: PDF/A-4 for engineering documents (3D and rich media, which Fagus does not
    /// write).
    PdfA4e,
    /// PDF/A-4f: PDF/A-4 with files of any type attached.
    PdfA4f,
}

impl Conformance {
    /// The level named like `3b`.
    pub fn from_level(level: &str) -> Option<Self> {
        Some(match level {
            "1b" => Self::PdfA1b,
            "2a" => Self::PdfA2a,
            "2b" => Self::PdfA2b,
            "2u" => Self::PdfA2u,
            "3a" => Self::PdfA3a,
            "3b" => Self::PdfA3b,
            "3u" => Self::PdfA3u,
            "4" => Self::PdfA4,
            "4e" => Self::PdfA4e,
            "4f" => Self::PdfA4f,
            _ => return None,
        })
    }

    /// The name of the level, like `PDF/A-3b`.
    pub fn name(self) -> &'static str {
        match self {
            Self::PdfA1b => "PDF/A-1b",
            Self::PdfA2a => "PDF/A-2a",
            Self::PdfA2b => "PDF/A-2b",
            Self::PdfA2u => "PDF/A-2u",
            Self::PdfA3a => "PDF/A-3a",
            Self::PdfA3b => "PDF/A-3b",
            Self::PdfA3u => "PDF/A-3u",
            Self::PdfA4 => "PDF/A-4",
            Self::PdfA4e => "PDF/A-4e",
            Self::PdfA4f => "PDF/A-4f",
        }
    }

    /// The part of ISO 19005 the level belongs to: 1 to 4.
    pub fn part(self) -> u8 {
        match self {
            Self::PdfA1b => 1,
            Self::PdfA2b | Self::PdfA2u | Self::PdfA2a => 2,
            Self::PdfA3b | Self::PdfA3u | Self::PdfA3a => 3,
            Self::PdfA4 | Self::PdfA4e | Self::PdfA4f => 4,
        }
    }

    /// Whether the level requires a tagged PDF.
    pub fn is_tagged(self) -> bool {
        matches!(self, Self::PdfA2a | Self::PdfA3a)
    }
}

/// How an attached file relates to the document (PDF 2.0 `AFRelationship`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Relationship {
    Source,
    Data,
    Alternative,
    Supplement,
    Unspecified,
}

/// A file attached to the document.
///
/// `#[non_exhaustive]`, so that a field can be added in a minor release: make one with
/// [`Attachment::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Attachment {
    pub name: String,
    pub mime_type: String,
    pub description: Option<String>,
    pub relationship: Relationship,
    pub data: Arc<Vec<u8>>,
}

impl Attachment {
    /// A file named `name` with its MIME type (`text/csv`) and data, without a description and
    /// with the relationship [`Relationship::Unspecified`].
    pub fn new(name: impl Into<String>, mime_type: impl Into<String>, data: impl Into<Arc<Vec<u8>>>) -> Self {
        Self {
            name: name.into(),
            mime_type: mime_type.into(),
            description: None,
            relationship: Relationship::Unspecified,
            data: data.into(),
        }
    }

    /// The attachment with a description, which PDF/A-3 and PDF/UA require.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The attachment with how it relates to the document.
    #[must_use]
    pub fn relationship(mut self, relationship: Relationship) -> Self {
        self.relationship = relationship;
        self
    }
}

/// How the PDF is written beyond its pages.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub conformance: Option<Conformance>,
    pub attachments: Vec<Attachment>,
    /// Additions to the document's XMP metadata (for example Factur-X's).
    pub xmp: Option<Extension>,
    /// The links of each page, by page index.
    pub links: Vec<Vec<Link>>,
    /// The entries of the document outline, in document order.
    pub bookmarks: Vec<Bookmark>,
    /// The logical structure, for a tagged PDF.
    pub structure: Option<Structure>,
    /// Whether the PDF conforms to PDF/UA-1, the standard for accessible PDF. It needs a
    /// structure (without one, all content belongs to the document as a whole), a title, and
    /// alternative text for images.
    pub accessible: bool,
    /// Images prepared for earlier documents, and where those of this one are kept.
    pub images: Images,
    /// The passwords and permissions of an encrypted PDF. PDF/A forbids encryption, so it
    /// cannot go with `conformance`; with `accessible`, it allows the extraction for
    /// accessibility, as PDF/UA requires.
    pub encryption: Option<Encryption>,
    /// The fields of an interactive form.
    pub fields: Vec<Field>,
    /// The quality, from 1 to 100, of the JPEGs that raster images are written as where their
    /// pixels are all opaque (and, for an image of a format without loss, where the JPEG is
    /// smaller than its file); another value is an error. Without one, the pixels of images are
    /// compressed without loss. JPEG images are embedded as they are either way.
    pub image_quality: Option<u8>,
    /// How a viewer shows the PDF when it opens it. What the document cannot have (see
    /// [`InitialView::checked`]) is not written.
    pub view: Option<InitialView>,
}

/// A link on a page.
#[derive(Clone, Debug)]
pub struct Link {
    /// The area that can be clicked, in CSS pixels from the page's top left corner: x, y, width
    /// and height.
    pub rect: [f32; 4],
    pub target: LinkTarget,
    /// The element of the link, for a tagged PDF.
    pub node: Option<NodeId>,
    /// What the link is, for readers that do not see it: usually its text.
    pub description: Option<String>,
    /// What the link is in the structure of a tagged PDF.
    pub role: LinkRole,
}

/// What a link is in the structure of a tagged PDF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkRole {
    /// The link of its element (`node`): it belongs to the element's structure element.
    #[default]
    Element,
    /// The call of the footnote whose element is `node`, which leads to the footnote.
    FootnoteCall,
    /// The marker of the footnote whose element is `node`, which leads back to its call.
    FootnoteMarker,
}

/// Where a link leads.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    /// A URI (`https:`, `mailto:`, …).
    Uri(String),
    /// A point in the document: the page index and the position in CSS pixels.
    Position { page: usize, x: f32, y: f32 },
}

/// An entry of the document outline (the bookmarks of a PDF viewer).
#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    /// 1 for the top level; deeper levels nest under the entry before them.
    pub level: u8,
    pub title: String,
    pub page: usize,
    /// The left of the target in CSS pixels.
    pub x: f32,
    /// The top of the target in CSS pixels.
    pub y: f32,
    /// Whether the entry shows its children.
    pub open: bool,
}

/// An error while writing the PDF.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PdfError {
    #[error("the PDF could not be written: {0}")]
    Write(String),
    #[error("the document does not meet {standard}: {problems}")]
    Conformance { standard: String, problems: String },
    #[error("the PDF could not be signed: {0}")]
    Sign(String),
    /// An image quality that is not from 1 to 100 ([`Options::image_quality`]).
    #[error("the image quality must be between 1 and 100, not {0}")]
    ImageQuality(u8),
    /// A merge without PDFs to merge.
    #[error("there are no PDF files to merge")]
    NothingToMerge,
    /// A PDF to merge that cannot be read; `input` is its index among the PDFs, from 0.
    #[error("the PDF at index {input} cannot be read: {reason}")]
    Unreadable { input: usize, reason: String },
    /// A PDF to merge that is encrypted, with a password to open it or one that restricts what
    /// may be done with it; `input` is its index among the PDFs, from 0.
    #[error("the PDF at index {input} is encrypted; only PDFs without encryption can be merged")]
    Encrypted { input: usize },
    /// A page to merge that a PDF does not have: the page (from 1) and how many the PDF has;
    /// `input` is its index among the PDFs, from 0.
    #[error(
        "the PDF at index {input} has {pages} {}, not page {page}",
        if *pages == 1 { "page" } else { "pages" }
    )]
    NoPage { input: usize, page: usize, pages: usize },
    /// A selection of pages that cannot be read (see [`PageRanges::parse`]).
    #[error("invalid pages `{text}`: {reason}")]
    PageRanges { text: String, reason: String },
}

/// A PDF document, written.
#[derive(Debug)]
pub struct Written {
    pub pdf: Vec<u8>,
    /// The data of the images left out, which turned out not to decode.
    pub broken_images: Vec<Arc<Vec<u8>>>,
}

/// Writes display lists as a PDF document. The lists are dropped once written, before the PDF
/// is compacted, so that a document of many pages does not hold both at once.
pub fn write(
    mut pages: Vec<PageList>,
    fonts: &FontDb,
    metadata: &Metadata,
    options: &Options,
) -> Result<Written, PdfError> {
    if options.encryption.is_some() && options.conformance.is_some() {
        return Err(PdfError::Conformance {
            standard: standards(options),
            problems: "the PDF cannot be encrypted (PDF/A forbids encryption)".to_owned(),
        });
    }
    if let Some(quality) = options.image_quality
        && !(1..=100).contains(&quality)
    {
        return Err(PdfError::ImageQuality(quality));
    }
    // Before the document is written: a missing password fails without the work.
    let mut encryptor = options
        .encryption
        .as_ref()
        .map(encrypt::Encryptor::new)
        .transpose()?;
    let page_count = pages.len();
    let written = write_uncompacted(&mut pages, fonts, metadata, options)?;
    drop(pages);
    // A viewer shows the title in place of the file name, as with Chrome's PDFs.
    let view = options
        .view
        .as_ref()
        .map(|view| view.checked(options.conformance, page_count).0);
    let preferences = InitialView::preferences(view.as_ref(), metadata.title.is_some());
    let mut entries: Vec<(&str, String)> = Vec::new();
    if !preferences.is_empty() {
        entries.push(("/ViewerPreferences", format!("<<{preferences}>>")));
    }
    if let Some(view) = &view {
        entries.extend(view.entries(&written.pdf));
    }
    let entries: Vec<(&str, &str)> = entries
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let catalog = &entries[..];
    // An encrypted PDF is written in the same pass as the object streams; unlike them, it must
    // not fail quietly.
    if let Some(encryptor) = &mut encryptor {
        let pdf = compact::compact(&written.pdf, catalog, Some(encryptor)).ok_or_else(|| {
            encryptor
                .take_error()
                .unwrap_or_else(|| PdfError::Write("the PDF could not be encrypted".to_owned()))
        })?;
        return Ok(Written { pdf, ..written });
    }
    // Object streams make the PDF smaller; should that fail, the PDF stays as it is. PDF/A-1
    // is PDF 1.4, which has none.
    let rewritten = if options
        .conformance
        .is_some_and(|conformance| conformance.part() == 1)
    {
        compact::classic(&written.pdf, catalog)
    } else {
        compact::compact(&written.pdf, catalog, None)
    };
    Ok(Written {
        pdf: rewritten.unwrap_or(written.pdf),
        ..written
    })
}

/// The PDF as krilla writes it, with every object on its own. An image whose data turns out
/// not to decode only as the PDF is written is left out, as browsers leave such images out, and
/// the PDF written again without it. Pages without images are let go of as they are written: a
/// long document then need not hold the display lists of all its pages until the end.
pub(crate) fn write_uncompacted(
    pages: &mut [PageList],
    fonts: &FontDb,
    metadata: &Metadata,
    options: &Options,
) -> Result<Written, PdfError> {
    // Only images make the PDF be written again, with every page drawn again.
    let release = !pages.iter().any(|page| page.items.iter().any(draws_image));
    let mut image_cache = ImageCache::new(options);
    let mut broken_images = Vec::new();
    loop {
        match attempt(pages, fonts, metadata, options, &mut image_cache, release) {
            Ok(pdf) => {
                for data in image_cache.undecoded {
                    if !broken_images.iter().any(|known| Arc::ptr_eq(known, &data)) {
                        broken_images.push(data);
                    }
                }
                return Ok(Written { pdf, broken_images });
            }
            Err(Failure::Pdf(error)) => return Err(error),
            // Not without the pages let go of; there are none to draw again where nothing
            // draws an image.
            Err(Failure::Image(_, message)) if release => return Err(PdfError::Write(message)),
            Err(Failure::Image(image, message)) => {
                let mut found = false;
                for ((_, _, _), embedded) in &mut image_cache.images {
                    if let Some(Embedded::Raster(raster, data)) = embedded
                        && *raster == image
                    {
                        if !broken_images.iter().any(|known| Arc::ptr_eq(known, data)) {
                            broken_images.push(Arc::clone(data));
                        }
                        *embedded = None;
                        found = true;
                    }
                }
                if !found {
                    return Err(PdfError::Write(message));
                }
            }
        }
    }
}

/// Why an attempt to write the PDF failed.
enum Failure {
    /// An image does not decode.
    Image(krilla::image::Image, String),
    Pdf(PdfError),
}

impl From<PdfError> for Failure {
    fn from(error: PdfError) -> Self {
        Self::Pdf(error)
    }
}

impl ImageCache {
    /// An empty cache for a PDF written with `options`.
    fn new(options: &Options) -> Self {
        Self {
            interpolate: options.conformance.is_none(),
            opaque: options
                .conformance
                .is_some_and(|conformance| conformance.part() == 1),
            quality: options.image_quality,
            images: FxHashMap::default(),
            strokes: FxHashMap::default(),
            sized: FxHashMap::default(),
            unclipped: FxHashMap::default(),
            memo: options.images.clone(),
            shadows: FxHashMap::default(),
            text_shadows: FxHashMap::default(),
            morphology: FxHashMap::default(),
            svgs: FxHashMap::default(),
            svg_traits: FxHashMap::default(),
            undecoded: Vec::new(),
            layers: layers::Layers::default(),
            pdfs: FxHashMap::default(),
        }
    }
}

/// Whether `item` draws an image, itself or in a tile, a mask or a filtered group.
fn draws_image(item: &Item) -> bool {
    match item {
        Item::Image { .. } => true,
        Item::Tiles { tile, .. } => draws_image(tile),
        Item::PushMask(mask) => mask.items.iter().any(draws_image),
        Item::Filtered(filtered) => filtered.items.iter().any(draws_image),
        _ => false,
    }
}

/// Writes the PDF once, with the images prepared so far. With `release`, the items of each page
/// go once it is drawn; the pages keep their sizes, which links and bookmarks need.
fn attempt(
    pages: &mut [PageList],
    fonts: &FontDb,
    metadata: &Metadata,
    options: &Options,
    image_cache: &mut ImageCache,
    release: bool,
) -> Result<Vec<u8>, Failure> {
    // krilla's surfaces must be finished before they are dropped: a budget or time that runs
    // out while a page is drawn ends the render once the page is done (see
    // `fagus_dom::budget::defer`).
    let _deferred = fagus_dom::budget::defer();
    let fallback = Structure::default();
    let structure = options.structure.as_ref().or_else(|| {
        let tagged = options.accessible || options.conformance.is_some_and(Conformance::is_tagged);
        tagged.then_some(&fallback)
    });
    let mut settings = krilla::SerializeSettings {
        enable_tagging: structure.is_some(),
        ..krilla::SerializeSettings::default()
    };
    let mut configuration = krilla::configure::ConfigurationBuilder::new();
    if let Some(conformance) = options.conformance {
        configuration = configuration.with_archival_validator(match conformance {
            Conformance::PdfA1b => krilla::configure::Archival::A1_B,
            Conformance::PdfA2b => krilla::configure::Archival::A2_B,
            Conformance::PdfA2u => krilla::configure::Archival::A2_U,
            Conformance::PdfA2a => krilla::configure::Archival::A2_A,
            Conformance::PdfA3b => krilla::configure::Archival::A3_B,
            Conformance::PdfA3u => krilla::configure::Archival::A3_U,
            Conformance::PdfA3a => krilla::configure::Archival::A3_A,
            Conformance::PdfA4 => krilla::configure::Archival::A4,
            Conformance::PdfA4e => krilla::configure::Archival::A4E,
            Conformance::PdfA4f => krilla::configure::Archival::A4F,
        });
    }
    if options.accessible {
        configuration = configuration.with_accessibility_validator(krilla::configure::Accessibility::UA1);
    }
    // PDF/UA-1 builds on PDF 1.7: krilla would write it with PDF/A-1b as PDF 1.4, without
    // what PDF/UA-1 needs of PDF 1.5 and later (the scope of table header cells), and PDF/A-4
    // is PDF 2.0.
    let apart = || {
        PdfError::Write(format!(
            "{} need different PDF versions and cannot be written together (PDF/UA-1 goes with \
             PDF/A-2 and PDF/A-3)",
            standards(options)
        ))
    };
    if options.accessible
        && options
            .conformance
            .is_some_and(|conformance| conformance.part() == 1)
    {
        return Err(apart().into());
    }
    settings.configuration = configuration.finish().map_err(|_| apart())?;
    let mut document = Document::new_with(settings);
    let mut tagger = structure.map(|structure| tagging::Tagger::new(structure, pages));
    let mut font_cache = FontCache::new(pages, &options.fields, fonts);
    font_cache.seed_notdef = options.conformance.is_none() && !options.accessible;
    // PDF/A-1 is PDF 1.4, which has no optional content.
    image_cache.layers = layers::Layers::new(
        options
            .conformance
            .is_none_or(|conformance| conformance.part() > 1),
    );
    // The widgets of the fields, by page.
    let mut widgets: FxHashMap<usize, Vec<(usize, usize)>> = FxHashMap::default();
    for (field, widget) in options
        .fields
        .iter()
        .enumerate()
        .flat_map(|(index, field)| (0..field.widgets.len()).map(move |widget| (index, widget)))
    {
        let page = options.fields[field].widgets[widget].page;
        if page < pages.len() {
            widgets.entry(page).or_default().push((field, widget));
        }
    }
    let mut appearances: Vec<forms::Appearance> = Vec::new();
    for index in 0..pages.len() {
        fagus_dom::budget::check_time_now();
        fagus_dom::budget::settle();
        let page = &pages[index];
        // Every page is written, so that page indices stay those of the layout. With bleed or
        // marks, the sheet is larger: the page is its trim box, inside its bleed box, with room
        // for the marks around it. A turned page lies on its sheet turned.
        let marked = page.marks.crop || page.marks.cross;
        let around = sheet_margin(page);
        let (width, height) = sheet_size(page);
        let (media_width, media_height) = media_size(page);
        let size = Size::from_wh(media_width, media_height).expect("a positive page size");
        let mut settings = PageSettings::new(size);
        if around > 0.0 {
            let boxed = |inset: f32| {
                Rect::from_xywh(
                    inset * SCALE,
                    inset * SCALE,
                    (width - 2.0 * inset) * SCALE,
                    (height - 2.0 * inset) * SCALE,
                )
            };
            settings = settings
                .with_trim_box(boxed(around))
                .with_bleed_box(boxed(around - page.bleed));
        }
        let mut pdf_page = document.start_page_with(settings);
        let mut surface = pdf_page.surface();
        surface.push_transform(&Transform::from_scale(DRAW_SCALE, DRAW_SCALE));
        surface.push_transform(&turn(page));
        if marked {
            draw_marks(&mut surface, page, around, tagger.is_some());
        }
        surface.push_transform(&Transform::from_translate(around, around));
        let mut transforms = vec![Some(IDENTITY)];
        // The sheet, a little larger (its media box is rounded up): nothing outside it shows.
        let sheet = RectF {
            x: -around - 2.0,
            y: -around - 2.0,
            width: page.width + 2.0 * around + 4.0,
            height: page.height + 2.0 * around + 4.0,
        };
        // Without transparency (PDF/A-1), each item tells krilla where it is, for the message
        // that names what is transparent; a group ends where it began.
        let mut started = Vec::new();
        let opaque = image_cache.opaque;
        let mut draw_item = |surface: &mut Surface<'_>, item: &Item| {
            if opaque {
                let location = match item {
                    Item::Pop => started.pop(),
                    _ => Some(problems::locate(index, item)),
                };
                if let Some(location) = location {
                    surface.set_location(location);
                }
                if is_push(item) {
                    started.push(problems::locate(index, item));
                }
            }
            draw(
                surface,
                item,
                fonts,
                &mut font_cache,
                image_cache,
                &mut transforms,
                Some(sheet),
            );
        };
        match &mut tagger {
            Some(tagger) => tagger.draw(&mut surface, index, &page.items, &mut draw_item),
            None => {
                for item in &page.items {
                    draw_item(&mut surface, item);
                }
            }
        }
        surface.pop();
        surface.pop();
        surface.pop();
        surface.finish();
        for link in options.links.get(index).into_iter().flatten() {
            if let Some(annotation) = link_annotation(link, page, pages) {
                match &mut tagger {
                    Some(tagger) => {
                        let id = pdf_page.add_tagged_annotation(annotation);
                        tagger.annotation(link.node, link.role, id);
                    }
                    None => pdf_page.add_annotation(annotation),
                }
            }
        }
        // Each widget starts as a link, to become a widget once the PDF is written.
        for &(field, position) in widgets.get(&index).into_iter().flatten() {
            let widget = &options.fields[field].widgets[position];
            let placeholder = Link {
                rect: widget.rect,
                target: LinkTarget::Uri(forms::placeholder(field, position)),
                node: widget.node,
                description: options.fields[field]
                    .description
                    .clone()
                    .or_else(|| Some(options.fields[field].name.clone())),
                role: LinkRole::Element,
            };
            let (Some(annotation), Some(_)) = (
                link_annotation(&placeholder, page, pages),
                Size::from_wh(widget.size.0 * SCALE, widget.size.1 * SCALE),
            ) else {
                continue;
            };
            match &mut tagger {
                Some(tagger) => {
                    let id = pdf_page.add_tagged_annotation(annotation);
                    tagger.widget(widget.node, id);
                }
                None => pdf_page.add_annotation(annotation),
            }
            let turn = match page.orientation {
                PageOrientation::Upright => [1.0, 0.0, 0.0, 1.0],
                PageOrientation::RotateLeft => [0.0, 1.0, -1.0, 0.0],
                PageOrientation::RotateRight => [0.0, -1.0, 1.0, 0.0],
            };
            let matrix = forms::matrix(widget.transform, turn);
            let buttons = matches!(options.fields[field].kind, FieldKind::Check | FieldKind::Radio);
            for off in std::iter::once(false).chain(buttons.then_some(true)) {
                appearances.push(forms::Appearance {
                    field,
                    widget: position,
                    off,
                    matrix,
                    bbox: forms::bbox(widget),
                });
            }
        }
        pdf_page.finish();
        if release {
            pages[index].items = Vec::new();
        }
    }
    fagus_dom::budget::settle();
    // What the widgets show, each on a page of its own after the document's pages.
    for appearance in &appearances {
        let widget = &options.fields[appearance.field].widgets[appearance.widget];
        let Some(size) = Size::from_wh(widget.size.0 * SCALE, widget.size.1 * SCALE) else {
            continue;
        };
        let mut pdf_page = document.start_page_with(PageSettings::new(size));
        let mut surface = pdf_page.surface();
        surface.push_transform(&Transform::from_scale(DRAW_SCALE, DRAW_SCALE));
        let mut transforms = vec![Some(IDENTITY)];
        let items = if appearance.off {
            &widget.off
        } else {
            &widget.appearance
        };
        for item in items {
            draw(
                &mut surface,
                item,
                fonts,
                &mut font_cache,
                image_cache,
                &mut transforms,
                None,
            );
        }
        surface.pop();
        surface.finish();
        pdf_page.finish();
    }
    if let Some(tagger) = tagger {
        document.set_tag_tree(tagger.finish());
    }
    // PDF/UA asks for an outline, even an empty one.
    if !options.bookmarks.is_empty() || options.accessible {
        let mut outline = krilla::outline::Outline::new();
        let valid: Vec<Bookmark> = options
            .bookmarks
            .iter()
            .filter(|bookmark| bookmark.page < pages.len())
            .cloned()
            .collect();
        for node in outline_nodes(&valid, pages, &mut 0, 0) {
            outline.push_child(node);
        }
        document.set_outline(outline);
    }
    let mut krilla_metadata = krilla::metadata::Metadata::new().producer("Fagus".to_owned());
    if let Some(title) = &metadata.title {
        krilla_metadata = krilla_metadata.title(title.clone());
    }
    if !metadata.authors.is_empty() {
        krilla_metadata = krilla_metadata.authors(metadata.authors.clone());
    }
    if let Some(description) = &metadata.description {
        krilla_metadata = krilla_metadata.description(description.clone());
    }
    if !metadata.keywords.is_empty() {
        krilla_metadata = krilla_metadata.keywords(metadata.keywords.clone());
    }
    if let Some(language) = &metadata.language {
        krilla_metadata = krilla_metadata.language(language.clone());
    }
    if let Some(creator) = &metadata.creator {
        krilla_metadata = krilla_metadata.creator(creator.clone());
    }
    if let Some(created) = metadata.created {
        krilla_metadata = krilla_metadata.creation_date(created.to_krilla());
    }
    document.set_metadata(krilla_metadata);
    for attachment in &options.attachments {
        let data: Arc<Vec<u8>> = Arc::clone(&attachment.data);
        let file = krilla::embed::EmbeddedFile {
            path: attachment.name.clone(),
            mime_type: krilla::embed::MimeType::new(&attachment.mime_type),
            description: attachment.description.clone(),
            association_kind: match attachment.relationship {
                Relationship::Source => krilla::embed::AssociationKind::Source,
                Relationship::Data => krilla::embed::AssociationKind::Data,
                Relationship::Alternative => krilla::embed::AssociationKind::Alternative,
                Relationship::Supplement => krilla::embed::AssociationKind::Supplement,
                Relationship::Unspecified => krilla::embed::AssociationKind::Unspecified,
            },
            data: data.into(),
            modification_date: metadata.created.map(Date::to_krilla),
            compress: None,
            location: None,
        };
        document
            .embed_file(file)
            .ok_or_else(|| PdfError::Write(format!("`{}` could not be attached", attachment.name)))?;
    }
    // Fonts by the name of their family.
    let name = |font: &Font| {
        font_cache
            .fonts
            .iter()
            .find(|(_, cached)| cached.as_ref().is_some_and(|cached| cached.font == *font))
            .map_or_else(
                || "a font".to_owned(),
                |((face, _), _)| fonts.face(*face).family.clone(),
            )
    };
    let pdf = document.finish().map_err(|error| match error {
        krilla::error::KrillaError::Validation(problems) => {
            // Each problem once, in the order found; transparency (in PDF/A-1) in one message
            // that says where it is.
            let transparent: Vec<_> = problems
                .iter()
                .filter_map(|(problem, _)| match problem {
                    krilla::configure::ValidationError::Transparency(location) => Some(*location),
                    _ => None,
                })
                .collect();
            let mut described: Vec<String> = Vec::new();
            for (problem, _) in &problems {
                let message = match problem {
                    krilla::configure::ValidationError::Transparency(_) => {
                        problems::transparency(&transparent)
                    }
                    _ => problems::describe(problem, &name, &options.attachments, options.conformance),
                };
                if !described.contains(&message) {
                    described.push(message);
                }
            }
            Failure::Pdf(PdfError::Conformance {
                standard: standards(options),
                problems: described.join("; "),
            })
        }
        krilla::error::KrillaError::Image(image, _, message) => Failure::Image(image, message),
        krilla::error::KrillaError::Font(font, message) => Failure::Pdf(PdfError::Write(format!(
            "{} could not be embedded: {message}",
            name(&font)
        ))),
        // PDF/A-1 is written as PDF 1.4, older than any page template krilla can embed: krilla
        // stops at the version before it validates, so this is the refusal of the standard.
        krilla::error::KrillaError::Pdf(..) if options.conformance.is_some() => {
            Failure::Pdf(PdfError::Conformance {
                standard: standards(options),
                problems: problems::describe(
                    &krilla::configure::ValidationError::EmbeddedPDF(None),
                    &name,
                    &options.attachments,
                    options.conformance,
                ),
            })
        }
        error => Failure::Pdf(PdfError::Write(error.to_string())),
    })?;
    let pdf = match &options.xmp {
        Some(extension) => xmp::extend(pdf, extension).ok_or_else(|| {
            Failure::Pdf(PdfError::Write(
                "the XMP metadata could not be extended".to_owned(),
            ))
        })?,
        None => pdf,
    };
    if image_cache.layers.list.is_empty() && options.fields.is_empty() {
        return Ok(pdf);
    }
    interactive(&pdf, &image_cache.layers.list, options, &appearances, pages.len()).ok_or_else(|| {
        Failure::Pdf(PdfError::Write(
            "the layers and form fields could not be written".to_owned(),
        ))
    })
}

/// The PDF with its layers made optional content and its form fields written (see [`layers`]
/// and [`forms`]).
fn interactive(
    pdf: &[u8],
    layers: &[Arc<fagus_style::values::Layer>],
    options: &Options,
    appearances: &[forms::Appearance],
    pages: usize,
) -> Option<Vec<u8>> {
    let mut update = update::Update::new(pdf)?;
    // PDF/A and PDF/UA leave the layers to the reader.
    let groups = layers::write(
        &mut update,
        layers,
        options.conformance.is_some() || options.accessible,
    )?;
    let groups: FxHashMap<Arc<str>, u32> = layers
        .iter()
        .zip(groups)
        .map(|(layer, group)| (Arc::clone(&layer.name), group))
        .collect();
    forms::write(&mut update, &options.fields, appearances, pages, &groups)?;
    update.finish()
}

/// The standards the PDF is to meet, like `PDF/A-3a and PDF/UA-1`.
fn standards(options: &Options) -> String {
    let mut standards: Vec<&str> = options.conformance.map(Conformance::name).into_iter().collect();
    if options.accessible {
        standards.push("PDF/UA-1");
    }
    standards.join(" and ")
}

/// The annotation of a link, if its area and target are valid.
fn link_annotation(
    link: &Link,
    page: &PageList,
    pages: &[PageList],
) -> Option<krilla::annotation::Annotation> {
    use krilla::action::LinkAction;
    use krilla::annotation::{Annotation, LinkAnnotation, Target};
    use krilla::destination::XyzDestination;

    let [x, y, width, height] = link.rect;
    let (x0, y0) = on_sheet(page, (x, y));
    let (x1, y1) = on_sheet(page, (x + width, y + height));
    let rect = Rect::from_ltrb(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))?;
    let target = match &link.target {
        LinkTarget::Uri(uri) => Target::Action(LinkAction::new(uri.clone()).into()),
        LinkTarget::Position { page, x, y } => {
            let (x, y) = on_sheet(pages.get(*page)?, (*x, *y));
            Target::Destination(XyzDestination::new(*page, Point::from_xy(x, y)).into())
        }
    };
    Some(Annotation::new_link(
        LinkAnnotation::new(rect, target),
        link.description.clone(),
    ))
}

/// Gives the `.notdef` glyph of `font` empty text, drawn at size 0 where it shows nothing,
/// before the glyph is drawn for characters that no font has. krilla writes no `ToUnicode` entry
/// for glyph 0 but takes the text of its first use as its text, so that the first such
/// character would be lost to text extraction and only later ones, whose text differs from
/// that, get their own (`ActualText`). With empty text first, every use of glyph 0 differs
/// from it and keeps its character.
fn seed_notdef(surface: &mut Surface<'_>, font: &Font) {
    let notdef = KrillaGlyph::new(GlyphId::new(0), 0.0, 0.0, 0.0, 0.0, 0..0, None);
    surface.draw_glyphs(Point::from_xy(0.0, 0.0), &[notdef], font.clone(), "", 0.0, false);
}

/// The PDF blend mode of a CSS one.
fn blend_mode(mode: fagus_style::values::BlendMode) -> krilla::blend::BlendMode {
    use fagus_style::values::BlendMode as Css;
    use krilla::blend::BlendMode as Pdf;
    match mode {
        Css::Normal => Pdf::Normal,
        Css::Multiply => Pdf::Multiply,
        Css::Screen => Pdf::Screen,
        Css::Overlay => Pdf::Overlay,
        Css::Darken => Pdf::Darken,
        Css::Lighten => Pdf::Lighten,
        Css::ColorDodge => Pdf::ColorDodge,
        Css::ColorBurn => Pdf::ColorBurn,
        Css::HardLight => Pdf::HardLight,
        Css::SoftLight => Pdf::SoftLight,
        Css::Difference => Pdf::Difference,
        Css::Exclusion => Pdf::Exclusion,
        Css::Hue => Pdf::Hue,
        Css::Saturation => Pdf::Saturation,
        Css::Color => Pdf::Color,
        Css::Luminosity => Pdf::Luminosity,
    }
}

/// Whether an item starts a group that a [`Item::Pop`] ends.
fn is_push(item: &Item) -> bool {
    matches!(
        item,
        Item::PushOpacity(_)
            | Item::PushBlend(_)
            | Item::PushIsolated
            | Item::PushLayer(_)
            | Item::PushMask(_)
            | Item::PushTransform(_)
            | Item::PushClip(..)
            | Item::PushClipShape(_)
    )
}

/// Whether an item starts a group that is drawn on its own, which krilla does for transparency
/// (and for an empty clip, which Fagus draws as a fully transparent group).
pub(crate) fn opens_group(item: &Item) -> bool {
    match item {
        Item::PushOpacity(opacity) => opacity.clamp(0.0, 1.0) < 1.0,
        Item::PushClip(rect, radii) => clip_path(*rect, radii.as_ref()).is_none(),
        Item::PushClipShape(shape) => shape_path(shape).is_none(),
        Item::PushBlend(_) | Item::PushIsolated | Item::PushMask(_) => true,
        _ => false,
    }
}

/// Opens a group in which nothing shows: a fully transparent one, or, `opaque` (without
/// transparency), one clipped to a square covered twice, which by the even-odd rule encloses
/// nothing.
fn hide(surface: &mut Surface<'_>, opaque: bool) {
    let square = if opaque {
        let mut builder = PathBuilder::new();
        for _ in 0..2 {
            builder.push_rect(Rect::from_xywh(0.0, 0.0, 1.0, 1.0).expect("a square"));
        }
        builder.finish()
    } else {
        None
    };
    match square {
        Some(square) => surface.push_clip_path(&square, &FillRule::EvenOdd),
        None => surface.push_opacity(NormalizedF32::ZERO),
    }
}

/// The path of a `clip-path` shape; `None` where it encloses nothing.
fn shape_path(shape: &ClipGeometry) -> Option<Path> {
    let mut builder = PathBuilder::new();
    append_shape(&mut builder, shape)?;
    builder.finish()
}

/// Appends the outline of a `clip-path` shape; `None` where it encloses nothing.
pub(crate) fn append_shape(builder: &mut impl Outline, shape: &ClipGeometry) -> Option<()> {
    // Four cubic quarters make an ellipse, their control points 0.5523 of the radius out.
    const K: f32 = 0.552_284_8;
    match shape {
        ClipGeometry::Ellipse { center, radii } => {
            if radii.0 <= 0.0 || radii.1 <= 0.0 {
                return None;
            }
            let ((cx, cy), (rx, ry)) = (*center, *radii);
            builder.move_to(cx + rx, cy);
            builder.cubic_to(cx + rx, cy + K * ry, cx + K * rx, cy + ry, cx, cy + ry);
            builder.cubic_to(cx - K * rx, cy + ry, cx - rx, cy + K * ry, cx - rx, cy);
            builder.cubic_to(cx - rx, cy - K * ry, cx - K * rx, cy - ry, cx, cy - ry);
            builder.cubic_to(cx + K * rx, cy - ry, cx + rx, cy - K * ry, cx + rx, cy);
            builder.close();
        }
        ClipGeometry::Polygon { points, .. } => {
            let (first, rest) = points.split_first()?;
            builder.move_to(first.0, first.1);
            for (x, y) in rest {
                builder.line_to(*x, *y);
            }
            builder.close();
        }
        ClipGeometry::Path { segments, .. } => append_segments(builder, segments),
    }
    Some(())
}

/// Appends an outline of lines and curves.
pub(crate) fn append_segments(builder: &mut impl Outline, segments: &[fagus_layout::PathSegment]) {
    use fagus_layout::PathSegment;
    for segment in segments {
        match *segment {
            PathSegment::Move(x, y) => builder.move_to(x, y),
            PathSegment::Line(x, y) => builder.line_to(x, y),
            PathSegment::Cubic([x1, y1, x2, y2, x, y]) => builder.cubic_to(x1, y1, x2, y2, x, y),
            PathSegment::Close => builder.close(),
        }
    }
}

/// The length of an outline of lines and curves as Skia measures it for dashes: each curve cut
/// in halves until the middle of each piece lies within half a pixel of its chord, and the
/// chords added up (`SkContourMeasure`).
fn segments_length(segments: &[fagus_layout::PathSegment]) -> f32 {
    use fagus_layout::PathSegment;
    fn pieces(
        at: &dyn Fn(f32) -> (f32, f32),
        (t0, p0): (f32, (f32, f32)),
        (t1, p1): (f32, (f32, f32)),
        (depth, tolerance): (u8, f32),
    ) -> f32 {
        let middle = f32::midpoint(t0, t1);
        let point = at(middle);
        let off = (point.0 - f32::midpoint(p0.0, p1.0))
            .abs()
            .max((point.1 - f32::midpoint(p0.1, p1.1)).abs());
        if depth < 16 && off > tolerance {
            pieces(at, (t0, p0), (middle, point), (depth + 1, tolerance))
                + pieces(at, (middle, point), (t1, p1), (depth + 1, tolerance))
        } else {
            libm::hypotf(p1.0 - p0.0, p1.1 - p0.1)
        }
    }
    let (mut start, mut current) = ((0.0, 0.0), (0.0, 0.0));
    let mut length = 0.0;
    for segment in segments {
        match *segment {
            PathSegment::Move(x, y) => {
                start = (x, y);
                current = start;
            }
            PathSegment::Line(x, y) => {
                length += libm::hypotf(x - current.0, y - current.1);
                current = (x, y);
            }
            PathSegment::Cubic([first_x, first_y, second_x, second_y, end_x, end_y]) => {
                let from = current;
                let at = |along: f32| {
                    let rest = 1.0 - along;
                    let weights = [
                        rest * rest * rest,
                        3.0 * rest * rest * along,
                        3.0 * rest * along * along,
                        along * along * along,
                    ];
                    (
                        weights[0] * from.0
                            + weights[1] * first_x
                            + weights[2] * second_x
                            + weights[3] * end_x,
                        weights[0] * from.1
                            + weights[1] * first_y
                            + weights[2] * second_y
                            + weights[3] * end_y,
                    )
                };
                // Half a pixel, or less precise for curves of tens of thousands of pixels: a
                // corner of a hundred million pixels was cut into 65,536 pieces on each page.
                let extent = [first_x, second_x, end_x]
                    .iter()
                    .map(|x| (x - from.0).abs())
                    .chain([first_y, second_y, end_y].iter().map(|y| (y - from.1).abs()))
                    .fold(0.0_f32, f32::max);
                let tolerance = (extent / 65536.0).max(0.5);
                length += pieces(&at, (0.0, from), (1.0, (end_x, end_y)), (0, tolerance));
                current = (end_x, end_y);
            }
            PathSegment::Close => {
                length += libm::hypotf(start.0 - current.0, start.1 - current.1);
                current = start;
            }
        }
    }
    length
}

fn clip_path(rect: RectF, radii: Option<&Radii>) -> Option<Path> {
    // A clip beyond a billion pixels clips nothing on the page; NaN leaves nothing visible.
    let clamp = |value: f32| value.clamp(-1e9, 1e9);
    let rect = RectF {
        x: clamp(rect.x),
        y: clamp(rect.y),
        width: clamp(rect.width),
        height: clamp(rect.height),
    };
    let radii = radii.filter(|radii| radii.iter().all(|(x, y)| x.is_finite() && y.is_finite()));
    match radii {
        Some(radii) => rounded_rect(rect, radii),
        None => rect_path(rect),
    }
}

/// The outline entries from `start` on that nest below `level`, of bookmarks on `pages`.
fn outline_nodes(
    bookmarks: &[Bookmark],
    pages: &[PageList],
    start: &mut usize,
    level: u8,
) -> Vec<krilla::outline::OutlineNode> {
    let mut nodes = Vec::new();
    while let Some(bookmark) = bookmarks.get(*start) {
        if bookmark.level <= level {
            break;
        }
        *start += 1;
        let (x, y) = on_sheet(&pages[bookmark.page], (bookmark.x, bookmark.y));
        let destination = krilla::destination::XyzDestination::new(bookmark.page, Point::from_xy(x, y));
        let mut node = krilla::outline::OutlineNode::new(bookmark.title.clone(), destination);
        for child in outline_nodes(bookmarks, pages, start, bookmark.level) {
            node.push_child(child);
        }
        nodes.push(node.with_open(bookmark.open));
    }
    nodes
}

fn fill(color: Rgba) -> Fill {
    Fill {
        paint: rgb::Color::new(channel(color.r), channel(color.g), channel(color.b)).into(),
        opacity: NormalizedF32::new(color.a.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE),
        rule: FillRule::NonZero,
    }
}

fn channel(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// An image prepared for the PDF.
#[derive(Clone)]
enum Embedded {
    /// Pixels, and the data they come from.
    Raster(krilla::image::Image, Arc<Vec<u8>>),
    Svg(Arc<usvg::Tree>, svg::Fit),
    /// A page of a PDF file (its index, from 0), drawn as a form `XObject`.
    Pdf(krilla::pdf::PdfDocument, usize),
}

/// Images prepared for the PDF, by the address of their data: each is embedded once.
/// An inline SVG parsed to show what it draws beyond its viewport, and how far that reaches (see
/// [`unclipped`]).
type Unclipped = (Arc<usvg::Tree>, [f32; 4]);

struct ImageCache {
    /// Whether viewers may smooth the images of blurred shadows; PDF/A does not allow it.
    /// Without it, the PDF is a PDF/A, which also rules out PostScript functions.
    interpolate: bool,
    /// Whether the PDF must do without transparency, as PDF/A-1 requires: what is hidden is
    /// clipped away rather than drawn transparent, isolated groups are plain ones, and images
    /// whose pixels are all opaque lose their alpha channel (see [`decode::flat`]).
    opaque: bool,
    /// The quality of the JPEGs that opaque raster images are written as (see [`lossy`]);
    /// without one, their pixels are compressed without loss.
    quality: Option<u8>,
    /// The images embedded so far, by their data, whether they are smoothed, and what their
    /// pixels go through (their filters, written out, and their padding).
    images: FxHashMap<(usize, bool, String), Option<Embedded>>,
    /// The data of BMP, TIFF and AVIF images that did not decode: Fagus decodes them itself, so krilla
    /// does not find them broken as it writes the PDF.
    undecoded: Vec<Arc<Vec<u8>>>,
    /// SVG images with non-scaling strokes drawn at another width than their natural one, by
    /// their data and that width (its bits), parsed for it (see [`svg::parse_drawn`]).
    strokes: FxHashMap<(usize, u32), Option<Arc<usvg::Tree>>>,
    /// Inline SVGs parsed for the size of the box they are drawn in, by their data and that
    /// size (its bits; see [`svg::parse_sized`]).
    sized: FxHashMap<(usize, u32, u32), Option<Arc<usvg::Tree>>>,
    /// Inline SVGs whose drawing reaches beyond their viewport, parsed to show it all (see
    /// [`svg::parse_unclipped`]), and how far it reaches (top, right, bottom, left, in the
    /// SVG's units), by their data and the size of their box (its bits).
    unclipped: FxHashMap<(usize, u32, u32), Option<Unclipped>>,
    /// Images prepared for this and earlier documents.
    memo: Images,
    /// The images of blurred box shadows, by the shadow (written out): a shadow on every page,
    /// as that of a fixed box, is blurred once.
    shadows: FxHashMap<String, Option<krilla::image::Image>>,
    /// The images of blurred text shadows and where they go, by the shadow (written out).
    text_shadows: FxHashMap<String, Option<(krilla::image::Image, RectF)>>,
    /// SVG images with their `feMorphology` filters as they are drawn at a scale, by their data
    /// and that scale (its bits; see [`drawn_morphology`]).
    morphology: FxHashMap<(usize, u32), Option<Arc<Vec<u8>>>>,
    /// SVGs with filters drawn so far, by their tree, size and scale (the bits of the width and
    /// height and of the linear part of the transform): drawn as a graphic from the second time
    /// on (see [`draw_svg`]).
    svgs: FxHashMap<(usize, [u32; 6]), Option<krilla::graphic::Graphic>>,
    /// Whether SVG images have an empty `viewBox`, a size relative to their box, no size of
    /// their own and non-scaling strokes, and the background color of their root, by their data.
    svg_traits: FxHashMap<usize, (bool, bool, bool, bool, Option<Rgba>)>,
    /// The layers (`-fagus-layer`) drawn so far.
    layers: layers::Layers,
    /// The PDF files shown as images, opened, by their data: the pages of one file come from one
    /// document, whose objects krilla writes once.
    pdfs: FxHashMap<usize, Option<OpenedPdf>>,
}

/// A PDF file shown as an image, read, and as krilla embeds its pages.
type OpenedPdf = (Arc<krilla::pdf::Pdf>, krilla::pdf::PdfDocument);

fn embedded_image(image: &fagus_layout::Image, fonts: &FontDb, cache: &mut ImageCache) -> Option<Embedded> {
    // Chrome writes images without `/Interpolate` (leaving smoothing to viewers), whatever
    // `image-rendering` says and however they are scaled (Chrome 157, measured).
    let interpolate = false;
    // The pixels of a PDF page cannot go through filters; a part of one (of a border image) is
    // clipped as it is drawn.
    let filtered =
        !matches!(image.format, ImageFormat::Pdf(_)) && (!image.filters.is_empty() || image.pad > 0);
    let filters = if let ImageFormat::Pdf(page) = image.format {
        format!("page {page}")
    } else if filtered {
        format!("{:?} {}", image.filters, image.pad)
    } else {
        String::new()
    };
    cache
        .images
        .entry((Arc::as_ptr(&image.data) as usize, interpolate, filters))
        .or_insert_with(|| {
            // The render ends after this page (see `attempt`): what is drawn is of no use.
            if fagus_dom::budget::spent() {
                return None;
            }
            fagus_dom::budget::check_time_now();
            if let ImageFormat::Pdf(page) = image.format {
                return embedded_pdf(&image.data, page, &mut cache.pdfs);
            }
            // Decoded, an image takes four bytes a pixel (an SVG image's pixels are charged where
            // they are drawn). krilla decodes PNG, JPEG, GIF and WebP as it writes the PDF, one
            // image after another, so they only need room; the pixels that Fagus decodes itself
            // (BMP, TIFF, filtered images, PDF/A-1) are held until the PDF is written.
            if image.format != ImageFormat::Svg {
                let pad = 2.0 * f64::from(image.pad);
                let bytes = ((f64::from(image.width) + pad) * (f64::from(image.height) + pad) * 4.0) as u64;
                if filtered || cache.opaque || matches!(image.format, ImageFormat::Bmp | ImageFormat::Tiff) {
                    fagus_dom::budget::charge(fagus_dom::budget::Kind::Images, bytes);
                } else {
                    fagus_dom::budget::ensure_room(fagus_dom::budget::Kind::Images, bytes);
                }
            }
            if filtered {
                // A shadow of an image that cannot be decoded is none; other filters leave it
                // as it is. An SVG image is drawn as pixels to be filtered.
                let silhouette = image
                    .filters
                    .iter()
                    .any(|filter| matches!(filter, fagus_layout::PixelFilter::Silhouette(_)));
                let drawn = if image.format == ImageFormat::Svg {
                    svg::parse_kept(&image.data, fonts, &cache.memo, true, image.inline).and_then(|tree| {
                        svg::pixels(
                            &tree,
                            svg::fit(&image.data, image.inline),
                            image.width,
                            image.height,
                        )
                    })
                } else {
                    None
                };
                let decodable = image.format != ImageFormat::Svg || drawn.is_some();
                match decodable
                    .then(|| decode::filtered(image, interpolate, drawn, cache.quality))
                    .flatten()
                {
                    Some(filtered) => return Some(Embedded::Raster(filtered, Arc::clone(&image.data))),
                    None if silhouette => return None,
                    None => {}
                }
            }
            // PDF/A-1: an image whose pixels are all opaque goes without its alpha channel (one
            // with transparent pixels keeps it, and is refused), and 16 bits become 8. With an
            // image quality, it is a JPEG where its pixels allow one (see `lossy`).
            let flattened = cache.opaque
                && matches!(
                    image.format,
                    ImageFormat::Png | ImageFormat::Gif | ImageFormat::Webp
                );
            if flattened {
                let kind = match cache.quality {
                    Some(quality) => memo::Kind::Lossy {
                        format: image.format,
                        quality,
                        interpolate,
                        flat: true,
                    },
                    None => memo::Kind::Flat(image.format),
                };
                let quality = cache.quality;
                let flat = cache.memo.get_or_prepare(&image.data, kind, None, || {
                    memo::Prepared::Raster(
                        quality
                            .and_then(|quality| lossy::image(&image.data, image.format, quality, interpolate))
                            .or_else(|| decode::flat(&image.data, image.format)),
                    )
                });
                if let memo::Prepared::Raster(Some(raster)) = flat {
                    return Some(Embedded::Raster(raster, Arc::clone(&image.data)));
                }
            }
            let lossless = match image.format {
                ImageFormat::Png => memo::Kind::Png(interpolate),
                ImageFormat::Jpeg => memo::Kind::Jpeg(interpolate),
                ImageFormat::Gif => memo::Kind::Gif(interpolate),
                ImageFormat::Webp => memo::Kind::Webp(interpolate),
                ImageFormat::Bmp | ImageFormat::Tiff | ImageFormat::Avif => {
                    memo::Kind::Decoded(image.format, interpolate)
                }
                ImageFormat::Svg => {
                    return svg::parse_kept(&image.data, fonts, &cache.memo, true, image.inline)
                        .map(|tree| Embedded::Svg(tree, svg::fit(&image.data, image.inline)));
                }
                // Embedded above.
                ImageFormat::Pdf(_) => return None,
            };
            // With an image quality, an image whose pixels are all opaque is written as a JPEG;
            // the others, and those that are smaller as they are, are prepared as without one
            // (as is an image PDF/A-1 flattens, tried as a JPEG above).
            let quality = cache.quality.filter(|_| !flattened);
            let kind = match quality {
                Some(quality) => memo::Kind::Lossy {
                    format: image.format,
                    quality,
                    interpolate,
                    flat: false,
                },
                None => lossless,
            };
            // Decoding and compressing the pixels is most of the work: they are kept for the
            // next document.
            let prepared = cache.memo.get_or_prepare(&image.data, kind, None, || {
                let data: Arc<Vec<u8>> = Arc::clone(&image.data);
                if let Some(jpeg) =
                    quality.and_then(|quality| lossy::image(&data, image.format, quality, interpolate))
                {
                    return memo::Prepared::Raster(Some(jpeg));
                }
                let raster = match lossless {
                    memo::Kind::Png(_) => match decode::gamma_png(&data, interpolate) {
                        Some(corrected) => Some(corrected),
                        None => krilla::image::Image::from_png(data.into(), interpolate).ok(),
                    },
                    memo::Kind::Jpeg(_) => match decode::cmyk_jpeg(&data, interpolate) {
                        Some(converted) => Some(converted),
                        None => krilla::image::Image::from_jpeg(data.into(), interpolate).ok(),
                    },
                    memo::Kind::Gif(_) => krilla::image::Image::from_gif(data.into(), interpolate).ok(),
                    memo::Kind::Decoded(format, _) => decode::decoded(&data, format, interpolate),
                    memo::Kind::Webp(_)
                    | memo::Kind::Flat(_)
                    | memo::Kind::Lossy { .. }
                    | memo::Kind::Svg { .. } => {
                        krilla::image::Image::from_webp(data.into(), interpolate).ok()
                    }
                };
                memo::Prepared::Raster(raster)
            });
            match prepared {
                memo::Prepared::Raster(raster) => {
                    if raster.is_none() && matches!(lossless, memo::Kind::Decoded(..)) {
                        cache.undecoded.push(Arc::clone(&image.data));
                    }
                    raster.map(|raster| Embedded::Raster(raster, Arc::clone(&image.data)))
                }
                memo::Prepared::Svg(_) => None,
            }
        })
        .clone()
}

/// The page `page` of the PDF file `data` as krilla embeds it, the file opened once in `pdfs`.
/// The file and the content of the page, which krilla decodes and compresses again, are
/// charged to the budget until the PDF is written.
fn embedded_pdf(
    data: &Arc<Vec<u8>>,
    page: u32,
    pdfs: &mut FxHashMap<usize, Option<OpenedPdf>>,
) -> Option<Embedded> {
    let (pdf, document) = pdfs
        .entry(Arc::as_ptr(data) as usize)
        .or_insert_with(|| {
            let pdf = Arc::new(pdf_image::open(data).ok()?);
            fagus_dom::budget::charge(fagus_dom::budget::Kind::Images, data.len() as u64);
            Some((Arc::clone(&pdf), krilla::pdf::PdfDocument::new(pdf)))
        })
        .clone()?;
    // Measured again (a Flate stream is decoded without being kept), as krilla will decode it.
    let content = pdf_image::content_size(pdf.pages().get(page as usize)?.raw()).ok()?;
    fagus_dom::budget::charge(fagus_dom::budget::Kind::Images, content);
    Some(Embedded::Pdf(document, page as usize))
}

/// Draws the page `page` of the PDF `document` (image `image`) into `rect`, at the origin of the
/// surface. A shadow of it (`drop-shadow()`) is not drawn; other filters leave it as it is.
fn draw_pdf_page(
    surface: &mut Surface<'_>,
    rect: RectF,
    image: &fagus_layout::Image,
    document: &krilla::pdf::PdfDocument,
    page: usize,
) {
    use fagus_layout::PixelFilter;
    if image
        .filters
        .iter()
        .any(|filter| matches!(filter, PixelFilter::Silhouette(_)))
    {
        return;
    }
    // A part of the page (a slice of a border image) is the page drawn larger, clipped to the
    // part.
    let part = image.filters.iter().find_map(|filter| match filter {
        PixelFilter::Part(part) => Some(*part),
        _ => None,
    });
    let Some([left, top, right, bottom]) = part else {
        if let Some(size) = Size::from_wh(rect.width, rect.height) {
            surface.draw_pdf_page(document, size, page);
        }
        return;
    };
    let (width, height) = (rect.width / (right - left), rect.height / (bottom - top));
    let (Some(size), Some(clip)) = (
        Size::from_wh(width, height).filter(|_| width.is_finite() && height.is_finite()),
        clip_path(
            RectF {
                x: 0.0,
                y: 0.0,
                ..rect
            },
            None,
        ),
    ) else {
        return;
    };
    surface.push_clip_path(&clip, &FillRule::NonZero);
    surface.push_transform(&Transform::from_translate(-left * width, -top * height));
    surface.draw_pdf_page(document, size, page);
    surface.pop();
    surface.pop();
}

/// The data of SVG image `image` with its `feMorphology` filters as they are drawn in `rect`
/// (see [`svg::drawn_morphology`]), where that differs from the image; kept in `cache` by the
/// image and the scale it is drawn at.
fn drawn_morphology(
    image: &fagus_layout::Image,
    tree: &usvg::Tree,
    fit: svg::Fit,
    rect: RectF,
    cache: &mut ImageCache,
) -> Option<Arc<Vec<u8>>> {
    let natural = (tree.size().width(), tree.size().height());
    let (_, _, width, _) = fit.place(natural, rect.width, rect.height);
    let scale = width / natural.0;
    cache
        .morphology
        .entry((Arc::as_ptr(&image.data) as usize, scale.to_bits()))
        .or_insert_with(|| {
            if !scale.is_finite() || !svg::has_morphology(&image.data) {
                return None;
            }
            let (reach, factor) = svg::morphology_reach(tree, scale);
            if reach <= svg::SEPARATE_MORPHOLOGY_RADIUS || !factor.is_finite() || factor <= 0.0 {
                return None;
            }
            match svg::drawn_morphology(&image.data, factor) {
                std::borrow::Cow::Owned(drawn) => {
                    let drawn = Arc::new(drawn);
                    if let Some(faces) = cache.memo.svg_faces(&image.data) {
                        cache.memo.add_svg_faces(&drawn, faces.to_vec());
                    }
                    Some(drawn)
                }
                std::borrow::Cow::Borrowed(_) => None,
            }
        })
        .clone()
}

fn draw_image(
    surface: &mut Surface<'_>,
    rect: RectF,
    image: &fagus_layout::Image,
    fonts: &FontDb,
    cache: &mut ImageCache,
) {
    draw_image_as(surface, rect, image, fonts, cache, true);
}

/// The size of an SVG, in CSS pixels, from which its filters are drawn with fewer pixels (see
/// [`draw_svg`]).
const HUGE_SVG: f32 = 100_000.0;

/// Draws an SVG `size` large. One with filters, which krilla-svg draws as pixels (all of the
/// region of each filter, wherever the page cuts it), is drawn once and shown again where it is
/// drawn again at the same size and scale, as an SVG broken across pages is on each.
fn draw_svg(surface: &mut Surface<'_>, tree: &Arc<usvg::Tree>, size: Size, cache: &mut ImageCache) {
    use krilla::graphic::Graphic;
    use krilla_svg::SurfaceExt;
    let mut settings = krilla_svg::SvgSettings::default();
    let ctm = surface.ctm();
    // The filters of an SVG drawn larger than a hundred thousand pixels, of which a page shows
    // a sliver, are drawn as at most a thousand pixels across rather than krilla-svg's five
    // thousand: their pixels are far larger than those of a page either way, and filters take
    // time with each (`feTurbulence` a minute for five thousand).
    let scale = libm::hypotf(ctm.sx(), ctm.ky()).max(libm::hypotf(ctm.kx(), ctm.sy()));
    let side = size.width().max(size.height());
    if side > HUGE_SVG && scale > 0.0 {
        settings.filter_scale = settings.filter_scale.min(1000.0 / (side * scale));
    }
    // Without the translation, which differs from page to page: the pixels follow the scale.
    let linear = Transform::from_row(ctm.sx(), ctm.ky(), ctm.kx(), ctm.sy(), 0.0, 0.0);
    let Some(back) = linear.invert().filter(|_| !tree.filters().is_empty()) else {
        surface.draw_svg(tree, size, settings);
        return;
    };
    let key = (
        Arc::as_ptr(tree) as usize,
        [
            size.width(),
            size.height(),
            ctm.sx(),
            ctm.ky(),
            ctm.kx(),
            ctm.sy(),
        ]
        .map(f32::to_bits),
    );
    let graphic = match cache.svgs.get(&key) {
        // Drawn once: as it is the first time.
        None => {
            cache.svgs.insert(key, None);
            surface.draw_svg(tree, size, settings);
            return;
        }
        Some(Some(graphic)) => graphic.clone(),
        Some(None) => {
            let mut builder = surface.stream_builder();
            let mut inner = builder.surface();
            inner.push_transform(&linear);
            inner.draw_svg(tree, size, settings);
            inner.pop();
            inner.finish();
            let graphic = Graphic::new(builder.finish(), false);
            cache.svgs.insert(key, Some(graphic.clone()));
            graphic
        }
    };
    surface.push_transform(&back);
    surface.draw_graphic(graphic);
    surface.pop();
}

/// The inline SVG `tree` (of `data`, parsed at the size of its box where `sized`) parsed again
/// to show what it draws beyond its viewport, and how far that reaches (top, right, bottom,
/// left, in its own units); `None` where it draws within its viewport.
fn unclipped(
    data: &Arc<Vec<u8>>,
    tree: &usvg::Tree,
    (width, height): (f32, f32),
    sized: bool,
    fonts: &FontDb,
    cache: &mut ImageCache,
) -> Option<Unclipped> {
    let size = tree.size();
    let drawn = tree.root().abs_layer_bounding_box();
    // A little room for the edges of what is drawn, which krilla clips at.
    let reach = |beyond: f32| if beyond > 1e-3 { beyond + 1.0 } else { 0.0 };
    let margins = [
        reach(-drawn.top()),
        reach(drawn.right() - size.width()),
        reach(drawn.bottom() - size.height()),
        reach(-drawn.left()),
    ];
    if margins.iter().all(|margin| *margin == 0.0) || margins.iter().any(|margin| !margin.is_finite()) {
        return None;
    }
    let key = (Arc::as_ptr(data) as usize, width.to_bits(), height.to_bits());
    cache
        .unclipped
        .entry(key)
        .or_insert_with(|| {
            let sized = sized.then_some((width, height));
            svg::parse_unclipped(data, fonts, sized, (size.width(), size.height()), margins)
                .map(|tree| (Arc::new(tree), margins))
        })
        .clone()
}

/// Draws an image; `morphology` lets an SVG be drawn with its `feMorphology` filters rewritten
/// (see [`drawn_morphology`]), which the rewritten one is not again.
fn draw_image_as(
    surface: &mut Surface<'_>,
    rect: RectF,
    image: &fagus_layout::Image,
    fonts: &FontDb,
    cache: &mut ImageCache,
    morphology: bool,
) {
    let Some(embedded) = embedded_image(image, fonts, cache) else {
        return;
    };
    let Some(size) = Size::from_wh(rect.width, rect.height) else {
        return;
    };
    // An SVG with `feMorphology` filters that reach far where it is drawn is drawn with them
    // as Chrome limits them, and faster.
    if morphology
        && let Embedded::Svg(tree, fit) = &embedded
        && let Some(drawn) = drawn_morphology(image, tree, *fit, rect, cache)
    {
        let drawn = fagus_layout::Image {
            data: drawn,
            ..image.clone()
        };
        draw_image_as(surface, rect, &drawn, fonts, cache, false);
        return;
    }
    surface.push_transform(&Transform::from_translate(rect.x, rect.y));
    match embedded {
        Embedded::Raster(raster, _) if image.orientation > 1 => {
            // The stored pixels, turned and flipped into the box as their EXIF orientation says.
            let (width, height) = (rect.width, rect.height);
            let [sx, ky, kx, sy, tx, ty] = match image.orientation {
                2 => [-1.0, 0.0, 0.0, 1.0, width, 0.0],
                3 => [-1.0, 0.0, 0.0, -1.0, width, height],
                4 => [1.0, 0.0, 0.0, -1.0, 0.0, height],
                5 => [0.0, 1.0, 1.0, 0.0, 0.0, 0.0],
                6 => [0.0, 1.0, -1.0, 0.0, width, 0.0],
                7 => [0.0, -1.0, -1.0, 0.0, width, height],
                _ => [0.0, -1.0, 1.0, 0.0, 0.0, height],
            };
            let stored = if image.orientation >= 5 {
                Size::from_wh(height, width)
            } else {
                Some(size)
            };
            if let Some(stored) = stored {
                surface.push_transform(&Transform::from_row(sx, ky, kx, sy, tx, ty));
                surface.draw_image(raster, stored);
                surface.pop();
            }
        }
        Embedded::Raster(raster, _) => surface.draw_image(raster, size),
        Embedded::Pdf(document, page) => draw_pdf_page(surface, rect, image, &document, page),
        Embedded::Svg(tree, fit) => {
            // An inline SVG is laid out at the size of its box, as Chrome lays out the root of
            // one: one as large as a percentage of the box, which the percentages in it are of,
            // and one whose `viewBox` keeps its aspect ratio in a box of another than that of
            // its `width` and `height` (KaTeX's stretchy delimiters) are parsed at that size.
            // So is an SVG image with a `viewBox` in such a box, whose viewport is the box in
            // browsers (one with a height and a `viewBox` only, drawn as wide as the ratio
            // makes it).
            let natural = (tree.size().width(), tree.size().height());
            let reshaped = matches!(fit, svg::Fit::Uniform { .. })
                && natural.0 > 0.0
                && natural.1 > 0.0
                && rect.height > 0.0
                && (natural.0 / natural.1 - rect.width / rect.height).abs()
                    > 1e-3 * (rect.width / rect.height);
            // What the drawing needs to know of the document, found once for it rather than on
            // each page it is drawn on.
            let (empty, relative, fills_box, non_scaling, background) = *cache
                .svg_traits
                .entry(Arc::as_ptr(&image.data) as usize)
                .or_insert_with(|| {
                    (
                        svg::has_empty_view_box(&image.data),
                        svg::has_relative_size(&image.data),
                        svg::fills_its_box(&image.data),
                        svg::has_non_scaling_stroke(&image.data),
                        svg::root_background(&image.data),
                    )
                });
            // An empty `viewBox` disables rendering.
            if empty {
                surface.pop();
                return;
            }
            // Without a `viewBox` the viewport of an inline SVG is its box, whatever its own
            // `width` and `height` say (one sized by CSS only: `<svg class=icon><use href=…>`).
            let resized = image.inline
                && ((natural.0 - rect.width).abs() > 0.01 || (natural.1 - rect.height).abs() > 0.01);
            let relative = fit == svg::Fit::Unscaled && (resized || relative);
            // So is an SVG image without a size of its own or a `viewBox`.
            let fills = !image.inline && fit == svg::Fit::Stretch && fills_box;
            let tree = if reshaped || (image.inline && relative) || fills {
                cache
                    .sized
                    .entry((
                        Arc::as_ptr(&image.data) as usize,
                        rect.width.to_bits(),
                        rect.height.to_bits(),
                    ))
                    .or_insert_with(|| {
                        svg::parse_sized(&image.data, fonts, rect.width, rect.height).map(Arc::new)
                    })
                    .clone()
                    .unwrap_or(tree)
            } else {
                tree
            };
            let natural = (tree.size().width(), tree.size().height());
            let (x, y, width, height) = fit.place(natural, rect.width, rect.height);
            // Non-scaling strokes are as wide as they say at the size the SVG is drawn at.
            let tree = if non_scaling
                && (svg::natural_size(&image.data).0.unwrap_or(natural.0) - width).abs() > 0.01
            {
                cache
                    .strokes
                    .entry((Arc::as_ptr(&image.data) as usize, width.to_bits()))
                    .or_insert_with(|| svg::parse_drawn(&image.data, fonts, width).map(Arc::new))
                    .clone()
                    .unwrap_or(tree)
            } else {
                tree
            };
            // An inline SVG is not cut off at its viewport: what it draws beyond is clipped as
            // its `overflow` says, which painting does (Blink's `SVGRootPainter`).
            let shown = (image.inline && !non_scaling)
                .then(|| {
                    unclipped(
                        &image.data,
                        &tree,
                        (rect.width, rect.height),
                        reshaped || relative,
                        fonts,
                        cache,
                    )
                })
                .flatten();
            let unclipped = shown.is_some();
            let (tree, x, y, width, height) = shown.map_or(
                (tree, x, y, width, height),
                |(tree, [top, right, bottom, left])| {
                    let (sx, sy) = (width / natural.0.max(1e-6), height / natural.1.max(1e-6));
                    (
                        tree,
                        x - left * sx,
                        y - top * sy,
                        width + (left + right) * sx,
                        height + (top + bottom) * sy,
                    )
                },
            );
            // What a `slice` or an unscaled SVG puts outside the box is clipped.
            let box_rect = RectF {
                x: 0.0,
                y: 0.0,
                ..rect
            };
            let clip = if !unclipped
                && (x < 0.0 || y < 0.0 || x + width > rect.width + 0.01 || y + height > rect.height + 0.01)
            {
                clip_path(box_rect, None)
            } else {
                None
            };
            // The root's background fills the whole box of an SVG image, under its drawing.
            if let Some(color) = background.filter(|_| !image.inline)
                && let Some(path) = rect_path(box_rect)
            {
                surface.set_fill(Some(fill(color)));
                surface.set_stroke(None);
                surface.draw_path(&path);
            }
            let clipped = clip.is_some();
            if let Some(clip) = clip {
                surface.push_clip_path(&clip, &FillRule::NonZero);
            }
            if let Some(size) = Size::from_wh(width, height) {
                surface.push_transform(&Transform::from_translate(x, y));
                draw_svg(surface, &tree, size, cache);
                surface.pop();
            }
            if clipped {
                surface.pop();
            }
        }
    }
    surface.pop();
}

/// The room around the bleed of a page for its printer's marks, in pixels (half an inch).
const MARKS_ROOM: f32 = 48.0;

/// How far a page lies inside its sheet: its bleed and the room for its marks, in pixels.
fn sheet_margin(page: &PageList) -> f32 {
    page.bleed
        + if page.marks.crop || page.marks.cross {
            MARKS_ROOM
        } else {
            0.0
        }
}

/// The size of a page's sheet, in pixels: turned with the page.
fn sheet_size(page: &PageList) -> (f32, f32) {
    let around = sheet_margin(page);
    let (width, height) = (page.width + 2.0 * around, page.height + 2.0 * around);
    match page.orientation {
        PageOrientation::Upright => (width, height),
        PageOrientation::RotateLeft | PageOrientation::RotateRight => (height, width),
    }
}

/// The size of a page's sheet in PDF points, its media box. A sheet without bleed and marks is
/// as large as Chrome prints it (which ignores bleed and marks): each side rounded to whole
/// points, then to the 1/300 inch of Chrome's PDF canvas, so that 80mm (226.77pt) becomes
/// 227.04pt and A4 594.96pt by 841.92pt. The content stays where it is from the top left.
fn media_size(page: &PageList) -> (f32, f32) {
    let (width, height) = sheet_size(page);
    if sheet_margin(page) > 0.0 {
        ((width * SCALE).max(1.0), (height * SCALE).max(1.0))
    } else {
        // A page too small for a whole point is not printed by Chrome; it has a side of 1/300
        // inch here.
        let side = |px: f32| chrome_sheet_side(px).max(0.24);
        (side(width), side(height))
    }
}

/// A side of a sheet of `px` pixels as Chrome prints it, in PDF points: rounded to whole points,
/// then to 1/300 inch. The points are multiplied by 300/72 in single precision, so that a side
/// half way between two steps goes up or down as in Chrome (225pt to 224.88pt, 567pt to
/// 567.12pt), and the dots by 0.24 in single precision, the very number Chrome writes (a
/// side of 210mm, 2479 dots, is 594.95996pt, not 594.96pt), so that edges at the same
/// place on the page fall where Chrome's do when the page is rasterized.
fn chrome_sheet_side(px: f32) -> f32 {
    const DOTS_PER_POINT: f32 = 300.0 / 72.0;
    const POINTS_PER_DOT: f32 = 72.0 / 300.0;
    ((px * SCALE).round() * DOTS_PER_POINT).round() * POINTS_PER_DOT
}

/// The size of a page's sheet, upright, by which it is turned, in pixels: without bleed and
/// marks, each side rounded to whole points, as Chrome turns pages (an 80mm side, 302.36 pixels,
/// turns by 227pt).
fn turned_size(page: &PageList) -> (f32, f32) {
    let around = sheet_margin(page);
    let (width, height) = (page.width + 2.0 * around, page.height + 2.0 * around);
    if around > 0.0 {
        (width, height)
    } else {
        let whole = |px: f32| (px * SCALE).round() / SCALE;
        (whole(width), whole(height))
    }
}

/// How a page's sheet, upright, is turned onto the sheet as it is printed.
fn turn(page: &PageList) -> Transform {
    let (width, height) = turned_size(page);
    match page.orientation {
        PageOrientation::Upright => Transform::identity(),
        // The top left corner goes to the bottom left, as Chrome turns it.
        PageOrientation::RotateLeft => Transform::from_row(0.0, -1.0, 1.0, 0.0, 0.0, width),
        PageOrientation::RotateRight => Transform::from_row(0.0, 1.0, -1.0, 0.0, height, 0.0),
    }
}

/// Where a point of a page lands on its sheet, in PDF units.
fn on_sheet(page: &PageList, (x, y): (f32, f32)) -> (f32, f32) {
    let around = sheet_margin(page);
    let (x, y) = (x + around, y + around);
    let (width, height) = turned_size(page);
    let (x, y) = match page.orientation {
        PageOrientation::Upright => (x, y),
        PageOrientation::RotateLeft => (y, width - x),
        PageOrientation::RotateRight => (height - y, x),
    };
    (x * SCALE, y * SCALE)
}

/// Draws the printer's marks of a page whose trim box lies `around` pixels inside the sheet:
/// crop marks at its corners, outside its bleed, and registration crosses at the middle of its
/// sides, hairlines in black, as artifacts of the page.
fn draw_marks(surface: &mut Surface<'_>, page: &PageList, around: f32, tagged: bool) {
    if tagged {
        surface.start_tagged(krilla::tagging::ContentTag::Artifact(
            krilla::tagging::Artifact::with_kind(krilla::tagging::ArtifactType::Page),
        ));
    }
    let (width, height) = (page.width, page.height);
    // From a little outside the bleed to the edge of the sheet.
    let gap = page.bleed + 4.0;
    let length = around - gap;
    let mut path = PathBuilder::new();
    let mut line = |from: (f32, f32), to: (f32, f32)| {
        path.move_to(from.0, from.1);
        path.line_to(to.0, to.1);
    };
    if page.marks.crop && length > 0.0 {
        for (x, outward_x) in [(around, -1.0), (around + width, 1.0)] {
            for (y, outward_y) in [(around, -1.0), (around + height, 1.0)] {
                line((x + outward_x * gap, y), (x + outward_x * (gap + length), y));
                line((x, y + outward_y * gap), (x, y + outward_y * (gap + length)));
            }
        }
    }
    if page.marks.cross {
        // A cross with a circle, halfway between the bleed and the edge of the sheet.
        let middle = gap + length / 2.0;
        let radius = (length / 4.0).min(8.0);
        let centers = [
            (around + width / 2.0, around - middle),
            (around + width / 2.0, around + height + middle),
            (around - middle, around + height / 2.0),
            (around + width + middle, around + height / 2.0),
        ];
        for (x, y) in centers {
            line((x - 1.5 * radius, y), (x + 1.5 * radius, y));
            line((x, y - 1.5 * radius), (x, y + 1.5 * radius));
        }
        let mut circles = PathBuilder::new();
        // Four cubic quarters, their control points 0.5523 of the radius out.
        let control = radius * 0.552_284_8;
        for (x, y) in centers {
            circles.move_to(x + radius, y);
            circles.cubic_to(x + radius, y + control, x + control, y + radius, x, y + radius);
            circles.cubic_to(x - control, y + radius, x - radius, y + control, x - radius, y);
            circles.cubic_to(x - radius, y - control, x - control, y - radius, x, y - radius);
            circles.cubic_to(x + control, y - radius, x + radius, y - control, x + radius, y);
            circles.close();
        }
        if let Some(circles) = circles.finish() {
            surface.set_fill(None);
            surface.set_stroke(Some(hairline()));
            surface.draw_path(&circles);
        }
    }
    if let Some(path) = path.finish() {
        surface.set_fill(None);
        surface.set_stroke(Some(hairline()));
        surface.draw_path(&path);
    }
    surface.set_stroke(None);
    if tagged {
        surface.end_tagged();
    }
}

/// A black hairline of a quarter point.
fn hairline() -> Stroke {
    Stroke {
        paint: rgb::Color::new(0, 0, 0).into(),
        width: 0.25 / SCALE,
        ..Stroke::default()
    }
}

/// The identity transform, `[a, b, c, d, e, f]`.
const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `outer` applied after `inner`, if the result can be drawn: finite, scaling by at most a
/// million and moving by at most a billion pixels. Beyond that, content is far off the page, and
/// krilla's arithmetic would overflow.
fn compose(outer: [f32; 6], inner: [f32; 6]) -> Option<[f32; 6]> {
    // Each maps (x, y) to (m[0]·x + m[2]·y + m[4], m[1]·x + m[3]·y + m[5]).
    let (m, n) = (outer, inner);
    let composed = [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ];
    let drawable = composed.iter().all(|value| value.is_finite())
        && composed[..4].iter().all(|value| value.abs() <= 1e6)
        && composed[4..].iter().all(|value| value.abs() <= 1e9);
    drawable.then_some(composed)
}

/// Whether all coordinates of an item are finite and within a billion pixels. CSS values can
/// overflow to infinity (`box-shadow: 1e305px …`), and krilla unwraps what it computes from
/// them; a panic while a page is drawn aborts the process, so such items are not drawn.
fn drawable(item: &Item) -> bool {
    let sane = |value: f32| value.is_finite() && value.abs() <= 1e9;
    let rect = |rect: &RectF| [rect.x, rect.y, rect.width, rect.height].into_iter().all(sane);
    let radii = |radii: &Radii| radii.iter().all(|(x, y)| sane(*x) && sane(*y));
    let segments = |segments: &[fagus_layout::PathSegment]| {
        segments.iter().all(|segment| match segment {
            fagus_layout::PathSegment::Move(x, y) | fagus_layout::PathSegment::Line(x, y) => {
                sane(*x) && sane(*y)
            }
            fagus_layout::PathSegment::Cubic(points) => points.iter().copied().all(sane),
            fagus_layout::PathSegment::Close => true,
        })
    };
    match item {
        Item::Rect {
            rect: area,
            radii: corners,
            ..
        } => rect(area) && corners.as_ref().is_none_or(radii),
        Item::BorderSide { points, width, .. } => {
            sane(*width) && points.iter().all(|(x, y)| sane(*x) && sane(*y))
        }
        Item::RoundedBorder {
            outer,
            outer_radii,
            inner,
            inner_radii,
            ..
        } => rect(outer) && rect(inner) && radii(outer_radii) && radii(inner_radii),
        Item::DashedBorder {
            outer,
            outer_radii,
            inner,
            inner_radii,
            center,
            center_radii,
            width,
            stroke,
            centerline,
            ..
        } => {
            rect(outer)
                && rect(inner)
                && rect(center)
                && radii(outer_radii)
                && radii(inner_radii)
                && radii(center_radii)
                && centerline.as_deref().is_none_or(segments)
                && sane(*width)
                && sane(*stroke)
        }
        Item::Tiles {
            area,
            origin,
            step,
            tile,
        } => {
            rect(area)
                && sane(origin.0)
                && sane(origin.1)
                && step.0 > 0.0
                && step.1 > 0.0
                && sane(step.0)
                && sane(step.1)
                && drawable(tile)
        }
        Item::Text {
            x, y, size, glyphs, ..
        } => {
            sane(*x)
                && sane(*y)
                && sane(*size)
                && glyphs
                    .iter()
                    .all(|glyph| sane(glyph.advance) && sane(glyph.x_offset) && sane(glyph.y_offset))
        }
        Item::Wave {
            start,
            y,
            control,
            wavelength,
            thickness,
            clip,
            ..
        } => {
            [*start, *y, *control, *wavelength, *thickness]
                .into_iter()
                .all(sane)
                && rect(clip)
                && *wavelength > 0.0
                && (clip.x + clip.width - start) / wavelength < 100_000.0
        }
        Item::DashedLine {
            from,
            to,
            thickness,
            dash,
            ..
        } => [from.0, from.1, to.0, to.1, *thickness, *dash]
            .into_iter()
            .all(sane),
        Item::Image { rect: area, .. } => rect(area),
        Item::Filtered(paint) => rect(&paint.bounds),
        Item::Gradient { rect: area, gradient } => {
            rect(area)
                && match &**gradient {
                    GradientPaint::Solid(_) => true,
                    GradientPaint::Linear { start, end, .. } => {
                        [start.0, start.1, end.0, end.1].into_iter().all(sane)
                    }
                    GradientPaint::Radial {
                        center,
                        radius,
                        scale_y,
                        ..
                    } => [center.0, center.1, *radius, *scale_y].into_iter().all(sane),
                    GradientPaint::Conic {
                        center, start_angle, ..
                    } => [center.0, center.1, *start_angle].into_iter().all(sane),
                }
        }
        Item::BoxShadow(shadow) => {
            rect(&shadow.area)
                && rect(&shadow.shape)
                && sane(shadow.blur)
                && shadow.area_radii.as_ref().is_none_or(radii)
                && shadow.shape_radii.as_ref().is_none_or(radii)
                && shadow
                    .outlines
                    .as_ref()
                    .is_none_or(|outlines| segments(&outlines.area) && segments(&outlines.shape))
        }
        Item::TextShadow {
            x,
            y,
            size,
            glyphs,
            lines,
            blur,
            ..
        } => {
            sane(*x)
                && sane(*y)
                && sane(*size)
                && sane(*blur)
                && lines.iter().all(rect)
                && glyphs
                    .iter()
                    .all(|glyph| sane(glyph.advance) && sane(glyph.x_offset) && sane(glyph.y_offset))
        }
        Item::PushOpacity(_)
        | Item::PushBlend(_)
        | Item::PushIsolated
        | Item::PushLayer(_)
        | Item::PushMask(_)
        | Item::PushTransform(_)
        | Item::PushClip(..)
        | Item::Pop
        | Item::Tag(_) => true,
        Item::PushClipShape(ClipGeometry::Ellipse { center, radii }) => {
            [center.0, center.1, radii.0, radii.1].into_iter().all(sane)
        }
        Item::PushClipShape(ClipGeometry::Polygon { points, .. }) => {
            points.iter().all(|(x, y)| sane(*x) && sane(*y))
        }
        Item::PushClipShape(ClipGeometry::Path {
            segments: outline, ..
        }) => segments(outline),
    }
}

/// A face, or of a variable face the instance at coordinates (as bits).
type Instance = (FaceId, Vec<([u8; 4], u32)>);

/// A font of the PDF: a face as krilla embeds it, or one made of the outlines that Fagus draws
/// (see [`outline_font`]) with the numbers it gives the glyphs of the face.
#[derive(Clone)]
struct PdfFont {
    font: Font,
    glyphs: Option<Arc<FxHashMap<u32, u16>>>,
}

/// The most bytes of copies of color fonts with other palettes in a document (see
/// [`FontCache::get_with_palette`]).
const MAX_PALETTE_BYTES: usize = 256 << 20;

/// The fonts of a document, made on first use.
struct FontCache {
    fonts: FxHashMap<Instance, Option<PdfFont>>,
    /// The fonts of color faces with the colors of another palette, by instance and colors.
    palettes: FxHashMap<(Instance, Vec<fagus_text::PaletteColor>), Option<PdfFont>>,
    /// The bytes of the fonts of `palettes`, which [`MAX_PALETTE_BYTES`] bounds.
    palette_bytes: usize,
    /// The glyphs that the document's text uses of each instance of a face that fonts made of
    /// outlines stand in for.
    outlined: FxHashMap<Instance, std::collections::BTreeSet<u32>>,
    /// The fonts whose `.notdef` glyph has been given empty text (see [`seed_notdef`]).
    seeded: rustc_hash::FxHashSet<Font>,
    /// Whether `.notdef` glyphs are seeded: not in PDF/A and PDF/UA, which refuse them.
    seed_notdef: bool,
}

impl FontCache {
    /// The cache for the pages and the appearances of `fields`, which knows the glyphs they use
    /// of faces that need fonts made of outlines.
    fn new(pages: &[PageList], fields: &[Field], fonts: &FontDb) -> Self {
        fn collect(
            items: &[Item],
            fonts: &FontDb,
            outlined: &mut FxHashMap<Instance, std::collections::BTreeSet<u32>>,
        ) {
            for item in items {
                match item {
                    Item::Text {
                        face,
                        variations,
                        glyphs,
                        ..
                    } if fonts.face(*face).outlines == Outlines::Hvgl => {
                        outlined
                            .entry((*face, bits(variations)))
                            .or_default()
                            .extend(glyphs.iter().map(|glyph| glyph.id));
                    }
                    Item::Tiles { tile, .. } => collect(std::slice::from_ref(tile), fonts, outlined),
                    Item::PushMask(mask) => collect(&mask.items, fonts, outlined),
                    _ => {}
                }
            }
        }
        let mut outlined = FxHashMap::default();
        for page in pages {
            collect(&page.items, fonts, &mut outlined);
        }
        for widget in fields.iter().flat_map(|field| &field.widgets) {
            collect(&widget.appearance, fonts, &mut outlined);
            collect(&widget.off, fonts, &mut outlined);
        }
        Self {
            fonts: FxHashMap::default(),
            palettes: FxHashMap::default(),
            palette_bytes: 0,
            outlined,
            seeded: rustc_hash::FxHashSet::default(),
            seed_notdef: true,
        }
    }

    /// The font of a face at `variations` with the colors of `palette` in place of its first
    /// palette's (see [`fagus_text::with_palette`]), made once; the face's own font where the
    /// colors cannot be set.
    fn get_with_palette(
        &mut self,
        fonts: &FontDb,
        face: FaceId,
        variations: &[([u8; 4], f32)],
        palette: Option<&[fagus_text::PaletteColor]>,
    ) -> Option<PdfFont> {
        let Some(palette) = palette else {
            return self.get(fonts, face, variations);
        };
        let key = ((face, bits(variations)), palette.to_vec());
        if self.outlined.contains_key(&key.0) {
            return self.get(fonts, face, variations);
        }
        if let Some(font) = self.palettes.get(&key) {
            return font.clone();
        }
        let source = fonts.face(face);
        // Each palette is a copy of the font: past the budget, faces keep their own colors.
        if self.palette_bytes + source.data.len() > MAX_PALETTE_BYTES {
            return self.get(fonts, face, variations);
        }
        self.palette_bytes += source.data.len();
        let font = fagus_text::with_palette(&source.data, source.index, palette).and_then(|data| {
            let coordinates: Vec<(Tag, f32)> = variations
                .iter()
                .map(|(tag, value)| (Tag::new(tag), *value))
                .collect();
            Font::new_variable(data.into(), source.index, &coordinates)
                .map(|font| PdfFont { font, glyphs: None })
        });
        let font = match font {
            Some(font) => Some(font),
            None => self.get(fonts, face, variations),
        };
        self.palettes.insert(key, font.clone());
        font
    }

    /// The font of a face at `variations`, made once; `None` if krilla cannot read it.
    fn get(&mut self, fonts: &FontDb, face: FaceId, variations: &[([u8; 4], f32)]) -> Option<PdfFont> {
        let key = (face, bits(variations));
        if let Some(font) = self.fonts.get(&key) {
            return font.clone();
        }
        let face = fonts.face(face);
        let font = if let Some(glyphs) = self.outlined.get(&key) {
            outline_font::build(face, variations, glyphs).and_then(|built| {
                Some(PdfFont {
                    font: Font::new(built.data.into(), 0)?,
                    glyphs: Some(Arc::new(built.glyphs)),
                })
            })
        } else {
            let data: Arc<Vec<u8>> = Arc::clone(&face.data);
            let coordinates: Vec<(Tag, f32)> = variations
                .iter()
                .map(|(tag, value)| (Tag::new(tag), *value))
                .collect();
            Font::new_variable(data.into(), face.index, &coordinates)
                .map(|font| PdfFont { font, glyphs: None })
        };
        self.fonts.insert(key, font.clone());
        font
    }
}

/// Coordinates on variation axes, as bits that can be compared and hashed.
fn bits(variations: &[([u8; 4], f32)]) -> Vec<([u8; 4], u32)> {
    variations
        .iter()
        .map(|(tag, value)| (*tag, value.to_bits()))
        .collect()
}

/// Draws an item. `transforms` holds the transform in effect for every group pushed so far, or
/// `None` inside a transform that cannot be drawn. Nothing drawn outside `visible` (in the
/// pixels of the page, where it is known) shows.
fn draw(
    surface: &mut Surface<'_>,
    item: &Item,
    fonts: &FontDb,
    font_cache: &mut FontCache,
    image_cache: &mut ImageCache,
    transforms: &mut Vec<Option<[f32; 6]>>,
    visible: Option<RectF>,
) {
    let current = transforms.last().copied().flatten();
    if !drawable(item) {
        // A clip shape too large to draw clips nothing, but its group still opens: the `Pop`
        // that ends it must have something to end.
        if matches!(item, Item::PushClipShape(_)) {
            transforms.push(current);
            surface.push_opacity(NormalizedF32::ONE);
        }
        return;
    }
    // The end of a layer is marked before its group ends.
    let stream = std::ptr::from_ref(transforms) as usize;
    if matches!(item, Item::Pop) && image_cache.layers.ends_at((stream, transforms.len())) {
        layers::end(surface);
    }
    match item {
        Item::PushOpacity(_)
        | Item::PushBlend(_)
        | Item::PushIsolated
        | Item::PushLayer(_)
        | Item::PushMask(_)
        | Item::PushClip(..)
        | Item::PushClipShape(_) => {
            transforms.push(current);
        }
        Item::Pop => {
            transforms.pop();
        }
        _ => {}
    }
    match item {
        Item::Rect { rect, color, radii } => {
            let path = match radii {
                Some(radii) => rounded_rect(*rect, radii),
                None => rect_path(*rect),
            };
            if let Some(path) = path {
                surface.set_fill(Some(fill(*color)));
                surface.set_stroke(None);
                surface.draw_path(&path);
            }
        }
        Item::RoundedBorder {
            outer,
            outer_radii,
            inner,
            inner_radii,
            color,
            stroke: Some(width),
        } => {
            // Stroked along the middle between the two edges.
            let middle = RectF {
                x: f32::midpoint(outer.x, inner.x),
                y: f32::midpoint(outer.y, inner.y),
                width: f32::midpoint(outer.width, inner.width),
                height: f32::midpoint(outer.height, inner.height),
            };
            let radii: Radii = std::array::from_fn(|corner| {
                (
                    f32::midpoint(outer_radii[corner].0, inner_radii[corner].0),
                    f32::midpoint(outer_radii[corner].1, inner_radii[corner].1),
                )
            });
            let mut builder = PathBuilder::new();
            append_rounded_rect(&mut builder, middle, &radii);
            if let Some(path) = builder.finish() {
                surface.set_fill(None);
                surface.set_stroke(Some(dashed_stroke(*color, width.max(0.0), None)));
                surface.draw_path(&path);
                surface.set_stroke(None);
            }
        }
        Item::RoundedBorder {
            outer,
            outer_radii,
            inner,
            inner_radii,
            color,
            stroke: None,
        } => {
            let mut builder = PathBuilder::new();
            append_rounded_rect(&mut builder, *outer, outer_radii);
            append_rounded_rect(&mut builder, *inner, inner_radii);
            if let Some(path) = builder.finish() {
                surface.set_fill(Some(Fill {
                    rule: FillRule::EvenOdd,
                    ..fill(*color)
                }));
                surface.set_stroke(None);
                surface.draw_path(&path);
            }
        }
        Item::Tiles {
            area,
            origin,
            step,
            tile,
        } => {
            // The tile once, in a stream of its own, which the viewer repeats.
            let mut builder = surface.stream_builder();
            let mut inner = builder.surface();
            let mut inner_transforms = vec![current];
            // The viewer repeats the tile anywhere in the area: all of it may show.
            draw(
                &mut inner,
                tile,
                fonts,
                font_cache,
                image_cache,
                &mut inner_transforms,
                None,
            );
            inner.finish();
            let pattern = krilla::paint::Pattern {
                stream: builder.finish(),
                transform: Transform::from_translate(origin.0, origin.1),
                width: step.0,
                height: step.1,
            };
            if let Some(path) = rect_path(*area) {
                surface.set_fill(Some(Fill {
                    paint: pattern.into(),
                    opacity: NormalizedF32::ONE,
                    rule: FillRule::NonZero,
                }));
                surface.set_stroke(None);
                surface.draw_path(&path);
            }
        }
        Item::DashedBorder {
            outer,
            outer_radii,
            inner,
            inner_radii,
            center,
            center_radii,
            color,
            style,
            width,
            stroke,
            centerline: shaped,
        } => {
            let mut ring = PathBuilder::new();
            append_rounded_rect(&mut ring, *outer, outer_radii);
            if inner.width > 0.0 && inner.height > 0.0 {
                append_rounded_rect(&mut ring, *inner, inner_radii);
            }
            let middle = match shaped {
                Some(segments) => {
                    let mut builder = PathBuilder::new();
                    append_segments(&mut builder, segments);
                    (builder.finish(), segments_length(segments))
                }
                None => centerline(*center, center_radii),
            };
            let (Some(ring), (Some(path), length)) = (ring.finish(), middle) else {
                return;
            };
            // As Chrome strokes it: the pattern for the whole path, its length cut to pixels;
            // dashes and square dots across the whole border, cut to it, round dots of its width.
            // A solid one (`border-shape`) is one stroke.
            let pattern = if *style == BorderStyle::Solid {
                None
            } else {
                dash_pattern(*style, *width, length.trunc(), true)
            };
            let round = matches!(pattern, Some((_, true)));
            let stroke_width = if round { *width } else { stroke * 2.2 };
            surface.push_clip_path(&ring, &FillRule::EvenOdd);
            surface.set_fill(None);
            surface.set_stroke(Some(dashed_stroke(*color, stroke_width, pattern)));
            surface.draw_path(&path);
            surface.set_stroke(None);
            surface.pop();
        }
        Item::BorderSide {
            points,
            color,
            style,
            width,
        } => draw_border_side(surface, points, *color, *style, *width),
        Item::Wave {
            start,
            y,
            control,
            wavelength,
            thickness,
            clip,
            color,
        } => {
            // One cubic curve a wavelength, from where the wave starts before the clip on;
            // counted rather than added up.
            let first = start - ((start - clip.x) / wavelength).ceil().max(0.0) * wavelength;
            let count = ((clip.x + clip.width - first) / wavelength).ceil().max(1.0) as usize;
            let mut builder = PathBuilder::new();
            builder.move_to(first, *y);
            for index in 0..count {
                let from = first + index as f32 * wavelength;
                let middle = from + wavelength / 2.0;
                builder.cubic_to(middle, y + control, middle, y - control, from + wavelength, *y);
            }
            if let (Some(path), Some(clip)) = (builder.finish(), rect_path(*clip)) {
                surface.push_clip_path(&clip, &FillRule::NonZero);
                surface.set_fill(None);
                surface.set_stroke(Some(dashed_stroke(*color, *thickness, None)));
                surface.draw_path(&path);
                surface.set_stroke(None);
                surface.pop();
            }
        }
        Item::DashedLine {
            from,
            to,
            thickness,
            dash,
            dotted,
            color,
        } => {
            let length = libm::hypotf(to.0 - from.0, to.1 - from.1);
            let style = if *dotted {
                BorderStyle::Dotted
            } else {
                BorderStyle::Dashed
            };
            // The pattern is laid out for the line from end to end; round dots stand inside it.
            let pattern = dash_pattern(
                style,
                *dash,
                if *dotted && *dash > 3.0 {
                    length + dash
                } else {
                    length
                },
                false,
            );
            let mut builder = PathBuilder::new();
            builder.move_to(from.0, from.1);
            builder.line_to(to.0, to.1);
            if let Some(path) = builder.finish() {
                surface.set_fill(None);
                surface.set_stroke(Some(dashed_stroke(*color, *thickness, pattern)));
                surface.draw_path(&path);
                surface.set_stroke(None);
            }
        }
        Item::Text {
            x,
            y,
            face,
            size,
            variations,
            color,
            glyphs,
            text,
            synthetic,
            vertical,
            palette,
        } => {
            let Some(PdfFont {
                font,
                glyphs: numbers,
            }) = font_cache.get_with_palette(fonts, *face, variations, palette.as_deref())
            else {
                return;
            };
            // The face's `size-adjust` sets its glyphs larger or smaller than the font size.
            let size = &(*size * fonts.face(*face).size_adjust);
            if glyphs.is_empty() || *size <= 0.0 {
                return;
            }
            // Glyphs one below the other advance down the page: by negative advances in
            // krilla, whose vertical advances point up.
            let (across, down) = if *vertical { (0.0, -1.0) } else { (1.0, 0.0) };
            let krilla_glyphs: Vec<KrillaGlyph> = glyphs
                .iter()
                .map(|glyph| {
                    // A font made of outlines numbers the glyphs it has anew.
                    let id = match &numbers {
                        Some(numbers) => numbers.get(&glyph.id).copied().map_or(0, u32::from),
                        None => glyph.id,
                    };
                    KrillaGlyph::new(
                        GlyphId::new(id),
                        across * glyph.advance / size,
                        glyph.x_offset / size,
                        glyph.y_offset / size,
                        down * glyph.advance / size,
                        glyph.cluster.0 as usize..glyph.cluster.1 as usize,
                        None,
                    )
                })
                .collect();
            surface.set_fill(Some(fill(*color)));
            if font_cache.seed_notdef
                && krilla_glyphs
                    .iter()
                    .any(|glyph| glyph.glyph_id == GlyphId::new(0))
                && font_cache.seeded.insert(font.clone())
            {
                seed_notdef(surface, &font);
            }
            // Synthetic bold: an outline in the text color thickens every stroke.
            surface.set_stroke(synthetic.bold.then(|| Stroke {
                paint: rgb::Color::new(channel(color.r), channel(color.g), channel(color.b)).into(),
                width: fake_bold_width(*size),
                opacity: NormalizedF32::new(color.a.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE),
                ..Stroke::default()
            }));
            // Synthetic italics lean every glyph from its baseline: for a horizontal run that of
            // the run, for glyphs one below the other each its own, as Chrome slants them.
            let slanted = |surface: &mut Surface<'_>, baseline: f32| {
                surface.push_transform(&Transform::from_row(1.0, 0.0, -SLANT, 1.0, SLANT * baseline, 0.0));
            };
            if synthetic.italic && *vertical {
                let mut pen = *y;
                for (glyph, krilla_glyph) in glyphs.iter().zip(&krilla_glyphs) {
                    slanted(surface, pen - glyph.y_offset);
                    surface.draw_glyphs(
                        Point::from_xy(*x, pen),
                        std::slice::from_ref(krilla_glyph),
                        font.clone(),
                        text,
                        *size,
                        false,
                    );
                    surface.pop();
                    pen += glyph.advance;
                }
            } else {
                if synthetic.italic {
                    slanted(surface, *y);
                }
                surface.draw_glyphs(Point::from_xy(*x, *y), &krilla_glyphs, font, text, *size, false);
                if synthetic.italic {
                    surface.pop();
                }
            }
            surface.set_stroke(None);
        }
        Item::PushOpacity(opacity) if image_cache.opaque && *opacity <= 0.0 => hide(surface, true),
        Item::PushOpacity(opacity) => {
            surface.push_opacity(NormalizedF32::new(opacity.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE));
        }
        Item::PushBlend(mode) => surface.push_blend_mode(blend_mode(*mode)),
        // Isolation changes nothing where nothing is transparent.
        Item::PushIsolated if image_cache.opaque => surface.push_opacity(NormalizedF32::ONE),
        Item::PushIsolated => surface.push_isolated(),
        Item::PushLayer(layer) => {
            if !image_cache
                .layers
                .start(surface, layer, (stream, transforms.len()))
            {
                surface.push_opacity(NormalizedF32::ONE);
            }
        }
        Item::PushMask(mask) => {
            // The mask's items, drawn where it applies, in a stream of their own.
            let mut builder = surface.stream_builder();
            let mut inner = builder.surface();
            let mut inner_transforms = vec![current];
            for item in &mask.items {
                draw(
                    &mut inner,
                    item,
                    fonts,
                    font_cache,
                    image_cache,
                    &mut inner_transforms,
                    visible,
                );
            }
            inner.finish();
            let stream = builder.finish();
            let kind = if mask.luminance {
                krilla::mask::MaskType::Luminosity
            } else {
                krilla::mask::MaskType::Alpha
            };
            surface.push_mask(krilla::mask::Mask::new(stream, kind));
        }
        Item::PushClip(rect, radii) => {
            match clip_path(*rect, radii.as_ref()) {
                Some(path) => surface.push_clip_path(&path, &FillRule::NonZero),
                // Nothing is visible in an empty clip; the group keeps push and pop balanced.
                None => hide(surface, image_cache.opaque),
            }
        }
        Item::PushClipShape(shape) => {
            let rule = match shape {
                ClipGeometry::Polygon { even_odd: true, .. } | ClipGeometry::Path { even_odd: true, .. } => {
                    FillRule::EvenOdd
                }
                _ => FillRule::NonZero,
            };
            match shape_path(shape) {
                Some(path) => surface.push_clip_path(&path, &rule),
                None => hide(surface, image_cache.opaque),
            }
        }
        Item::Gradient { rect, gradient } => {
            draw_gradient(surface, *rect, gradient, !image_cache.interpolate);
        }
        Item::Image { rect, image } => draw_image(surface, *rect, image, fonts, image_cache),
        Item::BoxShadow(paint) => {
            shadow::draw(surface, paint, image_cache.interpolate, &mut image_cache.shadows);
        }
        Item::TextShadow {
            x,
            y,
            face,
            size,
            variations,
            glyphs,
            lines,
            color,
            blur,
        } => {
            let interpolate = image_cache.interpolate;
            shadow::draw_text(
                surface,
                &mut image_cache.text_shadows,
                format!("{item:?}"),
                || {
                    shadow::text_shadow(
                        (fonts.face(*face), variations),
                        (*x, *y),
                        *size,
                        (glyphs, lines),
                        *color,
                        *blur,
                        interpolate,
                    )
                },
            );
        }
        Item::PushTransform(matrix) => {
            let composed = current.and_then(|current| compose(current, *matrix));
            transforms.push(composed);
            match composed {
                Some(_) => {
                    let [sx, ky, kx, sy, tx, ty] = *matrix;
                    surface.push_transform(&Transform::from_row(sx, ky, kx, sy, tx, ty));
                }
                // Content transformed beyond reason is clipped away, untransformed.
                None => {
                    if let Some(nowhere) = rect_path(RectF {
                        x: -2.0,
                        y: -2.0,
                        width: 1.0,
                        height: 1.0,
                    }) {
                        surface.push_clip_path(&nowhere, &FillRule::NonZero);
                    }
                }
            }
        }
        Item::Filtered(paint) => {
            if let Some(current) = current {
                draw_filtered(surface, paint, fonts, current, visible);
            }
        }
        Item::Pop => surface.pop(),
        Item::Tag(_) => {}
    }
}

/// Draws a box filtered as pixels (see [`filtered`]): an image on the grid of 300 ppi of the
/// page, where `current` maps the box's pixels to the page's, of the part of it that shows in
/// `visible` (in the page's pixels) where that is known.
fn draw_filtered(
    surface: &mut Surface<'_>,
    paint: &fagus_paint::FilteredPaint,
    fonts: &FontDb,
    current: [f32; 6],
    visible: Option<RectF>,
) {
    let [sx, ky, kx, sy, tx, ty] = current;
    let device = tiny_skia::Transform::from_row(sx, ky, kx, sy, tx, ty)
        .post_scale(filtered::PIXELS_PER_PX, filtered::PIXELS_PER_PX);
    let visible = visible.map(|rect| RectF {
        x: rect.x * filtered::PIXELS_PER_PX,
        y: rect.y * filtered::PIXELS_PER_PX,
        width: rect.width * filtered::PIXELS_PER_PX,
        height: rect.height * filtered::PIXELS_PER_PX,
    });
    let Some((pixels, rect)) = filtered::pixels(paint, fonts, device, visible) else {
        return;
    };
    // The pixels go into the image, and are gone before krilla compresses it.
    let Some((image, placed)) = filtered::image(pixels, rect) else {
        return;
    };
    if let Some(size) = Size::from_wh(placed.width, placed.height) {
        surface.push_transform(&Transform::from_translate(placed.x, placed.y));
        surface.draw_image(image, size);
        surface.pop();
    }
}

/// The mean color of gradient stops.
fn average(stops: &[(f32, Rgba)]) -> Rgba {
    let count = stops.len().max(1) as f32;
    let sum = stops.iter().fold([0.0; 4], |sum, (_, color)| {
        [
            sum[0] + color.r,
            sum[1] + color.g,
            sum[2] + color.b,
            sum[3] + color.a,
        ]
    });
    Rgba {
        r: sum[0] / count,
        g: sum[1] / count,
        b: sum[2] / count,
        a: sum[3] / count,
    }
}

fn gradient_stops(stops: &[(f32, Rgba)]) -> Vec<Stop> {
    stops
        .iter()
        .map(|(offset, color)| Stop {
            offset: NormalizedF32::new(offset.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ZERO),
            color: rgb::Color::new(channel(color.r), channel(color.g), channel(color.b)).into(),
            opacity: NormalizedF32::new(color.a.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE),
        })
        .collect()
}

/// The start and end point of a linear gradient, with its stops.
type GradientLine = ((f32, f32), (f32, f32), Vec<(f32, Rgba)>);

/// A repeating linear gradient as one that does not repeat: its period repeated across a tile
/// of `size`. Repetition in PDF takes a PostScript function, which PDF/A does not allow and
/// which Fagus keeps to gradients of solid bands (see `draw_gradient`). `None` for a period too
/// short to repeat this way.
fn unrepeat(size: RectF, start: (f32, f32), end: (f32, f32), stops: &[(f32, Rgba)]) -> Option<GradientLine> {
    const MAX_PERIODS: f32 = 2000.0;
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let length = dx * dx + dy * dy;
    if length <= f32::EPSILON || stops.is_empty() {
        return None;
    }
    // Where the corners of the tile are on the gradient line, in periods.
    let along = |x: f32, y: f32| ((x - start.0) * dx + (y - start.1) * dy) / length;
    let corners = [
        along(0.0, 0.0),
        along(size.width, 0.0),
        along(0.0, size.height),
        along(size.width, size.height),
    ];
    let first = corners.iter().copied().fold(f32::INFINITY, f32::min).floor();
    let last = corners
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .max(first + 1.0);
    let periods = last - first;
    if !periods.is_finite() || periods > MAX_PERIODS {
        return None;
    }
    let mut repeated = Vec::with_capacity(stops.len() * periods as usize);
    for period in 0..periods as usize {
        repeated.extend(
            stops
                .iter()
                .map(|(offset, color)| ((period as f32 + offset) / periods, *color)),
        );
    }
    Some((
        (start.0 + dx * first, start.1 + dy * first),
        (start.0 + dx * last, start.1 + dy * last),
        repeated,
    ))
}

/// Whether the colors of a gradient change only at hard stops (two stops at the same place),
/// so that it paints bands of solid colors.
fn hard_stops(stops: &[(f32, Rgba)]) -> bool {
    stops
        .windows(2)
        .all(|pair| pair[0].0 >= pair[1].0 || pair[0].1 == pair[1].1)
}

/// Fills a background tile with a gradient. Shading geometry is relative to the tile. In PDF/A
/// (`archival`), conic gradients are images, since their shading takes a PostScript function.
/// The most stops of a conic gradient drawn as a sweep shading.
const MAX_SWEEP_STOPS: usize = 96;

fn draw_gradient(surface: &mut Surface<'_>, rect: RectF, gradient: &GradientPaint, archival: bool) {
    let Some(path) = rect_path(rect) else { return };
    let paint: Paint = match gradient {
        GradientPaint::Solid(color) => {
            surface.set_fill(Some(fill(*color)));
            surface.set_stroke(None);
            surface.draw_path(&path);
            return;
        }
        GradientPaint::Linear {
            start,
            end,
            stops,
            repeat,
        } => {
            // Bands of solid colors repeat as Chrome repeats every gradient: in a shading whose
            // PostScript function takes the place in the period, which viewers evaluate at each
            // pixel. Viewers such as Poppler draw the shading of the periods one after the other
            // from a table of colors a pixel apart, which blurs bands a pixel or two wide (the
            // bars of a barcode) into gray. PDF/A, which allows no PostScript, keeps that.
            let bands = *repeat && !archival && hard_stops(stops);
            let unrepeated = (*repeat && !bands)
                .then(|| unrepeat(rect, *start, *end, stops))
                .flatten();
            let (start, end, stops, spread_method) = match (&unrepeated, repeat) {
                (Some((start, end, stops)), _) => (*start, *end, &stops[..], SpreadMethod::Pad),
                (None, true) if archival => {
                    let solid = GradientPaint::Solid(average(stops));
                    return draw_gradient(surface, rect, &solid, archival);
                }
                (None, true) => (*start, *end, &stops[..], SpreadMethod::Repeat),
                (None, false) => (*start, *end, &stops[..], SpreadMethod::Pad),
            };
            let (x1, y1) = (rect.x + start.0, rect.y + start.1);
            let (x2, y2) = (rect.x + end.0, rect.y + end.1);
            if spread_method == SpreadMethod::Pad {
                LinearGradient {
                    x1,
                    y1,
                    x2,
                    y2,
                    transform: Transform::identity(),
                    spread_method,
                    stops: gradient_stops(stops),
                    anti_alias: false,
                }
                .into()
            } else {
                // krilla turns the line of a repeating gradient onto the x axis with the
                // system's `atan2`, `sin` and `cos`, whose last bits differ between platforms: it
                // gets a line along the x axis, turned into place by the direction's cosine and
                // sine, which take a square root alone.
                let length = libm::hypotf(x2 - x1, y2 - y1);
                let (cos, sin) = if length > 0.0 {
                    ((x2 - x1) / length, (y2 - y1) / length)
                } else {
                    (1.0, 0.0)
                };
                LinearGradient {
                    x1: 0.0,
                    y1: 0.0,
                    x2: length,
                    y2: 0.0,
                    transform: Transform::from_row(cos, sin, -sin, cos, x1, y1),
                    spread_method,
                    stops: gradient_stops(stops),
                    anti_alias: false,
                }
                .into()
            }
        }
        GradientPaint::Radial {
            center,
            radius,
            scale_y,
            stops,
        } => RadialGradient {
            fx: 0.0,
            fy: 0.0,
            fr: 0.0,
            cx: 0.0,
            cy: 0.0,
            cr: *radius,
            // Circles around the origin, moved to the center and stretched into ellipses.
            transform: Transform::from_row(1.0, 0.0, 0.0, *scale_y, rect.x + center.0, rect.y + center.1),
            spread_method: SpreadMethod::Pad,
            stops: gradient_stops(stops),
            anti_alias: false,
        }
        .into(),
        // PDF viewers evaluate a sweep with a function of at most ~100 stops: one with more (a
        // repeating conic gradient of short periods) is drawn as an image, as in PDF/A.
        GradientPaint::Conic {
            center,
            start_angle,
            stops,
        } if archival || stops.len() > MAX_SWEEP_STOPS => {
            if let Some(image) = raster::conic(rect, *center, *start_angle, stops)
                && let Some(size) = Size::from_wh(rect.width, rect.height)
            {
                surface.push_transform(&Transform::from_translate(rect.x, rect.y));
                surface.draw_image(image, size);
                surface.pop();
            }
            return;
        }
        GradientPaint::Conic {
            center,
            start_angle,
            stops,
        } => {
            // CSS measures from the top, sweep gradients from the x axis; both turn clockwise
            // on a page whose y axis points down.
            let (sin, cos) = libm::sincosf((start_angle - 90.0).to_radians());
            SweepGradient {
                cx: 0.0,
                cy: 0.0,
                start_angle: 0.0,
                end_angle: 360.0,
                transform: Transform::from_row(cos, sin, -sin, cos, rect.x + center.0, rect.y + center.1),
                spread_method: SpreadMethod::Pad,
                stops: gradient_stops(stops),
                anti_alias: false,
            }
            .into()
        }
    };
    surface.set_fill(Some(Fill {
        paint,
        opacity: NormalizedF32::ONE,
        rule: FillRule::NonZero,
    }));
    surface.set_stroke(None);
    surface.draw_path(&path);
}

fn draw_border_side(
    surface: &mut Surface<'_>,
    points: &[(f32, f32); 4],
    color: Rgba,
    style: BorderStyle,
    width: f32,
) {
    match style {
        BorderStyle::Dashed | BorderStyle::Dotted => {
            // A dashed or dotted line along the middle of the side, from corner to corner, as
            // Chrome prints it; round dots stand inside its ends.
            let across = |a: (f32, f32), b: (f32, f32)| (f32::midpoint(a.0, b.0), f32::midpoint(a.1, b.1));
            let (outer_start, outer_end) = (points[0], points[1]);
            let horizontal = (outer_end.0 - outer_start.0).abs() >= (outer_end.1 - outer_start.1).abs();
            let (middle_start, middle_end) = (across(points[0], points[3]), across(points[1], points[2]));
            let (mut start, mut end) = if horizontal {
                ((outer_start.0, middle_start.1), (outer_end.0, middle_end.1))
            } else {
                ((middle_start.0, outer_start.1), (middle_end.0, outer_end.1))
            };
            // From left to right and from top to bottom, as Blink draws them.
            if (horizontal && start.0 > end.0) || (!horizontal && start.1 > end.1) {
                (start, end) = (end, start);
            }
            let length = libm::hypotf(end.0 - start.0, end.1 - start.1);
            let pattern = dash_pattern(style, width, length, false);
            if length <= 0.0 {
                return;
            }
            let along = ((end.0 - start.0) / length, (end.1 - start.1) / length);
            if let Some((_, true)) = pattern {
                start = (start.0 + along.0 * width / 2.0, start.1 + along.1 * width / 2.0);
                end = (end.0 - along.0 * width / 2.0, end.1 - along.1 * width / 2.0);
            } else if style == BorderStyle::Dotted {
                (start, end) = dots_at_ends(surface, (start, end), along, length, width, color);
            }
            // Square-ended dashes are filled rectangles, as Chrome's PDFs give them (Skia fills
            // the outlines of dashed strokes), where viewers draw thin strokes bolder: the
            // dashes of a side as one path, square dots each on its own (they blend otherwise
            // where they share a pixel).
            if let Some(([dash, gap], false)) = pattern
                && dash > 0.0
                && let Some(dashes) = dash_rectangles((start, end), along, width, (dash, gap))
            {
                surface.set_stroke(None);
                surface.set_fill(Some(fill(color)));
                if style == BorderStyle::Dotted {
                    for dash in &dashes {
                        let mut builder = PathBuilder::new();
                        builder.push_rect(*dash);
                        if let Some(path) = builder.finish() {
                            surface.draw_path(&path);
                        }
                    }
                } else {
                    let mut builder = PathBuilder::new();
                    for dash in &dashes {
                        builder.push_rect(*dash);
                    }
                    if let Some(path) = builder.finish() {
                        surface.draw_path(&path);
                    }
                }
                return;
            }
            let mut builder = PathBuilder::new();
            builder.move_to(start.0, start.1);
            builder.line_to(end.0, end.1);
            let Some(path) = builder.finish() else { return };
            surface.set_fill(None);
            surface.set_stroke(Some(dashed_stroke(color, width, pattern)));
            surface.draw_path(&path);
            surface.set_stroke(None);
        }
        _ => {
            let mut builder = PathBuilder::new();
            builder.move_to(points[0].0, points[0].1);
            for point in &points[1..] {
                builder.line_to(point.0, point.1);
            }
            builder.close();
            if let Some(path) = builder.finish() {
                surface.set_fill(Some(fill(color)));
                surface.set_stroke(None);
                surface.draw_path(&path);
            }
        }
    }
}

/// The dashes of a horizontal or vertical line from `start` to `end` (in direction `along`),
/// `width` wide, `dash` long and `gap` apart from its start on, as rectangles; `None` for a
/// slanted line.
fn dash_rectangles(
    (start, end): ((f32, f32), (f32, f32)),
    along: (f32, f32),
    width: f32,
    (dash, gap): (f32, f32),
) -> Option<Vec<Rect>> {
    let horizontal = (start.1 - end.1).abs() < 1e-3;
    if !horizontal && (start.0 - end.0).abs() >= 1e-3 {
        return None;
    }
    let length = (end.0 - start.0).abs() + (end.1 - start.1).abs();
    let mut dashes = Vec::new();
    let mut from = 0.0;
    while from < length {
        let to = (from + dash).min(length);
        let (a, b) = (
            (start.0 + along.0 * from, start.1 + along.1 * from),
            (start.0 + along.0 * to, start.1 + along.1 * to),
        );
        let rect = if horizontal {
            Rect::from_ltrb(a.0.min(b.0), a.1 - width / 2.0, a.0.max(b.0), a.1 + width / 2.0)
        } else {
            Rect::from_ltrb(a.0 - width / 2.0, a.1.min(b.1), a.0 + width / 2.0, a.1.max(b.1))
        };
        dashes.extend(rect);
        from += dash + gap;
    }
    Some(dashes)
}

/// Draws whole dots at the ends of a thin dotted line from `start` to `end`, as Blink enforces
/// them where the line's length is not an odd multiple of its width, and returns where the rest
/// of the dots go (`EnforceDotsAtEndpoints`).
fn dots_at_ends(
    surface: &mut Surface<'_>,
    (start, end): ((f32, f32), (f32, f32)),
    along: (f32, f32),
    length: f32,
    width: f32,
    color: Rgba,
) -> ((f32, f32), (f32, f32)) {
    let (length, width_px) = (length.round() as i32, width.round() as i32);
    let (by_4, by_6) = (length.rem_euclid(4), length.rem_euclid(6));
    // Grow the start dot by a pixel, or move the line's start by one, at the start and the end.
    let (mut start_dot, mut start_growth, mut start_offset) = (false, 0, 0);
    let (mut end_dot, mut end_growth) = (false, 0);
    if (width_px == 1 && length % 2 == 0) || (width_px == 3 && by_6 == 0) {
        (start_dot, start_growth, start_offset) = (true, 1, 1);
    }
    if (width_px == 2 && (by_4 == 0 || by_4 == 1)) || (width_px == 3 && (by_6 == 1 || by_6 == 2)) {
        (start_dot, start_offset) = (true, -1);
    }
    if (width_px == 2 && by_4 == 0) || (width_px == 3 && by_6 == 1) {
        end_dot = true;
    }
    if (width_px == 2 && by_4 == 3) || (width_px == 3 && (by_6 == 4 || by_6 == 5)) {
        (start_dot, start_offset) = (true, 1);
    }
    if width_px == 3 && by_6 == 5 {
        end_dot = true;
    } else if width_px == 3 && by_6 == 0 {
        (end_dot, end_growth) = (true, 1);
    }
    // A square of the width across the line, from `from` along it for `size`.
    let across = (-along.1, along.0);
    let half = width / 2.0;
    let dot = |surface: &mut Surface<'_>, from: (f32, f32), size: f32| {
        let corner = |a: f32, b: f32| {
            (
                from.0 + along.0 * a + across.0 * b,
                from.1 + along.1 * a + across.1 * b,
            )
        };
        let mut builder = PathBuilder::new();
        let points = [
            corner(0.0, -half),
            corner(size, -half),
            corner(size, width - half),
            corner(0.0, width - half),
        ];
        builder.move_to(points[0].0, points[0].1);
        for point in &points[1..] {
            builder.line_to(point.0, point.1);
        }
        builder.close();
        if let Some(path) = builder.finish() {
            surface.set_stroke(None);
            surface.set_fill(Some(fill(color)));
            surface.draw_path(&path);
            surface.set_fill(None);
        }
    };
    let (mut start, mut end) = (start, end);
    if start_dot {
        dot(surface, start, width + start_growth as f32);
        let moved = 2.0 * width + start_offset as f32;
        start = (start.0 + along.0 * moved, start.1 + along.1 * moved);
    }
    if end_dot {
        let size = width + end_growth as f32;
        dot(surface, (end.0 - along.0 * size, end.1 - along.1 * size), size);
        let moved = width + end_growth as f32 + 1.0;
        end = (end.0 - along.0 * moved, end.1 - along.1 * moved);
    }
    (start, end)
}

/// A stroke of `color` and `width` with the dashes of `pattern`, round dots with round caps.
fn dashed_stroke(color: Rgba, width: f32, pattern: Option<([f32; 2], bool)>) -> Stroke {
    Stroke {
        paint: rgb::Color::new(channel(color.r), channel(color.g), channel(color.b)).into(),
        width,
        line_cap: match pattern {
            Some((_, true)) => LineCap::Round,
            _ => LineCap::Butt,
        },
        opacity: NormalizedF32::new(color.a.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE),
        dash: pattern.map(|(array, _)| StrokeDash {
            array: array.to_vec(),
            offset: 0.0,
        }),
        ..Stroke::default()
    }
}

/// The dashes and gaps Chrome strokes a dashed or dotted line of `length` with (Blink's
/// `StyledStrokeData`), and whether they are round dots; `None` for a solid line, where there is
/// no room for dashes. Dashes are three widths long and two apart, from 3px two widths and one;
/// dots to 3px are square dashes a width apart, wider ones round. The gaps stretch so that the
/// line holds whole dashes, from end to end or, on a `closed` path, all round.
fn dash_pattern(style: BorderStyle, width: f32, length: f32, closed: bool) -> Option<([f32; 2], bool)> {
    // Dashes of no width: a solid line (see `Item::DashedLine`).
    if width <= 0.0 {
        return None;
    }
    if style == BorderStyle::Dashed || width <= 3.0 {
        let (mut dash, mut gap) = (width, width);
        if style == BorderStyle::Dashed {
            let (dash_ratio, gap_ratio) = if width >= 3.0 { (2.0, 1.0) } else { (3.0, 2.0) };
            dash *= dash_ratio;
            gap *= gap_ratio;
        }
        if length <= dash * 2.0 {
            return None;
        }
        let two = 2.0 * dash + gap + if closed { gap } else { 0.0 };
        if length <= two {
            let scale = length / two;
            return Some(([dash * scale, gap * scale], false));
        }
        let gap = if style == BorderStyle::Dashed {
            best_gap(length, dash, gap, closed)
        } else {
            gap
        };
        return Some(([dash, gap], false));
    }
    if length < width * 2.0 {
        return Some(([0.0, width * 2.0], true));
    }
    // A little short, so that the last dot is whole.
    Some(([0.0, best_gap(length, width, width, closed) + width - 0.01], true))
}

/// The gap between dashes of `dash` that fits whole ones into `length` and differs least from
/// `gap` (Blink's `SelectBestDashGap`).
fn best_gap(length: f32, dash: f32, gap: f32, closed: bool) -> f32 {
    let available = if closed { length } else { length + gap };
    let fewer = (available / (dash + gap)).floor();
    let more = fewer + 1.0;
    let gaps = |dashes: f32| if closed { dashes } else { dashes - 1.0 };
    let fewer_gap = (length - fewer * dash) / gaps(fewer);
    let more_gap = (length - more * dash) / gaps(more);
    if more_gap <= 0.0 || (fewer_gap - gap).abs() < (more_gap - gap).abs() {
        fewer_gap
    } else {
        more_gap
    }
}

/// The path along the middle of a rounded border, from the top side's start after the top left
/// corner clockwise, as Blink adds a rounded rectangle, and its length.
fn centerline(rect: RectF, radii: &Radii) -> (Option<Path>, f32) {
    let mut builder = PathBuilder::new();
    let length = append_centerline(&mut builder, rect, radii);
    (builder.finish(), length)
}

/// Appends the path along the middle of a rounded border (see [`centerline`]); its length.
pub(crate) fn append_centerline(builder: &mut impl Outline, rect: RectF, radii: &Radii) -> f32 {
    const K: f32 = 0.552_284_8;
    let RectF { x, y, width, height } = rect;
    let (right, bottom) = (x + width, y + height);
    let [top_left, top_right, bottom_right, bottom_left] = *radii;
    builder.move_to(x + top_left.0, y);
    builder.line_to(right - top_right.0, y);
    builder.cubic_to(
        right - top_right.0 * (1.0 - K),
        y,
        right,
        y + top_right.1 * (1.0 - K),
        right,
        y + top_right.1,
    );
    builder.line_to(right, bottom - bottom_right.1);
    builder.cubic_to(
        right,
        bottom - bottom_right.1 * (1.0 - K),
        right - bottom_right.0 * (1.0 - K),
        bottom,
        right - bottom_right.0,
        bottom,
    );
    builder.line_to(x + bottom_left.0, bottom);
    builder.cubic_to(
        x + bottom_left.0 * (1.0 - K),
        bottom,
        x,
        bottom - bottom_left.1 * (1.0 - K),
        x,
        bottom - bottom_left.1,
    );
    builder.line_to(x, y + top_left.1);
    builder.cubic_to(
        x,
        y + top_left.1 * (1.0 - K),
        x + top_left.0 * (1.0 - K),
        y,
        x + top_left.0,
        y,
    );
    builder.close();
    let straight = (width - top_left.0 - top_right.0)
        + (height - top_right.1 - bottom_right.1)
        + (width - bottom_right.0 - bottom_left.0)
        + (height - bottom_left.1 - top_left.1);
    let curved: f32 = [top_left, top_right, bottom_right, bottom_left]
        .into_iter()
        .map(measured_quarter)
        .sum();
    straight.max(0.0) + curved
}

/// The length Skia measures for a quarter of an ellipse with radii `(rx, ry)`, as Chrome sizes
/// dashes by it: its conic cut in halves of its parameter until the middle of each piece lies
/// within half a pixel of its chord, and the chords added up (`SkContourMeasure`).
fn measured_quarter((rx, ry): (f32, f32)) -> f32 {
    fn pieces(
        at: &dyn Fn(f32) -> (f32, f32),
        (t0, p0): (f32, (f32, f32)),
        (t1, p1): (f32, (f32, f32)),
        depth: u8,
    ) -> f32 {
        let middle = f32::midpoint(t0, t1);
        let point = at(middle);
        let off = (point.0 - f32::midpoint(p0.0, p1.0))
            .abs()
            .max((point.1 - f32::midpoint(p0.1, p1.1)).abs());
        if depth < 20 && off > 0.5 {
            pieces(at, (t0, p0), (middle, point), depth + 1)
                + pieces(at, (middle, point), (t1, p1), depth + 1)
        } else {
            libm::hypotf(p1.0 - p0.0, p1.1 - p0.1)
        }
    }
    if rx <= 0.0 || ry <= 0.0 {
        return rx.max(ry);
    }
    // From (0, ry) towards the corner (0, 0) to (rx, 0).
    let at = |t: f32| {
        let (a, b, c) = (
            (1.0 - t) * (1.0 - t),
            2.0 * std::f32::consts::FRAC_1_SQRT_2 * t * (1.0 - t),
            t * t,
        );
        let weight = a + b + c;
        (c * rx / weight, a * ry / weight)
    };
    pieces(&at, (0.0, at(0.0)), (1.0, at(1.0)), 0)
}

fn rect_path(rect: RectF) -> Option<Path> {
    let mut builder = PathBuilder::new();
    builder.push_rect(Rect::from_xywh(
        rect.x,
        rect.y,
        rect.width.max(0.0),
        rect.height.max(0.0),
    )?);
    builder.finish()
}

fn rounded_rect(rect: RectF, radii: &Radii) -> Option<Path> {
    let mut builder = PathBuilder::new();
    append_rounded_rect(&mut builder, rect, radii);
    builder.finish()
}

/// Appends a rectangle with elliptical corners (cubic Bézier approximation).
pub(crate) fn append_rounded_rect(builder: &mut impl Outline, rect: RectF, radii: &Radii) {
    const K: f32 = 0.552_284_8;
    let RectF { x, y, width, height } = rect;
    let [(tl_x, tl_y), (tr_x, tr_y), (br_x, br_y), (bl_x, bl_y)] = *radii;
    // Square corners need no curve.
    let round = |rx: f32, ry: f32| rx > 0.0 || ry > 0.0;
    builder.move_to(x + tl_x, y);
    builder.line_to(x + width - tr_x, y);
    if round(tr_x, tr_y) {
        builder.cubic_to(
            x + width - tr_x + tr_x * K,
            y,
            x + width,
            y + tr_y - tr_y * K,
            x + width,
            y + tr_y,
        );
    }
    builder.line_to(x + width, y + height - br_y);
    if round(br_x, br_y) {
        builder.cubic_to(
            x + width,
            y + height - br_y + br_y * K,
            x + width - br_x + br_x * K,
            y + height,
            x + width - br_x,
            y + height,
        );
    }
    builder.line_to(x + bl_x, y + height);
    if round(bl_x, bl_y) {
        builder.cubic_to(
            x + bl_x - bl_x * K,
            y + height,
            x,
            y + height - bl_y + bl_y * K,
            x,
            y + height - bl_y,
        );
    }
    builder.line_to(x, y + tl_y);
    if round(tl_x, tl_y) {
        builder.cubic_to(x, y + tl_y - tl_y * K, x + tl_x - tl_x * K, y, x + tl_x, y);
    }
    builder.close();
}

#[cfg(test)]
mod tests {
    use super::{
        BorderStyle, Conformance, Date, RectF, centerline, chrome_sheet_side, dash_pattern, dash_rectangles,
        fake_bold_width,
    };

    #[test]
    fn names_the_pdf_a_levels() {
        for (level, name, part) in [
            ("1b", "PDF/A-1b", 1),
            ("2b", "PDF/A-2b", 2),
            ("2u", "PDF/A-2u", 2),
            ("2a", "PDF/A-2a", 2),
            ("3b", "PDF/A-3b", 3),
            ("3u", "PDF/A-3u", 3),
            ("3a", "PDF/A-3a", 3),
            ("4", "PDF/A-4", 4),
            ("4e", "PDF/A-4e", 4),
            ("4f", "PDF/A-4f", 4),
        ] {
            let conformance = Conformance::from_level(level).expect(level);
            assert_eq!(conformance.name(), name);
            assert_eq!(conformance.part(), part);
            assert_eq!(conformance.is_tagged(), level.ends_with('a'));
        }
        for unknown in ["1a", "4b", "4u", "5", ""] {
            assert_eq!(Conformance::from_level(unknown), None, "{unknown}");
        }
    }

    /// A PDF as text, its streams (and so its object streams) inflated where they are deflated.
    fn inflated(pdf: &[u8]) -> String {
        use std::io::Read as _;
        let mut text = String::new();
        let mut rest = pdf;
        while let Some(start) = rest.windows(7).position(|window| window == b"stream\n") {
            text.push_str(&String::from_utf8_lossy(&rest[..start]));
            let body = &rest[start + 7..];
            let end = body
                .windows(9)
                .position(|window| window == b"endstream")
                .unwrap_or(body.len());
            let mut inflated = Vec::new();
            if flate2::read::ZlibDecoder::new(&body[..end])
                .read_to_end(&mut inflated)
                .is_ok()
            {
                text.push_str(&String::from_utf8_lossy(&inflated));
            }
            rest = body.get(end + 9..).unwrap_or_default();
        }
        text.push_str(&String::from_utf8_lossy(rest));
        text
    }

    #[test]
    fn draws_conic_gradients_of_many_stops_as_images() {
        use fagus_paint::{GradientPaint, Item, PageList};
        use fagus_style::values::Rgba;
        let (gold, light) = (
            Rgba::from_u8(0xb0, 0x8d, 0x57, 1.0),
            Rgba::from_u8(0xc9, 0xa8, 0x6a, 1.0),
        );
        let pdf = |count: usize| {
            // Hard stops every 12 degrees, as a repeating conic gradient gives them.
            let stops = (0..count)
                .map(|index| {
                    let at = (index / 2 + index % 2) as f32 / count as f32 * 2.0;
                    (at.min(1.0), if (index / 2) % 2 == 0 { gold } else { light })
                })
                .collect();
            let page = PageList {
                width: 100.0,
                height: 100.0,
                items: vec![Item::Gradient {
                    rect: RectF {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                    gradient: std::sync::Arc::new(GradientPaint::Conic {
                        center: (50.0, 50.0),
                        start_angle: 0.0,
                        stops,
                    }),
                }],
                bleed: 0.0,
                marks: fagus_layout::PageMarks::default(),
                orientation: fagus_layout::PageOrientation::Upright,
            };
            let written = super::write(
                vec![page],
                &fagus_text::FontDb::new(),
                &super::Metadata::default(),
                &super::Options::default(),
            )
            .expect("pdf");
            inflated(&written.pdf)
        };
        // A sweep shading for a few stops; an image for more than viewers evaluate.
        let few = pdf(8);
        assert!(few.contains("/ShadingType") && !few.contains("/Subtype/Image"));
        let many = pdf(120);
        assert!(many.contains("/Subtype/Image") && !many.contains("/ShadingType"));
    }

    #[test]
    fn repeats_bands_of_solid_colors_in_a_postscript_shading() {
        use fagus_paint::{GradientPaint, Item, PageList};
        use fagus_style::values::Rgba;
        // The bars of form-tax-comb-fields' barcode, 0.4mm, 0.3mm, 0.2mm and 0.6mm in a period of
        // 1.5mm, which Chrome draws in a shading whose PostScript function repeats the period.
        let black = Rgba::from_u8(0, 0, 0, 1.0);
        let white = Rgba::from_u8(255, 255, 255, 1.0);
        let bars = vec![
            (0.0, black),
            (0.4 / 1.5, black),
            (0.4 / 1.5, white),
            (0.7 / 1.5, white),
            (0.7 / 1.5, black),
            (0.9 / 1.5, black),
            (0.9 / 1.5, white),
            (1.0, white),
        ];
        let pdf = |stops: Vec<(f32, Rgba)>, conformance: Option<Conformance>| {
            let page = PageList {
                width: 230.0,
                height: 40.0,
                items: vec![Item::Gradient {
                    rect: RectF {
                        x: 0.0,
                        y: 0.0,
                        width: 226.8,
                        height: 34.0,
                    },
                    gradient: std::sync::Arc::new(GradientPaint::Linear {
                        start: (0.0, 17.0),
                        end: (5.67, 17.0),
                        stops,
                        repeat: true,
                    }),
                }],
                bleed: 0.0,
                marks: fagus_layout::PageMarks::default(),
                orientation: fagus_layout::PageOrientation::Upright,
            };
            let written = super::write(
                vec![page],
                &fagus_text::FontDb::new(),
                &super::Metadata {
                    created: Some(Date::from_unix(0)),
                    ..super::Metadata::default()
                },
                &super::Options {
                    conformance,
                    ..super::Options::default()
                },
            )
            .expect("pdf");
            inflated(&written.pdf)
        };
        assert!(pdf(bars.clone(), None).contains("/FunctionType 4"));
        // PDF/A allows no PostScript: the periods one after the other in an axial shading.
        let archived = pdf(bars, Some(Conformance::PdfA2b));
        assert!(!archived.contains("/FunctionType 4") && archived.contains("/ShadingType 2"));
        // Colors that blend between stops stay an axial shading.
        let smooth = pdf(vec![(0.0, black), (1.0, white)], None);
        assert!(!smooth.contains("/FunctionType 4") && smooth.contains("/ShadingType 2"));
    }

    #[test]
    fn sizes_sheets_as_chrome_prints_them() {
        // Whole points, then 1/300 inch, as Chrome's PDFs measure: 80mm by 200mm (226.77pt
        // by 566.93pt) are 227.04pt by 567.12pt, A4 594.96pt by 841.92pt, 333px 250.08pt.
        let mm = 96.0 / 25.4;
        let side = |px: f32| (chrome_sheet_side(px) * 100.0).round() / 100.0;
        assert_eq!(side(80.0 * mm), 227.04);
        assert_eq!(side(200.0 * mm), 567.12);
        assert_eq!(side(210.0 * mm), 594.96);
        assert_eq!(side(297.0 * mm), 841.92);
        assert_eq!(side(333.0), 250.08);
        assert_eq!(side(555.0), 415.92);
        assert_eq!(side(400.0), 300.0);
        assert_eq!(side(1.0), 0.96);
        // Halfway between two steps, as single precision rounds 300/72 times the points.
        assert_eq!(side(300.0), 224.88);
        assert_eq!(side(36.0), 26.88);
        assert_eq!(side(12.0), 9.12);
        assert_eq!(side(756.0), 567.12);
        // Letter and inches stay as they are.
        assert_eq!(side(816.0), 612.0);
        // Exactly as Chrome writes them, a hair off the hundredths.
        assert_eq!(chrome_sheet_side(210.0 * mm).to_bits(), 594.959_96_f32.to_bits());
        assert_eq!(chrome_sheet_side(300.0).to_bits(), 224.879_99_f32.to_bits());
    }

    #[test]
    fn emboldens_text_as_skia_does() {
        // 1/24 of the size up to 9 pixels, 1/32 from 36 on (1.25 pixels at 40, as Chrome's
        // outlines of synthetic bold text measure), in proportion between.
        assert!((fake_bold_width(8.0) - 8.0 / 24.0).abs() < 1e-6);
        assert!((fake_bold_width(40.0) - 1.25).abs() < 1e-6);
        let middle = 22.5_f32;
        assert!((fake_bold_width(middle) - middle * (1.0 / 24.0 + 1.0 / 32.0) / 2.0).abs() < 1e-6);
    }

    #[test]
    fn strokes_dashes_and_dots_as_chrome() {
        let pattern = |style, width, length, closed| {
            dash_pattern(style, width, length, closed)
                .map(|(array, round)| (array.map(|value| (value * 1e4).round() / 1e4), round))
        };
        // Straight sides 101 pixels long, as Chrome 141 prints them.
        assert_eq!(
            pattern(BorderStyle::Dashed, 1.0, 101.0, false),
            Some(([3.0, 1.9], false))
        );
        assert_eq!(
            pattern(BorderStyle::Dashed, 3.0, 101.0, false),
            Some(([6.0, 2.6364], false))
        );
        assert_eq!(
            pattern(BorderStyle::Dashed, 8.0, 101.0, false),
            Some(([16.0, 5.25], false))
        );
        assert_eq!(
            pattern(BorderStyle::Dotted, 2.0, 101.0, false),
            Some(([2.0, 2.0], false))
        );
        assert!(matches!(
            pattern(BorderStyle::Dotted, 6.0, 101.0, false),
            Some(([0.0, _], true))
        ));
        assert_eq!(
            pattern(BorderStyle::Dashed, 4.0, 15.0, false),
            None,
            "no room for dashes"
        );
        // The middle of a 3px border with corners of 16px: Chrome measures 365 pixels along it,
        // the conics cut into chords, and strokes whole dashes all round.
        let middle = RectF {
            x: 21.0,
            y: 21.0,
            width: 118.0,
            height: 78.0,
        };
        let (_, length) = centerline(middle, &[(15.0, 15.0); 4]);
        assert_eq!(length.trunc(), 365.0);
        assert_eq!(
            pattern(BorderStyle::Dashed, 3.0, 365.0, true),
            Some(([6.0, 2.9024], false))
        );
    }

    #[test]
    fn fills_square_dashes_as_rectangles() {
        use fagus_paint::{Item, PageList};
        use fagus_style::values::Rgba;
        // Dashes three long, two apart from the start of a 13px line on, the last one cut.
        let dashes =
            dash_rectangles(((0.0, 5.5), (13.0, 5.5)), (1.0, 0.0), 1.0, (3.0, 2.0)).expect("straight");
        let spans: Vec<(f32, f32, f32, f32)> = dashes
            .iter()
            .map(|rect| (rect.left(), rect.top(), rect.right(), rect.bottom()))
            .collect();
        assert_eq!(
            spans,
            [(0.0, 5.0, 3.0, 6.0), (5.0, 5.0, 8.0, 6.0), (10.0, 5.0, 13.0, 6.0)]
        );
        assert!(dash_rectangles(((0.0, 0.0), (10.0, 10.0)), (0.707, 0.707), 1.0, (3.0, 2.0)).is_none());
        // A dashed and a dotted side are filled, as Chrome's PDFs give them, not stroked with a
        // dash array.
        let side = |style| Item::BorderSide {
            points: [(0.0, 0.0), (100.0, 0.0), (99.0, 1.0), (1.0, 1.0)],
            color: Rgba::from_u8(0, 0, 255, 1.0),
            style,
            width: 1.0,
        };
        let page = PageList {
            width: 100.0,
            height: 100.0,
            items: vec![side(BorderStyle::Dashed), side(BorderStyle::Dotted)],
            bleed: 0.0,
            marks: fagus_layout::PageMarks::default(),
            orientation: fagus_layout::PageOrientation::Upright,
        };
        let written = super::write(
            vec![page],
            &fagus_text::FontDb::new(),
            &super::Metadata::default(),
            &super::Options::default(),
        )
        .expect("pdf");
        let text = inflated(&written.pdf);
        assert!(!text.contains("] 0 d") && !text.contains("]0 d"), "{text}");
    }

    #[test]
    fn lets_go_of_pages_without_images_as_they_are_written() {
        use fagus_paint::{Item, PageList};
        use fagus_style::values::Rgba;
        let rect = |x: f32| Item::Rect {
            rect: RectF {
                x,
                y: 10.0,
                width: 20.0,
                height: 20.0,
            },
            color: Rgba::from_u8(0x27, 0x41, 0xd9, 1.0),
            radii: None,
        };
        let pages: Vec<PageList> = (0..3)
            .map(|index| PageList {
                width: 100.0,
                height: 100.0 + index as f32,
                items: vec![rect(index as f32 * 10.0), rect(50.0)],
                bleed: 0.0,
                marks: fagus_layout::PageMarks::default(),
                orientation: fagus_layout::PageOrientation::Upright,
            })
            .collect();
        let (fonts, metadata, options) = (
            fagus_text::FontDb::new(),
            super::Metadata {
                created: Some(Date::from_unix(1_793_618_400)),
                ..super::Metadata::default()
            },
            super::Options::default(),
        );
        let mut released = pages.clone();
        let mut kept = pages.clone();
        let mut cache = super::ImageCache::new(&options);
        let written = super::attempt(&mut released, &fonts, &metadata, &options, &mut cache, true);
        let mut cache = super::ImageCache::new(&options);
        let same = super::attempt(&mut kept, &fonts, &metadata, &options, &mut cache, false);
        let (Ok(written), Ok(same)) = (written, same) else {
            panic!("not written");
        };
        assert_eq!(written, same, "the same PDF");
        assert!(released.iter().all(|page| page.items.is_empty()));
        assert!(
            kept.iter()
                .zip(&pages)
                .all(|(kept, page)| kept.items == page.items)
        );
        // The pages keep their sizes.
        assert_eq!(
            released.iter().map(|page| page.height).collect::<Vec<_>>(),
            [100.0, 101.0, 102.0]
        );
        // Pages without images are let go of when the PDF is written.
        let mut written = pages.clone();
        super::write_uncompacted(&mut written, &fonts, &metadata, &options).expect("written");
        assert!(written.iter().all(|page| page.items.is_empty()));
        // Images, which may turn out not to decode, keep all pages for drawing them again.
        let image = Item::Image {
            rect: RectF {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            image: std::sync::Arc::new(fagus_layout::Image {
                data: std::sync::Arc::new(Vec::new()),
                format: fagus_layout::ImageFormat::Png,
                width: 10.0,
                height: 10.0,
                natural: (true, true),
                ratio: Some(1.0),
                inline: false,
                orientation: 1,
                filters: std::sync::Arc::new([]),
                pad: 0,
            }),
        };
        assert!(super::draws_image(&image));
        assert!(super::draws_image(&Item::Tiles {
            area: RectF {
                x: 0.0,
                y: 0.0,
                width: 30.0,
                height: 30.0,
            },
            origin: (0.0, 0.0),
            step: (10.0, 10.0),
            tile: std::sync::Arc::new(image),
        }));
        assert!(!super::draws_image(&rect(0.0)));
    }

    #[test]
    fn converts_unix_time() {
        let date = Date::from_unix(1_793_618_400);
        assert_eq!(
            (date.year, date.month, date.day, date.hour, date.minute),
            (2026, 11, 2, 11, 20)
        );
        let leap = Date::from_unix(951_782_400);
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
    }
}
