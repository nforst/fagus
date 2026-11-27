//! Merging PDF files: the pages of several PDFs, or some of them, one after another in a new PDF.
//!
//! krilla takes each page over as a page of its own (`Document::embed_pdf_pages`, which extracts
//! it with hayro-write): its content stream (decoded and compressed again), its resources (fonts,
//! images, patterns, graphics states, with what they refer to), its transparency group, its media
//! box, crop box and `/Rotate`. A page is therefore as large and turned as it was, and its text
//! stays text. What belongs to the page beyond that is not taken over: its annotations (links,
//! form fields, comments, the appearance of signatures), its trim, bleed and art boxes, its
//! `/UserUnit`, and the entries hayro-write leaves out (`/StructParents`, `/OC` of `XObject`s,
//! `/Metadata`, `/AF`, `/PieceInfo`). Nor is what belongs to the document: its structure tree
//! (tags), outline, named destinations, attachments (the invoice XML of Factur-X), interactive
//! form, signatures, page labels, optional content (layers, which then all show), document
//! metadata and how it opens. The PDF that comes out is an ordinary PDF, neither PDF/A nor PDF/UA.
//!
//! Every page that is to be taken over is checked first as PDF images are: krilla cannot write a
//! page whose objects cannot be read or that refers to objects that do not exist, and decodes its
//! content, which may be no larger than Fagus decodes for an image.

use std::fmt::Write as _;
use std::str::FromStr;
use std::sync::Arc;

use hayro_syntax::{LoadPdfError, Pdf, PdfVersion};

use crate::pdf_image::{self, Extraction, PdfImageError};
use crate::{Metadata, PdfError, compact};

/// A password no PDF has: a PDF that opens without a password but not with this one is encrypted
/// (with an owner password only).
const PROBE: &str = "\u{1}fagus\u{1}no password\u{1}";

/// Pages of a PDF, as ranges of page numbers from 1, in the order they are to be taken: `1-3,5`
/// for the pages 1, 2, 3 and 5, `2-` for the second page to the last.
///
/// ```
/// use fagus_pdf::PageRanges;
///
/// let pages: PageRanges = "1-3, 5, 8-".parse().unwrap();
/// assert_eq!(pages.to_string(), "1-3,5,8-");
/// assert!("0".parse::<PageRanges>().is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRanges {
    /// The first and last page of each range, from 1; no last page for a range to the end.
    ranges: Vec<(usize, Option<usize>)>,
}

impl PageRanges {
    /// Every page, `1-`.
    pub fn all() -> Self {
        Self {
            ranges: vec![(1, None)],
        }
    }

    /// The pages `first` to `last`, from 1.
    pub fn range(first: usize, last: usize) -> Result<Self, PdfError> {
        Self::parse(&format!("{first}-{last}"))
    }

    /// The pages of a selection such as `1-3,5,8-`: page numbers from 1 and ranges of them,
    /// separated by commas. A range is `first-last` or `first-` to the last page; spaces around
    /// numbers and commas are allowed.
    pub fn parse(text: &str) -> Result<Self, PdfError> {
        let invalid = |reason: &str| PdfError::PageRanges {
            text: text.to_owned(),
            reason: reason.to_owned(),
        };
        let number = |part: &str| -> Result<usize, PdfError> {
            let part = part.trim();
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("expected page numbers and ranges like `1-3,5,8-`"));
            }
            match part.parse::<usize>() {
                Ok(0) => Err(invalid("pages count from 1")),
                Ok(page) => Ok(page),
                Err(_) => Err(invalid("a page number is too large")),
            }
        };
        let mut ranges = Vec::new();
        for item in text.split(',') {
            let range = match item.split_once('-') {
                None => {
                    let page = number(item)?;
                    (page, Some(page))
                }
                Some((first, last)) if last.trim().is_empty() => (number(first)?, None),
                Some((first, last)) => {
                    let (first, last) = (number(first)?, number(last)?);
                    if last < first {
                        return Err(invalid("a range ends before it begins"));
                    }
                    (first, Some(last))
                }
            };
            ranges.push(range);
        }
        Ok(Self { ranges })
    }

    /// The pages, from 0, of a PDF of `count` pages; the first page number it does not have if
    /// any.
    fn indices(&self, count: usize) -> Result<Vec<usize>, usize> {
        let mut indices = Vec::new();
        for &(first, last) in &self.ranges {
            let last = last.unwrap_or(count);
            for page in [first, last] {
                if page > count {
                    return Err(page);
                }
            }
            indices.extend(first - 1..last);
        }
        Ok(indices)
    }
}

