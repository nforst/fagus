//! Images prepared for PDF, kept from one document to the next: a raster image is decoded and
//! compressed when it is prepared, an SVG document parsed (its text set in the document's
//! fonts), and a logo on every invoice that an engine renders is the same each time.
//!
//! An entry is found by the image's data, compared in full, and what else the result depends
//! on: for a raster image its format, whether it is smoothed and the quality it is written as a
//! JPEG with, for an SVG whether it is
//! parsed to be drawn (see [`crate::svg::parse_to_draw`]) and, for one with text, the fonts it is
//! parsed with. The fonts are those of the document, compared by their data's addresses; an
//! entry holds the data, so the addresses are not reused while it lives. The memo keeps the
//! images used last, up to [`MAX_ENTRIES`] of them and [`MAX_BYTES`] of image data.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use fagus_text::FontDb;
use rustc_hash::FxHasher;

/// The most images kept.
const MAX_ENTRIES: usize = 64;

/// The most image data kept, in bytes; an image of more than a quarter of it is not kept.
const MAX_BYTES: usize = 16 << 20;

/// What an image is prepared as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// A raster image of a format, smoothed or not.
    Png(bool),
    Jpeg(bool),
    Gif(bool),
    Webp(bool),
    /// A raster image that krilla cannot read, decoded (see [`crate::decode::decoded`]).
    Decoded(fagus_layout::ImageFormat, bool),
    /// A raster image as PDF/A-1 can take it (see [`crate::decode::flat`]).
    Flat(fagus_layout::ImageFormat),
    /// A raster image as a JPEG of a quality where its pixels allow one (see
    /// [`crate::lossy::image`]), and otherwise as without the quality: as its format is prepared,
    /// or as PDF/A-1 can take it (`flat`).
    Lossy {
        format: fagus_layout::ImageFormat,
        quality: u8,
        interpolate: bool,
        flat: bool,
    },
    /// An SVG document, parsed to be drawn or only for its size.
    Svg {
        draw: bool,
    },
}

/// A prepared image.
#[derive(Clone)]
pub(crate) enum Prepared {
    Raster(Option<krilla::image::Image>),
    Svg(Option<Arc<usvg::Tree>>),
}

/// The fonts an SVG with text is parsed with: the data of the faces, in order, and the family
/// of the first.
#[derive(Default, PartialEq)]
struct Fonts {
    data: Vec<Arc<Vec<u8>>>,
    first_family: Option<String>,
}

impl Fonts {
    fn of(fonts: &FontDb) -> Self {
        let mut data: Vec<Arc<Vec<u8>>> = Vec::new();
        for face in fonts.faces() {
            if !data.iter().any(|known| Arc::ptr_eq(known, &face.data)) {
                data.push(Arc::clone(&face.data));
            }
        }
        Self {
            data,
            first_family: fonts.faces().next().map(|face| face.family.clone()),
        }
    }

    fn same(&self, other: &Self) -> bool {
        self.data.len() == other.data.len()
            && self.data.iter().zip(&other.data).all(|(a, b)| Arc::ptr_eq(a, b))
            && self.first_family == other.first_family
    }
}

struct Entry {
    hash: u64,
    data: Arc<Vec<u8>>,
    kind: Kind,
    fonts: Fonts,
    prepared: Prepared,
}

/// Images prepared before, shared by the clones of an engine.
#[derive(Clone, Default)]
pub struct Images(Arc<Mutex<Vec<Entry>>>, Arc<Mutex<Vec<SvgFaces>>>);

/// The faces of the `@font-face` rules of an SVG image, by the image's data: the text of the
/// image takes them, as in Chrome, where the image is a document of its own.
struct SvgFaces {
    hash: u64,
    data: Arc<Vec<u8>>,
    faces: Arc<[Arc<fagus_text::Face>]>,
}

impl std::fmt::Debug for Images {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.0.lock().unwrap_or_else(PoisonError::into_inner).len();
        let faces = self.1.lock().unwrap_or_else(PoisonError::into_inner).len();
        f.debug_struct("Images")
            .field("entries", &entries)
            .field("svg_faces", &faces)
            .finish()
    }
}

fn hash(data: &[u8]) -> u64 {
    let mut hasher = FxHasher::default();
    data.hash(&mut hasher);
    hasher.finish()
}

