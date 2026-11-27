//! PDF files as images, as Safari and Prince show them (Chrome shows none): a page of the file,
//! drawn as a form `XObject` with its text, vector graphics, fonts and images as they are. This
//! is how a letterhead designed elsewhere becomes the background of the pages
//! (`@page { background: url(letterhead.pdf) }`).
//!
//! The page is the first, or the one the fragment of the URL names (`letterhead.pdf#page=2`,
//! RFC 8118). The image is as large as the page's crop box, with its `/Rotate` and `/UserUnit`,
//! a point (1/72 inch) for 4/3 CSS pixels. Links and other annotations of the page are not
//! taken over, nor its structure tree.

use std::io::Read;
use std::sync::Arc;

use fagus_layout::{Image, ImageFormat};
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, ObjRef, Object, Stream};
use hayro_syntax::{LoadPdfError, Pdf, PdfVersion};

/// The largest PDF file shown as an image.
pub const MAX_BYTES: usize = 128 << 20;

/// The most bytes the content stream of a page may hold once decoded: krilla decodes it to
/// write the page as a form `XObject`.
const MAX_CONTENT_BYTES: u64 = 128 << 20;

/// The most compressed bytes of a content stream with other filters than Flate (LZW, run
/// length …), which Fagus cannot measure before decoding: decoded, they hold at most a few
/// thousand times as much.
const MAX_OTHER_FILTERED_BYTES: usize = 32 << 10;

/// Why a PDF file cannot be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfImageError {
    /// The file is not a PDF that can be read.
    Broken,
    /// The file needs a password to be opened.
    Password,
    /// The file is larger than [`MAX_BYTES`], or the page's content than Fagus decodes.
    TooLarge,
    /// The file is PDF 2.0, newer than the PDF 1.7 Fagus writes.
    Version,
    /// The fragment names a page the file does not have (the page, from 1, and how many there
    /// are).
    NoPage(u32, usize),
}

/// Whether `data` is a PDF file: its header (`%PDF-`) within the first 1024 bytes, as viewers
/// find it.
pub fn is_pdf(data: &[u8]) -> bool {
    data.get(..data.len().min(1024))
        .is_some_and(|start| start.windows(5).any(|window| window == b"%PDF-"))
}

/// The page that the fragment of a URL (what follows `#`) names, from 0: `page=N` among the
/// parameters of RFC 8118 (`#page=2&zoom=50`), the first page without one or with one that is no
/// page number.
pub fn page_of(fragment: Option<&str>) -> u32 {
    fragment
        .into_iter()
        .flat_map(|fragment| fragment.split('&'))
        .filter_map(|parameter| parameter.split_once('='))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("page"))
        .find_map(|(_, value)| value.trim().parse::<u32>().ok().filter(|page| *page >= 1))
        .map_or(0, |page| page - 1)
}

/// The image of the page of the PDF `data` that `fragment` names (see [`page_of`]): its size in
/// CSS pixels.
pub fn probe(data: Arc<Vec<u8>>, fragment: Option<&str>) -> Result<Image, PdfImageError> {
    if data.len() > MAX_BYTES {
        return Err(PdfImageError::TooLarge);
    }
    let page = page_of(fragment);
    let pdf = open(&data)?;
    let pages = pdf.pages();
    let Some(chosen) = pages.get(page as usize) else {
        return Err(PdfImageError::NoPage(page + 1, pages.len()));
    };
    content_size(chosen.raw())?;
    if !readable(&pdf, page as usize, Extraction::XObject)
        || !complete(&pdf, page as usize, Extraction::XObject)
    {
        return Err(PdfImageError::Broken);
    }
    let (width, height) = chosen.render_dimensions();
    // A user unit makes the units of the page larger than a point (PDF 1.6).
    let unit = chosen
        .raw()
        .get::<f32>(b"UserUnit")
        .filter(|unit| unit.is_finite() && *unit > 0.0 && *unit <= 75_000.0)
        .unwrap_or(1.0);
    let (width, height) = (width * unit * 96.0 / 72.0, height * unit * 96.0 / 72.0);
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return Err(PdfImageError::Broken);
    }
    Ok(Image {
        data,
        format: ImageFormat::Pdf(page),
        width,
        height,
        natural: (true, true),
        ratio: Some(width / height),
        inline: false,
        orientation: 1,
        filters: Arc::from([]),
        pad: 0,
    })
}