impl FromStr for PageRanges {
    type Err = PdfError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl std::fmt::Display for PageRanges {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut text = String::new();
        for (index, &(first, last)) in self.ranges.iter().enumerate() {
            if index > 0 {
                text.push(',');
            }
            let _ = match last {
                Some(last) if last == first => write!(text, "{first}"),
                Some(last) => write!(text, "{first}-{last}"),
                None => write!(text, "{first}-"),
            };
        }
        formatter.write_str(&text)
    }
}

/// A PDF to merge: its data and the pages to take of it (all if `None`).
#[derive(Clone, Debug)]
pub struct MergeInput {
    pub data: Arc<Vec<u8>>,
    pub pages: Option<PageRanges>,
}

/// A merged PDF.
#[derive(Debug)]
pub struct Merged {
    pub pdf: Vec<u8>,
    /// How many pages it has.
    pub pages: usize,
}

/// Merges the pages of `inputs` into one PDF with `metadata` (the title, authors, language,
/// creation date and so on of the new document; nothing is taken from the inputs). The PDF is
/// PDF 1.7, or PDF 2.0 if an input is.
pub fn merge(inputs: &[MergeInput], metadata: &Metadata) -> Result<Merged, PdfError> {
    if inputs.is_empty() {
        return Err(PdfError::NothingToMerge);
    }
    let mut opened = Vec::with_capacity(inputs.len());
    let mut version = krilla::configure::PdfVersion::Pdf17;
    for (index, input) in inputs.iter().enumerate() {
        fagus_dom::budget::check_time_now();
        fagus_dom::budget::charge(fagus_dom::budget::Kind::Resources, input.data.len() as u64);
        let (pdf, pages) = open(index, input)?;
        if pdf.version() > PdfVersion::Pdf17 {
            version = krilla::configure::PdfVersion::Pdf20;
        }
        opened.push((Arc::new(pdf), pages));
    }
    let configuration = krilla::configure::ConfigurationBuilder::new()
        .with_version(version)
        .finish()
        .map_err(|_| PdfError::Write(format!("PDF {} cannot be written", version.as_str())))?;
    let settings = krilla::SerializeSettings {
        enable_tagging: false,
        configuration,
        ..krilla::SerializeSettings::default()
    };
    let mut document = krilla::Document::new_with(settings);
    let mut count = 0;
    for (pdf, pages) in opened {
        count += pages.len();
        document.embed_pdf_pages(&krilla::pdf::PdfDocument::new(pdf), &pages);
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
    fagus_dom::budget::check_time_now();
    let pdf = document
        .finish()
        .map_err(|error| PdfError::Write(error.to_string()))?;
    // A viewer shows the title in place of the file name, as with the PDFs Fagus renders.
    let catalog: &[(&str, &str)] = if metadata.title.is_some() {
        &[("/ViewerPreferences", "<</DisplayDocTitle true>>")]
    } else {
        &[]
    };
    let pdf = compact::compact(&pdf, catalog, None).unwrap_or(pdf);
    Ok(Merged { pdf, pages: count })
}

/// The PDF of `input` (the `index`th), opened, and the pages to take of it, from 0, each checked
/// as krilla will extract it.
fn open(index: usize, input: &MergeInput) -> Result<(Pdf, Vec<usize>), PdfError> {
    let unreadable = |reason: &str| PdfError::Unreadable {
        input: index,
        reason: reason.to_owned(),
    };
    if !pdf_image::is_pdf(&input.data) {
        return Err(unreadable("it is not a PDF file (it has no `%PDF-` header)"));
    }
    let pdf = Pdf::new(Arc::clone(&input.data)).map_err(|error| match error {
        LoadPdfError::Decryption(_) => PdfError::Encrypted { input: index },
        LoadPdfError::Invalid => unreadable("its structure cannot be read"),
    })?;
    // One that opens without a password but is encrypted all the same restricts what may be
    // done with it (its owner password lifts the restrictions); a merge would drop them.
    if let Err(LoadPdfError::Decryption(_)) = Pdf::new_with_password(Arc::clone(&input.data), PROBE) {
        return Err(PdfError::Encrypted { input: index });
    }
    let count = pdf.pages().len();
    if count == 0 {
        return Err(unreadable("it has no pages"));
    }
    // hayro-syntax repairs a broken cross-reference table the first time it fails to read an
    // object through it, which changes what later reads find: reading every object once does
    // that now, so that the checks find what krilla finds (see `pdf_image::open`).
    for _ in pdf.objects() {}
    let pages = input
        .pages
        .as_ref()
        .map_or_else(|| Ok((0..count).collect()), |pages| pages.indices(count))
        .map_err(|page| PdfError::NoPage {
            input: index,
            page,
            pages: count,
        })?;
    let mut checked = rustc_hash::FxHashSet::default();
    for &page in &pages {
        if !checked.insert(page) {
            continue;
        }
        fagus_dom::budget::check_time_now();
        let content = pdf_image::content_size(pdf.pages()[page].raw()).map_err(|error| match error {
            PdfImageError::TooLarge => unreadable(&format!(
                "page {} holds more content than Fagus decodes (128 MiB)",
                page + 1
            )),
            _ => unreadable(&format!("the content of page {} cannot be read", page + 1)),
        })?;
        // krilla decodes the content and compresses it again.
        fagus_dom::budget::charge(fagus_dom::budget::Kind::Resources, content);
        if !pdf_image::readable(&pdf, page, Extraction::Page)
            || !pdf_image::complete(&pdf, page, Extraction::Page)
        {
            return Err(unreadable(&format!(
                "page {} refers to objects that cannot be read or do not exist",
                page + 1
            )));
        }
    }
    Ok((pdf, pages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_page_ranges() {
        let pages = |text: &str| PageRanges::parse(text).map(|pages| pages.indices(10));
        assert_eq!(pages("1").unwrap(), Ok(vec![0]));
        assert_eq!(pages(" 2 - 4 , 1 ").unwrap(), Ok(vec![1, 2, 3, 0]));
        assert_eq!(pages("9-").unwrap(), Ok(vec![8, 9]));
        assert_eq!(pages("3,3").unwrap(), Ok(vec![2, 2]));
        assert_eq!(pages("4-11").unwrap(), Err(11));
        assert_eq!(pages("12-").unwrap(), Err(12));
        assert_eq!(PageRanges::all().indices(3), Ok(vec![0, 1, 2]));
        assert_eq!(PageRanges::range(2, 3).unwrap().indices(3), Ok(vec![1, 2]));
        for (text, reason) in [
            ("", "expected page numbers and ranges like `1-3,5,8-`"),
            ("1,,2", "expected page numbers and ranges like `1-3,5,8-`"),
            ("-3", "expected page numbers and ranges like `1-3,5,8-`"),
            ("1-2-3", "expected page numbers and ranges like `1-3,5,8-`"),
            ("+1", "expected page numbers and ranges like `1-3,5,8-`"),
            ("a", "expected page numbers and ranges like `1-3,5,8-`"),
            ("0", "pages count from 1"),
            ("0-2", "pages count from 1"),
            ("3-2", "a range ends before it begins"),
            ("99999999999999999999999", "a page number is too large"),
        ] {
            let Err(PdfError::PageRanges {
                text: given,
                reason: found,
            }) = PageRanges::parse(text)
            else {
                panic!("`{text}` was read");
            };
            assert_eq!((given.as_str(), found.as_str()), (text, reason), "{text}");
        }
        assert_eq!(
            PageRanges::parse("1-3, 5,5-5, 8-").unwrap().to_string(),
            "1-3,5,5,8-"
        );
    }
}
