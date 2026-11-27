//! Merging finished PDF files into one (see [`Merge`]).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use fagus_pdf::{MergeInput, Metadata, PageRanges};

use crate::{Date, Error, Failure, Limits, Output, Timings};

/// PDF files to merge into one: their pages, or some of them, one after another, as for printing
/// a batch of invoices or an invoice with its terms. The PDFs can come from Fagus or from any
/// other program.
///
/// ```no_run
/// use fagus::{Engine, Document, Merge, PageRanges};
///
/// let engine = Engine::new();
/// let invoice = engine.render(&Document::html("<h1>Rechnung</h1>")).unwrap().pdf;
/// let terms = std::fs::read("agb.pdf").unwrap();
/// let output = Merge::new()
///     .add(invoice)
///     .add_pages(terms, "1-2".parse::<PageRanges>().unwrap())
///     .title("Rechnung 2026-0042")
///     .write()
///     .unwrap();
/// std::fs::write("rechnung.pdf", output.pdf).unwrap();
/// ```
///
/// Each page is taken over as it is: its content, fonts and images, its size, crop box and
/// rotation. Not taken over are the annotations of the pages (links, form fields, comments,
/// signatures) and what belongs to a document as a whole: its tags, outline, attachments (the
/// invoice XML of Factur-X), form, signatures, layers and metadata. The merged PDF is neither
/// PDF/A nor PDF/UA. Encrypted PDFs cannot be merged.
///
/// Without [`Merge::created`], the PDF has no creation date, and the same PDFs give the same
/// bytes.
#[derive(Clone, Default)]
pub struct Merge {
    inputs: Vec<MergeInput>,
    metadata: Metadata,
    limits: Option<Limits>,
}

impl std::fmt::Debug for Merge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The sizes of the PDFs, not their bytes.
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .map(|input| match &input.pages {
                Some(pages) => format!("{} bytes, pages {pages}", input.data.len()),
                None => format!("{} bytes", input.data.len()),
            })
            .collect();
        formatter
            .debug_struct("Merge")
            .field("inputs", &inputs)
            .field("metadata", &self.metadata)
            .field("limits", &self.limits)
            .finish()
    }
}

impl Merge {
    /// A merge without PDFs yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds every page of the PDF `pdf`.
    #[must_use]
    #[allow(
        clippy::should_implement_trait,
        reason = "a builder method, named as in the PHP package"
    )]
    pub fn add(mut self, pdf: impl Into<Arc<Vec<u8>>>) -> Self {
        self.inputs.push(MergeInput {
            data: pdf.into(),
            pages: None,
        });
        self
    }

    /// Adds the pages `pages` of the PDF `pdf`, in their order (see [`PageRanges`]).
    #[must_use]
    pub fn add_pages(mut self, pdf: impl Into<Arc<Vec<u8>>>, pages: PageRanges) -> Self {
        self.inputs.push(MergeInput {
            data: pdf.into(),
            pages: Some(pages),
        });
        self
    }

    /// The title of the merged PDF, which viewers show in place of the file name.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    /// Adds an author of the merged PDF.
    #[must_use]
    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.metadata.authors.push(author.into());
        self
    }

    /// The language of the merged PDF, such as `de-DE`.
    #[must_use]
    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.metadata.language = Some(language.into());
        self
    }

    /// The creation date of the merged PDF. Without one, the PDF has none.
    #[must_use]
    pub fn created(mut self, date: Date) -> Self {
        self.metadata.created = Some(date);
        self
    }

    /// The memory budget and the time limit of the merge ([`Limits::new`] if not set): the PDFs,
    /// and the content of their pages decoded, count against the budget.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// How many PDFs are added.
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    /// Whether no PDF is added.
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Merges the PDFs, on this thread: the [`Output`] has the PDF, its number of pages and the
    /// time it took (`timings.pdf`). Fails with [`crate::PdfError::NothingToMerge`],
    /// [`crate::PdfError::Unreadable`], [`crate::PdfError::Encrypted`] or
    /// [`crate::PdfError::NoPage`], or when the budget or the time runs out.
    pub fn write(&self) -> Result<Output, Error> {
        let start = Instant::now();
        let limits = self.limits.unwrap_or_default();
        let budget = fagus_dom::budget::Budget::new(limits.memory_budget(), limits.time_limit(), start);
        let merged = std::panic::catch_unwind(AssertUnwindSafe(|| {
            fagus_dom::budget::run(budget, || fagus_pdf::merge::merge(&self.inputs, &self.metadata))
        }))
        .map_err(|panic| {
            let message = panic
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".to_owned());
            Error::Internal(message)
        })?
        .map_err(|exceeded| Failure::exceeded(exceeded).error)??;
        Ok(Output {
            pdf: merged.pdf,
            pages: merged.pages,
            timings: Timings {
                pdf: start.elapsed(),
                ..Timings::default()
            },
            ..Output::default()
        })
    }
}

/// Merges the PDFs `pdfs`, every page of each, one after another (see [`Merge`]).
///
/// ```no_run
/// let invoices = [std::fs::read("1.pdf").unwrap(), std::fs::read("2.pdf").unwrap()];
/// let output = fagus::merge(invoices).unwrap();
/// std::fs::write("invoices.pdf", output.pdf).unwrap();
/// ```
pub fn merge<I>(pdfs: I) -> Result<Output, Error>
where
    I: IntoIterator,
    I::Item: Into<Arc<Vec<u8>>>,
{
    pdfs.into_iter().fold(Merge::new(), Merge::add).write()
}