/// The PDF file `data`, opened as krilla embeds it: one that needs no password (one its owner
/// only restricts opens without one), of PDF 1.7 at most.
pub(crate) fn open(data: &Arc<Vec<u8>>) -> Result<Pdf, PdfImageError> {
    // hayro-syntax panics on a few broken encryption dictionaries (a key longer than its
    // revision allows): such a file is broken.
    let pdf = fagus_css::guarded(|| Pdf::new(Arc::clone(data)))
        .unwrap_or(Err(LoadPdfError::Invalid))
        .map_err(|error| match error {
            LoadPdfError::Decryption(_) => PdfImageError::Password,
            LoadPdfError::Invalid => PdfImageError::Broken,
        })?;
    if pdf.version() > PdfVersion::Pdf17 {
        return Err(PdfImageError::Version);
    }
    if pdf.pages().is_empty() {
        return Err(PdfImageError::Broken);
    }
    // hayro-syntax repairs a broken cross-reference table the first time it fails to read an
    // object through it, which changes what later reads find. Reading every object once does
    // that now: what [`complete`] finds for a page is then what krilla finds as it writes it.
    for _ in pdf.objects() {}
    Ok(pdf)
}

/// How krilla takes a page of another PDF: as a form `XObject` (a PDF image) or as a page of its
/// own (a merge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Extraction {
    XObject,
    Page,
}

/// Whether every object that krilla's extraction of the page `page` of `pdf` reads can be read
/// as it reads it: hayro-write takes the entries of a dictionary that cannot be read, and a
/// stream where a direct object stands, for granted. It reads the page's resources, those of
/// the pages above it, and what they refer to, as [`complete`] does; a page of its own also its
/// transparency group.
pub(crate) fn readable(pdf: &Pdf, page: usize, extraction: Extraction) -> bool {
    // The keys whose values hayro-write leaves out (its `IGNORE_KEYS`).
    const IGNORED: [&[u8]; 10] = [
        b"Metadata",
        b"StructParent",
        b"OC",
        b"AF",
        b"PtData",
        b"Ref",
        b"LastModified",
        b"PieceInfo",
        b"StructParents",
        b"OPI",
    ];
    fn dict<'a>(dict: &Dict<'a>, stream: bool, pending: &mut Vec<MaybeRef<Object<'a>>>) -> bool {
        for key in dict.keys() {
            let Some(value) = dict.get_raw::<Object<'_>>(key.as_ref()) else {
                return false;
            };
            let skipped = IGNORED.contains(&key.as_ref()) || (stream && key.as_ref() == b"Length");
            if !skipped {
                pending.push(value);
            }
        }
        true
    }
    let Some(chosen) = pdf.pages().get(page) else {
        return false;
    };
    let mut pending: Vec<MaybeRef<Object<'_>>> = Vec::new();
    let mut resources = Some(chosen.resources());
    while let Some(level) = resources {
        for kind in [
            &level.ext_g_states,
            &level.shadings,
            &level.patterns,
            &level.x_objects,
            &level.color_spaces,
            &level.fonts,
            &level.properties,
        ] {
            if !dict(kind, false, &mut pending) {
                return false;
            }
        }
        resources = level.parent();
    }
    if extraction == Extraction::Page
        && let Some(group) = chosen.raw().get_raw::<Object<'_>>(b"Group")
    {
        pending.push(group);
    }
    let mut visited: rustc_hash::FxHashSet<ObjRef> = rustc_hash::FxHashSet::default();
    while let Some(value) = pending.pop() {
        let (object, indirect) = match value {
            MaybeRef::NotRef(object) => (object, false),
            MaybeRef::Ref(reference) => {
                if !visited.insert(reference) {
                    continue;
                }
                // A missing object is written as `null`.
                let Some(object) = pdf.xref().get::<Object<'_>>(reference.into()) else {
                    continue;
                };
                (object, true)
            }
        };
        let read = match &object {
            Object::Dict(entries) => dict(entries, false, &mut pending),
            Object::Array(items) => {
                pending.extend(items.raw_iter());
                true
            }
            Object::Stream(stream) => indirect && dict(stream.dict(), true, &mut pending),
            _ => true,
        };
        if !read {
            return false;
        }
    }
    true
}