impl Images {
    /// The image prepared from `data` as `kind`: the one kept, or the one `prepare` makes, which
    /// is then kept. `fonts` are those an SVG with text is parsed with.
    pub(crate) fn get_or_prepare(
        &self,
        data: &Arc<Vec<u8>>,
        kind: Kind,
        fonts: Option<&FontDb>,
        prepare: impl FnOnce() -> Prepared,
    ) -> Prepared {
        if data.len() > MAX_BYTES / 4 {
            return prepare();
        }
        let hash = hash(data);
        let fonts = fonts.map(Fonts::of).unwrap_or_default();
        // The entry of the image, moved to the end: the most recently used come last.
        let kept = |entries: &mut Vec<Entry>| {
            let index = entries.iter().position(|entry| {
                entry.hash == hash
                    && entry.kind == kind
                    && (Arc::ptr_eq(&entry.data, data) || entry.data == *data)
                    && entry.fonts.same(&fonts)
            })?;
            let entry = entries.remove(index);
            let prepared = entry.prepared.clone();
            entries.push(entry);
            Some(prepared)
        };
        if let Some(prepared) = kept(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner)) {
            return prepared;
        }
        // Prepared without the lock: other threads go on meanwhile (and may prepare it too).
        let prepared = prepare();
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(prepared) = kept(&mut entries) {
            return prepared;
        }
        entries.push(Entry {
            hash,
            data: Arc::clone(data),
            kind,
            fonts,
            prepared: prepared.clone(),
        });
        let mut bytes: usize = entries.iter().map(|entry| entry.data.len()).sum();
        while entries.len() > MAX_ENTRIES || bytes > MAX_BYTES {
            bytes -= entries.remove(0).data.len();
        }
        prepared
    }

    /// Keeps the faces of the `@font-face` rules of the SVG image `data`, which its text takes
    /// (see [`crate::svg::parse_kept`]); the faces of the images given last are kept.
    pub fn add_svg_faces(&self, data: &Arc<Vec<u8>>, faces: Vec<Arc<fagus_text::Face>>) {
        let hash = hash(data);
        let mut kept = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        kept.retain(|known| !(known.hash == hash && *known.data == **data));
        kept.push(SvgFaces {
            hash,
            data: Arc::clone(data),
            faces: Arc::from(faces),
        });
        if kept.len() > MAX_ENTRIES {
            kept.remove(0);
        }
    }

    /// The faces of the `@font-face` rules of the SVG image `data`, if it has any.
    pub fn svg_faces(&self, data: &[u8]) -> Option<Arc<[Arc<fagus_text::Face>]>> {
        let kept = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.is_empty() {
            return None;
        }
        let hash = hash(data);
        kept.iter()
            .find(|known| known.hash == hash && *known.data == data)
            .map(|known| Arc::clone(&known.faces))
    }

    /// The number of images kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(images: &Images, data: &Arc<Vec<u8>>, kind: Kind, calls: &mut usize) -> bool {
        let prepared = images.get_or_prepare(data, kind, None, || {
            *calls += 1;
            Prepared::Svg(None)
        });
        matches!(prepared, Prepared::Svg(None))
    }

    #[test]
    fn keeps_images_by_their_data_and_kind() {
        let images = Images::default();
        let mut calls = 0;
        let logo = Arc::new(b"<svg/>".to_vec());
        assert!(prepared(&images, &logo, Kind::Svg { draw: false }, &mut calls));
        // The same data, from another document.
        let again = Arc::new(b"<svg/>".to_vec());
        assert!(prepared(&images, &again, Kind::Svg { draw: false }, &mut calls));
        assert_eq!(calls, 1);
        // Prepared otherwise, or other data.
        assert!(prepared(&images, &logo, Kind::Svg { draw: true }, &mut calls));
        assert!(prepared(
            &images,
            &Arc::new(b"<svg></svg>".to_vec()),
            Kind::Svg { draw: true },
            &mut calls
        ));
        assert_eq!(calls, 3);
        assert_eq!(images.len(), 3);
    }

    #[test]
    fn tells_svgs_apart_by_their_fonts() {
        let images = Images::default();
        let svg = Arc::new(b"<svg><text>A</text></svg>".to_vec());
        let mut calls = 0;
        let mut prepare = |fonts: &FontDb| {
            images.get_or_prepare(&svg, Kind::Svg { draw: true }, Some(fonts), || {
                calls += 1;
                Prepared::Svg(None)
            });
        };
        let mut fonts = FontDb::new();
        fonts.add_default_fonts();
        prepare(&fonts);
        prepare(&fonts.clone());
        let mut more = fonts.clone();
        more.add_font_data(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fonts/Inter-Italic.ttf"
            ))
            .expect("font"),
        )
        .expect("faces");
        prepare(&more);
        assert_eq!(
            calls, 2,
            "the same fonts find the SVG, other fonts parse it again"
        );
    }

    #[test]
    fn keeps_the_images_used_last() {
        let images = Images::default();
        let mut calls = 0;
        let data: Vec<Arc<Vec<u8>>> = (0..=MAX_ENTRIES)
            .map(|number| Arc::new(format!("<svg id='{number}'/>").into_bytes()))
            .collect();
        for data in &data {
            prepared(&images, data, Kind::Png(true), &mut calls);
        }
        assert_eq!(images.len(), MAX_ENTRIES);
        // The first was dropped, the last is kept.
        prepared(&images, &data[MAX_ENTRIES], Kind::Png(true), &mut calls);
        assert_eq!(calls, MAX_ENTRIES + 1);
        prepared(&images, &data[0], Kind::Png(true), &mut calls);
        assert_eq!(calls, MAX_ENTRIES + 2);
    }
}