/// Whether the page `page` of `pdf`, extracted as krilla extracts it, refers only to objects that
/// exist. krilla cannot write one that refers to missing objects (a reference to an object that
/// is not in the file, which readers take as `null`). A page of its own refers to the page tree
/// as well, which krilla writes.
pub(crate) fn complete(pdf: &Pdf, page: usize, extraction: Extraction) -> bool {
    let mut next = pdf_writer::Ref::new(1);
    let Ok(extracted) = hayro_write::extract(
        pdf,
        Box::new(|| next.bump()),
        hayro_write::ChunkSettings { pretty: false },
        |group| {
            group.color_space().device_rgb();
        },
        &[match extraction {
            Extraction::XObject => hayro_write::ExtractionQuery::new_xobject(page),
            Extraction::Page => hayro_write::ExtractionQuery::new_page(page),
        }],
    ) else {
        return false;
    };
    if !extracted.root_refs.iter().all(Result::is_ok) {
        return false;
    }
    let mut written: rustc_hash::FxHashSet<pdf_writer::Ref> = extracted.chunk.refs().collect();
    if extraction == Extraction::Page {
        written.insert(extracted.page_tree_parent_ref);
    }
    let mut missing = false;
    let _ = extracted.chunk.renumber(|old| {
        missing |= !written.contains(&old);
        old
    });
    !missing
}

/// How many bytes the content of the page `page` (its dictionary) holds decoded, an estimate
/// that decodes no more than [`MAX_CONTENT_BYTES`]: a content stream compressed with Flate is
/// decoded without being kept.
pub(crate) fn content_size(page: &Dict<'_>) -> Result<u64, PdfImageError> {
    content_size_within(page, MAX_CONTENT_BYTES)
}

/// [`content_size`] with a limit of `limit` bytes.
fn content_size_within(page: &Dict<'_>, limit: u64) -> Result<u64, PdfImageError> {
    let streams: Vec<Stream<'_>> = match page.get::<Stream<'_>>(b"Contents") {
        Some(stream) => vec![stream],
        None => page
            .get::<Array<'_>>(b"Contents")
            .map(|array| array.iter::<Stream<'_>>().collect())
            .unwrap_or_default(),
    };
    let mut total = 0_u64;
    for stream in &streams {
        let filters: Vec<Vec<u8>> = match stream.dict().get::<Name<'_>>(b"Filter") {
            Some(name) => vec![name.to_vec()],
            None => stream
                .dict()
                .get::<Array<'_>>(b"Filter")
                .map(|array| array.iter::<Name<'_>>().map(|name| name.to_vec()).collect())
                .unwrap_or_default(),
        };
        let raw = stream.raw_data();
        let size = match filters.as_slice() {
            [] => raw.len() as u64,
            [flate] if flate == b"FlateDecode" || flate == b"Fl" => {
                // Read to the limit, and one byte more to tell that it is passed.
                let mut decoder = flate2::read::ZlibDecoder::new(&raw[..]).take(limit + 1);
                std::io::copy(&mut decoder, &mut std::io::sink()).unwrap_or(0)
            }
            _ if raw.len() <= MAX_OTHER_FILTERED_BYTES => {
                stream.decoded().map_or(0, |decoded| decoded.len() as u64)
            }
            _ => return Err(PdfImageError::TooLarge),
        };
        total = total.saturating_add(size);
        if total > limit {
            return Err(PdfImageError::TooLarge);
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PDF file of `objects` (numbered from 1, the first the catalog), with a cross-reference
    /// table.
    pub(crate) fn pdf(version: &str, objects: &[&[u8]]) -> Vec<u8> {
        let mut out = format!("%PDF-{version}\n").into_bytes();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n", index + 1).as_bytes());
            out.extend(*object);
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn two_pages() -> Vec<u8> {
        pdf(
            "1.4",
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>",
                b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 595.28 841.89] >>",
                b"<< /Type /Page /Parent 2 0 R >>",
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 150] /CropBox [10 10 160 85] \
                   /Rotate 90 /UserUnit 2 >>",
            ],
        )
    }

    #[test]
    fn takes_an_encryption_that_hayro_cannot_read_as_broken() {
        // A key of 256 bits for a revision that allows 128 at most: hayro-syntax panics on it.
        let zeros = "0".repeat(64);
        let encrypt =
            format!("<< /Filter /Standard /V 2 /R 3 /Length 256 /O <{zeros}> /U <{zeros}> /P -4 >>");
        let data = String::from_utf8(pdf(
            "1.4",
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>",
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
                encrypt.as_bytes(),
            ],
        ))
        .expect("text")
        .replace(
            "/Root 1 0 R >>",
            &format!("/Root 1 0 R /Encrypt 4 0 R /ID [<{zeros}> <{zeros}>] >>"),
        );
        assert_eq!(
            probe(Arc::new(data.into_bytes()), None).err(),
            Some(PdfImageError::Broken)
        );
    }

    #[test]
    fn finds_the_header() {
        assert!(is_pdf(b"%PDF-1.7\n"));
        assert!(is_pdf(b"\xEF\xBB\xBF garbage %PDF-1.4"));
        assert!(!is_pdf(b"<svg/>"));
        let mut late = vec![b' '; 1024];
        late.extend(b"%PDF-1.4");
        assert!(!is_pdf(&late));
    }

    #[test]
    fn reads_the_page_of_the_fragment() {
        assert_eq!(page_of(None), 0);
        assert_eq!(page_of(Some("page=2")), 1);
        assert_eq!(page_of(Some("zoom=50&page=3")), 2);
        assert_eq!(page_of(Some("Page=4")), 3);
        assert_eq!(page_of(Some("page=0")), 0);
        assert_eq!(page_of(Some("page=x")), 0);
        assert_eq!(page_of(Some("nameddest=intro")), 0);
    }

    #[test]
    fn sizes_pages_by_their_crop_box() {
        let data = Arc::new(two_pages());
        let first = probe(Arc::clone(&data), None).expect("first page");
        assert_eq!(first.format, ImageFormat::Pdf(0));
        assert!((first.width - 595.28 * 4.0 / 3.0).abs() < 0.01, "{}", first.width);
        assert!(
            (first.height - 841.89 * 4.0 / 3.0).abs() < 0.01,
            "{}",
            first.height
        );
        // The crop box of 150 × 75 points, turned a quarter and in units of 2 points.
        let second = probe(Arc::clone(&data), Some("page=2")).expect("second page");
        assert_eq!(second.format, ImageFormat::Pdf(1));
        assert!(
            (second.width - 75.0 * 2.0 * 4.0 / 3.0).abs() < 0.01,
            "{}",
            second.width
        );
        assert!(
            (second.height - 150.0 * 2.0 * 4.0 / 3.0).abs() < 0.01,
            "{}",
            second.height
        );
        assert_eq!(
            probe(data, Some("page=3")).err(),
            Some(PdfImageError::NoPage(3, 2))
        );
    }

    #[test]
    fn refuses_what_it_cannot_show() {
        assert_eq!(
            probe(Arc::new(b"%PDF-1.4 nothing".to_vec()), None).err(),
            Some(PdfImageError::Broken)
        );
        let newer = pdf(
            "2.0",
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>",
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            ],
        );
        assert_eq!(probe(Arc::new(newer), None).err(), Some(PdfImageError::Version));
    }

    #[test]
    fn takes_references_to_missing_objects_as_null() {
        // Object 9 is not in the file: readers take it as `null`.
        let missing = pdf(
            "1.4",
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>",
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /X 9 0 R >> >> >>",
            ],
        );
        // hayro-write writes a reference to a missing object as `null`.
        assert!(probe(Arc::new(missing), None).is_ok());
    }

    #[test]
    fn measures_content_without_keeping_it() {
        use std::io::Write;
        // 3 MiB of spaces compress to a few KiB: more than a limit of 2 MiB.
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&vec![b' '; 3 << 20]).expect("compress");
        let compressed = encoder.finish().expect("compress");
        let mut stream = format!(
            "<< /Length {} /Filter /FlateDecode >>\nstream\n",
            compressed.len()
        )
        .into_bytes();
        stream.extend(&compressed);
        stream.extend(b"\nendstream");
        let data = Arc::new(pdf(
            "1.4",
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>",
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents [4 0 R 4 0 R] >>",
                &stream,
            ],
        ));
        let pdf = open(&data).expect("a PDF");
        let page = pdf.pages()[0].raw();
        assert_eq!(content_size_within(page, 8 << 20), Ok(6 << 20));
        // Each stream within the limit, both above it.
        assert_eq!(content_size_within(page, 5 << 20), Err(PdfImageError::TooLarge));
        assert_eq!(content_size_within(page, 2 << 20), Err(PdfImageError::TooLarge));
    }
}
