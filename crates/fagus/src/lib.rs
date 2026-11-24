//! HTML and CSS to PDF, without a browser.
//!
//! ```no_run
//! use fagus::{Document, Engine};
//!
//! let mut engine = Engine::new();
//! engine.add_font_dir("fonts").unwrap();
//! let output = engine.render(&Document::html("<h1>Rechnung</h1>")).unwrap();
//! std::fs::write("rechnung.pdf", output.pdf).unwrap();
//! ```

#![forbid(unsafe_code)]

mod font_face;
mod forms;
mod frames;
mod interactive;
mod layout_info;
mod limits;
mod missing_glyphs;
mod palette_values;
mod resources;
mod sheets;
mod structure;
mod svg_document;
mod svg_geometry;
mod svg_transform;

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use fagus_css::lightningcss::rules::font_face::Source;
use fagus_css::{MediaEnvironment, Origin, RuleSet, Stylesheet};
use fagus_layout::responsive;
use fagus_layout::{Image, ImageError, Images};
use fagus_pdf::Metadata;
use fagus_text::{Face, FontDb};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

pub use fagus_css::{Diagnostic, Severity};
pub use fagus_pdf::sign::{
    Algorithm, Certification, Placement, Revocation, RevocationRequest, Signature, Signer, Timestamper,
    timestamp_request,
};
pub use fagus_pdf::{Attachment, Conformance, Date, Encryption, PdfError, Permission, Relationship};
pub use fagus_text::FontError;
pub use layout_info::{Layout, LayoutBox, LayoutElement, LayoutPage, LayoutRow, RowSection};
pub use limits::Limits;
pub use resources::{BATCH, ResourceLoader};

/// An error while rendering.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Font(#[from] FontError),
    #[error(transparent)]
    Pdf(#[from] PdfError),
    /// A bug in Fagus: rendering stopped where it should not.
    #[error("internal error while rendering: {0}")]
    Internal(String),
    /// The document needed more memory than its budget ([`Limits`]), in bytes. `held` is what
    /// its parts held then, the largest first.
    #[error(
        "the document needs more memory than its budget of {} ({})",
        limits::size(*budget),
        limits::held(held)
    )]
    MemoryBudget {
        budget: u64,
        held: Vec<(&'static str, u64)>,
    },
    /// The document took longer than its time limit ([`Limits`]).
    #[error("the document took longer than its time limit of {:.1} s", limit.as_secs_f64())]
    TimeLimit { limit: Duration },
}

/// The stack of the threads that render documents. Style, layout and painting walk the document
/// recursively; with elements nested as deep as Fagus allows, that takes a few megabytes, more
/// than some threads have (1 MiB for the main thread on Windows, 2 MiB for threads Rust starts).
/// The memory is only reserved and used as the stack grows.
const RENDER_STACK: usize = 64 << 20;

/// The description of the attached invoice XML, unless another is given.
const FACTUR_X_DESCRIPTION: &str = "Factur-X/ZUGFeRD invoice";

/// A document to render.
#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag for each option of the builder"
)]
pub struct Document {
    html: String,
    /// Whether the markup is XHTML, parsed as XML.
    xhtml: bool,
    /// Directory that relative URLs resolve against, if any.
    base: Option<PathBuf>,
    /// Directory that files may be read from; the base directory if not set.
    root: Option<PathBuf>,
    stylesheets: Vec<(String, String)>,
    resources: FxHashMap<String, Arc<Vec<u8>>>,
    loader: Option<Arc<dyn ResourceLoader>>,
    conformance: Option<Conformance>,
    tagged: bool,
    accessible: bool,
    /// Whether form controls become the fields of an interactive form.
    forms: bool,
    /// The quality of the JPEGs that opaque images are written as (see
    /// [`Document::image_quality`]).
    image_quality: Option<u8>,
    attachments: Vec<Attachment>,
    factur_x: Option<FacturXProfile>,
    /// The profile the invoice XML declares, if any.
    declared_profile: Option<FacturXProfile>,
    /// The `Version` of the Factur-X XMP metadata, if not the default.
    factur_x_version: Option<String>,
    /// The description of the attached invoice XML, if not the default.
    factur_x_description: Option<String>,
    /// Where the invoice XML lies in `attachments`.
    factur_x_attachment: Option<usize>,
    created: Option<Date>,
    /// Whether to report where the elements ended up (see [`Document::report_layout`]).
    report_layout: bool,
    encryption: Option<Encryption>,
    /// The digital signature of the PDF, if any.
    signature: Option<Signature>,
    /// How deep the document lies in SVG images with HTML in them (see [`svg_document`]): 0
    /// for the document itself.
    svg_depth: u8,
    /// Where the document lies in frames (see [`frames`]).
    nesting: frames::Nesting,
    /// Its memory budget and time limit, if not the engine's.
    limits: Option<Limits>,
    /// The resources a render of it has loaded, shared with the documents of its frames.
    loaded: Arc<resources::Loaded>,
}

/// The profile of a ZUGFeRD 2 or Factur-X invoice, as named in its XML.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FacturXProfile {
    Minimum,
    BasicWl,
    Basic,
    En16931,
    Extended,
    XRechnung,
}

impl FacturXProfile {
    /// The profile an invoice XML declares in its guideline parameter
    /// (`GuidelineSpecifiedDocumentContextParameter`), for Factur-X and ZUGFeRD 2.
    pub fn detect(xml: &[u8]) -> Option<Self> {
        let xml = std::str::from_utf8(xml).ok()?;
        let mut rest = &xml[xml.find("GuidelineSpecifiedDocumentContextParameter")?..];
        // The parameter's `ID` element, whatever its namespace prefix.
        let id = loop {
            rest = &rest[rest.find('<')? + 1..];
            let end = rest.find('>')?;
            let name = rest[..end].split_ascii_whitespace().next()?;
            rest = &rest[end + 1..];
            if name.rsplit(':').next() == Some("ID") {
                break rest[..rest.find('<')?].trim().to_ascii_lowercase();
            }
        };
        Some(if id.contains("xrechnung") {
            Self::XRechnung
        } else if id.ends_with("extended") {
            Self::Extended
        } else if id.ends_with("basicwl") {
            Self::BasicWl
        } else if id.ends_with("basic") {
            Self::Basic
        } else if id.ends_with("minimum") {
            Self::Minimum
        } else if id.starts_with("urn:cen.eu:en16931:2017") {
            Self::En16931
        } else {
            return None;
        })
    }

    /// The `ConformanceLevel` of the Factur-X metadata.
    fn level(self) -> &'static str {
        match self {
            Self::Minimum => "MINIMUM",
            Self::BasicWl => "BASIC WL",
            Self::Basic => "BASIC",
            Self::En16931 => "EN 16931",
            Self::Extended => "EXTENDED",
            Self::XRechnung => "XRECHNUNG",
        }
    }

    /// The name the XML must have inside the PDF.
    fn file_name(self) -> &'static str {
        match self {
            Self::XRechnung => "xrechnung.xml",
            _ => "factur-x.xml",
        }
    }

    /// The profiles below BASIC do not carry a full invoice: their XML is data for the PDF, not
    /// an alternative to it.
    fn relationship(self) -> Relationship {
        match self {
            Self::Minimum | Self::BasicWl => Relationship::Data,
            _ => Relationship::Alternative,
        }
    }
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("base", &self.base)
            .field("root", &self.root)
            .field("stylesheets", &self.stylesheets.len())
            .field("resources", &self.resources.keys().collect::<Vec<_>>())
            .field("loader", &self.loader.is_some())
            .finish_non_exhaustive()
    }
}

impl Document {
    pub fn html(html: impl Into<String>) -> Self {
        Self {
            html: html.into(),
            xhtml: false,
            base: None,
            root: None,
            stylesheets: Vec::new(),
            resources: FxHashMap::default(),
            loader: None,
            conformance: None,
            tagged: true,
            accessible: false,
            forms: false,
            image_quality: None,
            attachments: Vec::new(),
            factur_x: None,
            declared_profile: None,
            factur_x_version: None,
            factur_x_description: None,
            factur_x_attachment: None,
            created: None,
            report_layout: false,
            encryption: None,
            signature: None,
            svg_depth: 0,
            nesting: frames::Nesting::default(),
            limits: None,
            loaded: Arc::default(),
        }
    }

    /// An XHTML document (`application/xhtml+xml`, `.xhtml`), parsed as XML as browsers parse
    /// it: CDATA sections and empty-element tags (`<div/>`) work and nothing is implied.
    pub fn xhtml(xml: impl Into<String>) -> Self {
        Self {
            xhtml: true,
            ..Self::html(xml)
        }
    }

    /// Sets the creation date written into the PDF (the current time by default). A fixed date
    /// makes the output reproducible.
    #[must_use]
    pub fn created(mut self, date: Date) -> Self {
        self.created = Some(date);
        self
    }

    /// Whether to report where the elements with an `id` ended up: their pages and boxes, and
    /// the rows of their tables, in [`Output::layout`]. Off by default, as finding them takes a
    /// little time and memory.
    #[must_use]
    pub fn report_layout(mut self, report: bool) -> Self {
        self.report_layout = report;
        self
    }

    /// Signs the PDF digitally (`PAdES`). The time of signing is the creation date (see
    /// [`Document::created`]) unless the signature gives its own. An encrypted PDF (see
    /// [`Document::encrypt`]) is signed with its file key: the signature's incremental update is
    /// encrypted as the rest of the file is.
    #[must_use]
    pub fn sign(mut self, signature: Signature) -> Self {
        self.signature = Some(signature);
        self
    }

    /// Writes the PDF to a standard (PDF/A).
    #[must_use]
    pub fn conformance(mut self, conformance: Conformance) -> Self {
        self.conformance = Some(conformance);
        self
    }

    /// Whether the PDF is tagged: with the logical structure of the document (headings,
    /// paragraphs, lists, tables, images and links in reading order), which screen readers read
    /// and which lets text flow into other layouts. On by default; without it, a PDF with many
    /// table cells is somewhat smaller and faster to write.
    #[must_use]
    pub fn tagged(mut self, tagged: bool) -> Self {
        self.tagged = tagged;
        self
    }

    /// Writes the PDF to PDF/UA-1, the standard for accessible PDF: tagged, and checked for what
    /// the standard requires of the document itself. It needs a `<title>`, a language
    /// (`<html lang>`) and alternative text for every image (`alt`, empty for decoration);
    /// without them, rendering fails.
    #[must_use]
    pub fn accessible(mut self) -> Self {
        self.accessible = true;
        self
    }

    /// Writes images as JPEGs of this quality, from 1 (the smallest files) to 100 (the best
    /// pictures), which makes a PDF with photos much smaller; 80 suits screens and office
    /// printers. Without it, the pixels of images are compressed without loss.
    ///
    /// Only raster images whose pixels are all opaque become a JPEG. Images with transparent
    /// pixels stay as they are, as do images of formats without loss (PNG, GIF, lossless WebP,
    /// BMP, TIFF) whose JPEG would be no smaller than their file: logos, charts and QR codes.
    /// JPEG images are embedded as they are, never compressed again, and SVG images stay
    /// vectors. Below a quality of 90 the color of a JPEG has half the resolution of its
    /// brightness (4:2:0), from 90 the full one.
    ///
    /// A quality of 0 or above 100 fails the render with [`PdfError::ImageQuality`].
    #[must_use]
    pub fn image_quality(mut self, quality: u8) -> Self {
        self.image_quality = Some(quality);
        self
    }

    /// Protects the PDF with passwords: encrypted with AES-256, as PDF 2.0 defines it. The
    /// key, salts and initialization vectors are random, so the bytes differ from run to run
    /// (unless [`Encryption::seed`] fixes them, for tests). PDF/A and Factur-X cannot be
    /// encrypted: rendering such a document fails. An accessible PDF ([`Document::accessible`])
    /// can: assistive technology may always extract its text, whatever the permissions say.
    ///
    /// ```
    /// use fagus::{Document, Encryption, Permission};
    ///
    /// let document = Document::html("<p>Gehalt</p>").encrypt(
    ///     Encryption::new()
    ///         .user_password("open sesame")
    ///         .owner_password("change me")
    ///         .permissions([Permission::Print]),
    /// );
    /// ```
    #[must_use]
    pub fn encrypt(mut self, encryption: Encryption) -> Self {
        self.encryption = Some(encryption);
        self
    }

    /// Whether the form controls of the document become the fields of an interactive form,
    /// which a reader fills in in a PDF viewer: text fields, text areas, checkboxes, radio
    /// buttons, drop-down lists, list boxes and the buttons that submit (to an absolute
    /// `action`) or reset a form, named after their `name`, with their values. Off by
    /// default: Chrome prints the controls as they look, and so does Fagus without it. Controls
    /// in an `inert` element stay as they look.
    #[must_use]
    pub fn forms(mut self, forms: bool) -> Self {
        self.forms = forms;
        self
    }

    /// Attaches a file to the PDF.
    #[must_use]
    pub fn attach(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Makes the PDF an electronic invoice of ZUGFeRD 2 or Factur-X: the invoice XML is attached
    /// under the name its profile requires, described in the XMP metadata, and the PDF conforms
    /// to PDF/A-3b unless another PDF/A-3 level is chosen. The profile is the one the XML
    /// declares (EN 16931 if it declares none).
    #[must_use]
    pub fn factur_x(self, xml: Vec<u8>) -> Self {
        let profile = FacturXProfile::detect(&xml).unwrap_or(FacturXProfile::En16931);
        self.factur_x_profile(xml, profile)
    }

    /// Like [`Document::factur_x`], with the profile given.
    #[must_use]
    pub fn factur_x_profile(mut self, xml: Vec<u8>, profile: FacturXProfile) -> Self {
        self.declared_profile = FacturXProfile::detect(&xml);
        self.factur_x_attachment = Some(self.attachments.len());
        let description = self
            .factur_x_description
            .clone()
            .unwrap_or_else(|| FACTUR_X_DESCRIPTION.to_owned());
        self.attachments.push(
            Attachment::new(profile.file_name(), "text/xml", xml)
                .description(description)
                .relationship(profile.relationship()),
        );
        self.factur_x = Some(profile);
        self
    }

    /// Sets the `Version` that the Factur-X XMP metadata gives (`1.0` by default), for example
    /// `3.0` for an invoice of XRechnung 3.0. It applies to the invoice of
    /// [`Document::factur_x`], whether it is set before or after. Rendering fails if it is
    /// empty or contains control characters.
    #[must_use]
    pub fn factur_x_version(mut self, version: impl Into<String>) -> Self {
        self.factur_x_version = Some(version.into());
        self
    }

    /// Sets the description of the attached invoice XML ("Factur-X/ZUGFeRD invoice" by
    /// default). It applies to the invoice of [`Document::factur_x`], whether it is set before
    /// or after. Rendering fails if it is empty or contains control characters.
    #[must_use]
    pub fn factur_x_description(mut self, description: impl Into<String>) -> Self {
        let description = description.into();
        if let Some(attachment) = self
            .factur_x_attachment
            .and_then(|index| self.attachments.get_mut(index))
        {
            attachment.description = Some(description.clone());
        }
        self.factur_x_description = Some(description);
        self
    }

    /// The standard that forbids encrypting the document, if it is encrypted.
    fn encryption_problem(&self) -> Option<PdfError> {
        self.encryption.as_ref()?;
        let (standard, problems) = if let Some(conformance) = self.conformance {
            (
                conformance.name(),
                "the PDF cannot be encrypted (PDF/A forbids encryption)",
            )
        } else if self.factur_x.is_some() {
            (
                "Factur-X",
                "the invoice cannot be encrypted (it is PDF/A-3, which forbids encryption)",
            )
        } else {
            return None;
        };
        Some(PdfError::Conformance {
            standard: standard.to_owned(),
            problems: problems.to_owned(),
        })
    }

    /// What makes the version or description of the invoice unusable, if anything.
    fn factur_x_problem(&self) -> Option<String> {
        self.factur_x?;
        // Factur-X 1.07 and ZUGFeRD 2.3 require PDF/A-3. PDF/A-1 and PDF/A-2 levels are raised to
        // it; PDF/A-4 is not a level below PDF/A-3 but a successor on PDF 2.0.
        if let Some(conformance) = self.conformance.filter(|conformance| conformance.part() == 4) {
            return Some(format!(
                "the invoice must be PDF/A-3, not {} (choose 3b, 3u or 3a)",
                conformance.name()
            ));
        }
        let fields = [
            ("version", &self.factur_x_version),
            ("description", &self.factur_x_description),
        ];
        fields.into_iter().find_map(|(name, value)| {
            let value = value.as_deref()?;
            if value.trim().is_empty() {
                Some(format!("the invoice {name} is empty"))
            } else if value.chars().any(char::is_control) {
                Some(format!("the invoice {name} contains control characters"))
            } else {
                None
            }
        })
    }

    /// Sets the directory that relative URLs (stylesheets, images, fonts) resolve against. Only
    /// files inside this directory are read, unless [`Document::root`] allows a larger one.
    #[must_use]
    pub fn base_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.base = Some(directory.into());
        self
    }

    /// Allows reading files anywhere inside `directory` (which should contain the base
    /// directory), for documents whose URLs lead out of their own directory (`../fonts/…`).
    #[must_use]
    pub fn root(mut self, directory: impl Into<PathBuf>) -> Self {
        self.root = Some(directory.into());
        self
    }

    /// Hands over a resource: where the document refers to `url`, these bytes are used (and where
    /// the document of a frame refers to a URL that resolves to `url`, see [`ResourceLoader`]).
    #[must_use]
    pub fn resource(mut self, url: impl Into<String>, data: Vec<u8>) -> Self {
        self.resources.insert(url.into(), Arc::new(data));
        self
    }

    /// Sets a loader for resources that are not handed over. It is asked before a file is looked
    /// for: after `None`, the file inside the base directory (or root) is read.
    #[must_use]
    pub fn loader(mut self, loader: Arc<dyn ResourceLoader>) -> Self {
        self.loader = Some(loader);
        self
    }

    /// Sets the memory budget and the time limit of this document, in place of the engine's
    /// ([`Engine::set_limits`]).
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Adds a stylesheet after the stylesheets of the document.
    #[must_use]
    pub fn stylesheet(mut self, name: impl Into<String>, css: impl Into<String>) -> Self {
        self.stylesheets.push((name.into(), css.into()));
        self
    }
}

/// Time spent in each phase of rendering.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct Timings {
    pub parse: Duration,
    pub style: Duration,
    pub layout: Duration,
    pub paint: Duration,
    pub pdf: Duration,
}

/// A document that could not be rendered: why, and the diagnostics found before it failed.
///
/// `#[non_exhaustive]`: make one from an [`Error`] with `From`.
#[derive(Debug)]
#[non_exhaustive]
pub struct Failure {
    /// Why rendering failed.
    pub error: Error,
    /// The diagnostics found until then (CSS that was ignored, resources that are missing),
    /// which often explain the failure.
    pub diagnostics: Vec<Diagnostic>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.error)
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            error,
            diagnostics: Vec::new(),
        }
    }
}

/// The result of rendering.
///
/// `#[non_exhaustive]`, so that a field can be added in a minor release: outside the engine,
/// start from `Output::default()` and set the fields.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Output {
    pub pdf: Vec<u8>,
    pub pages: usize,
    pub diagnostics: Vec<Diagnostic>,
    pub timings: Timings,
    /// Where the elements with an `id` ended up, if the document asked for it with
    /// [`Document::report_layout`].
    pub layout: Option<Layout>,
}

/// The rendering engine. It keeps fonts, the parsed user agent stylesheet and the faces of
/// `@font-face` rules for its lifetime, so rendering many documents with one engine is fast.
pub struct Engine {
    inner: Arc<Inner>,
    workers: Workers,
}

/// What an engine renders with, shared with the threads that render.
struct Inner {
    fonts: FontDb,
    user_agent: Arc<Stylesheet>,
    /// What the user agent stylesheet adds in quirks mode.
    quirks: Arc<Stylesheet>,
    /// Faces loaded through `@font-face`, by their data and descriptors.
    document_faces: Mutex<FaceCache>,
    /// The language of the texts of form controls outside any `lang` ([`Engine::set_locale`]).
    locale: Option<String>,
    /// Images prepared for earlier documents (decoded raster images, parsed SVG documents).
    images: fagus_pdf::Images,
    /// Stylesheets parsed for earlier documents.
    sheets: sheets::Sheets,
    /// SVG images with HTML laid out as documents for earlier documents and layouts.
    svg_documents: Mutex<svg_document::Cache>,
    /// The memory budget and time limit of documents that set none of their own.
    limits: Limits,
}

/// Faces of `@font-face` rules, kept for the next documents up to a size: the oldest go first.
#[derive(Clone, Default)]
struct FaceCache {
    faces: FxHashMap<FaceKey, Arc<Face>>,
    order: std::collections::VecDeque<FaceKey>,
    bytes: usize,
}

impl FaceCache {
    /// The font data the cache holds at most.
    const MAX_BYTES: usize = 256 << 20;

    fn get(&self, key: &FaceKey) -> Option<Arc<Face>> {
        self.faces.get(key).cloned()
    }

    fn insert(&mut self, key: FaceKey, face: Arc<Face>) {
        self.bytes += face.data.len();
        if self.faces.insert(key.clone(), face).is_none() {
            self.order.push_back(key);
        }
        while self.bytes > Self::MAX_BYTES && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(face) = self.faces.remove(&oldest) {
                self.bytes -= face.data.len();
            }
        }
    }
}

impl Clone for Inner {
    fn clone(&self) -> Self {
        Self {
            fonts: self.fonts.clone(),
            user_agent: Arc::clone(&self.user_agent),
            quirks: Arc::clone(&self.quirks),
            document_faces: Mutex::new(
                self.document_faces
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
            ),
            locale: self.locale.clone(),
            images: self.images.clone(),
            sheets: self.sheets.clone(),
            // Laid out with the fonts and the locale of the engine, which may change.
            svg_documents: Mutex::default(),
            limits: self.limits,
        }
    }
}

/// A rendering for a worker thread.
type Job = Box<dyn FnOnce() + Send>;

/// The threads that render documents. They have a large stack ([`RENDER_STACK`]) and stay for the
/// next document: a new thread for every document would cost more than starting it, as its
/// allocations start cold. A thread is added when all are busy, up to one per processor.
struct Workers {
    jobs: mpsc::Sender<Job>,
    queue: Arc<Mutex<mpsc::Receiver<Job>>>,
    started: AtomicUsize,
    /// Threads waiting for a job.
    idle: Arc<AtomicUsize>,
}

impl Workers {
    fn new() -> Self {
        let (jobs, queue) = mpsc::channel();
        Self {
            jobs,
            queue: Arc::new(Mutex::new(queue)),
            started: AtomicUsize::new(0),
            idle: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Hands a job to a thread, or back if there is none and none can be started.
    fn run(&self, job: Job) -> Result<(), Job> {
        let limit = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let started = self.started.load(Ordering::Acquire);
        if self.idle.load(Ordering::Acquire) == 0 && started < limit {
            let queue = Arc::clone(&self.queue);
            let idle = Arc::clone(&self.idle);
            let spawned = std::thread::Builder::new()
                .name("fagus".to_owned())
                .stack_size(RENDER_STACK)
                .spawn(move || work(&queue, &idle));
            match spawned {
                Ok(_) => {
                    self.started.fetch_add(1, Ordering::AcqRel);
                }
                Err(_) if started == 0 => return Err(job),
                Err(_) => {}
            }
        }
        self.jobs.send(job).map_err(|error| error.0)
    }
}

/// The loop of a worker thread; it ends with the engine.
fn work(queue: &Mutex<mpsc::Receiver<Job>>, idle: &AtomicUsize) {
    loop {
        idle.fetch_add(1, Ordering::AcqRel);
        let job = queue.lock().unwrap_or_else(PoisonError::into_inner).recv();
        idle.fetch_sub(1, Ordering::AcqRel);
        match job {
            Ok(job) => job(),
            Err(_) => return,
        }
    }
}

/// A face from a document: its data (by hash) and its rule's descriptors (as they print, the
/// family in lower case).
#[derive(Clone, PartialEq, Eq, Hash)]
struct FaceKey {
    data: u64,
    descriptors: String,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("fonts", &self.inner.fonts.len())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// An engine with the fonts built into Fagus (Inter in regular and bold), which stand in
    /// where the fonts added and those of a document do not fit.
    pub fn new() -> Self {
        let mut fonts = FontDb::new();
        fonts.add_default_fonts();
        Self {
            inner: Arc::new(Inner {
                fonts,
                user_agent: Arc::new(Stylesheet::parse(
                    fagus_style::USER_AGENT_CSS,
                    Origin::UserAgent,
                    "fagus:ua.css",
                )),
                quirks: Arc::new(Stylesheet::parse(
                    fagus_style::QUIRKS_CSS,
                    Origin::UserAgent,
                    "fagus:quirks.css",
                )),
                document_faces: Mutex::default(),
                locale: None,
                images: fagus_pdf::Images::default(),
                sheets: sheets::Sheets::default(),
                svg_documents: Mutex::default(),
                limits: Limits::new(),
            }),
            workers: Workers::new(),
        }
    }

    /// Adds all fonts in a directory. Returns the number of faces added.
    pub fn add_font_dir(&mut self, directory: impl AsRef<Path>) -> Result<usize, Error> {
        Ok(Arc::make_mut(&mut self.inner)
            .fonts
            .add_font_dir(directory.as_ref())?)
    }

    /// Adds the fonts installed on the system, as Chrome finds them: on macOS those in
    /// `/System/Library/Fonts`, `/Library/Fonts` and `~/Library/Fonts`, on Linux and FreeBSD
    /// those fontconfig lists, on Windows those in `%WINDIR%\Fonts` and the user's. They serve the
    /// families that documents name, the generic families as in Chrome (Times for `serif`,
    /// Helvetica for `sans-serif`, Menlo for `monospace`, San Francisco for `system-ui`, and
    /// Hiragino, PingFang or Apple SD Gothic Neo by the language of the text) and fallback for
    /// characters that other fonts lack. Fonts from [`Self::add_font_dir`] and `@font-face` come
    /// first for their families. Faces are indexed now and read when a document uses them.
    ///
    /// Without this, the output does not depend on the machine: documents only get the fonts
    /// that are added and the fonts built into Fagus. Returns the number of faces found.
    pub fn add_system_fonts(&mut self) -> usize {
        Arc::make_mut(&mut self.inner).fonts.add_system_fonts()
    }

    /// The directories of the installed fonts that [`Self::add_system_fonts`] found: their files
    /// are read when a document first uses one of their faces, so a sandbox around the engine
    /// must let it read them. Empty without system fonts.
    pub fn system_font_directories(&self) -> Vec<PathBuf> {
        self.inner.fonts.system_font_directories()
    }

    /// Adds a font file's data. Returns the number of faces added.
    pub fn add_font_data(&mut self, data: Vec<u8>) -> Result<usize, Error> {
        Ok(Arc::make_mut(&mut self.inner).fonts.add_font_data(data)?.len())
    }

    /// Sets the language of the texts that form controls show of their own: the labels of
    /// submit and reset buttons, the button and text of file inputs, the placeholders and
    /// formats of date and time fields, the decimal separator of number fields, the summary of a
    /// `<details>` without one. Chrome writes
    /// these in the language of its user interface; Fagus takes the `lang` of the control
    /// (inherited from its ancestors, as `<html lang="de">`), else this language, else American
    /// English.
    ///
    /// `locale` is a language tag such as `de` or `de-DE`. The texts and formats are Chrome's for
    /// English (`en-US`, and `en-GB`), German, French, Spanish, Italian, Dutch, Portuguese
    /// (`pt-BR`, and `pt-PT`) and Polish; other languages are written in American English.
    ///
    /// Text outside any `lang` is shaped in this language as well, as Chrome shapes it in the
    /// language of its user interface: a face's forms for the language apply (EB Garamond's
    /// German `f`, which takes no `fi` ligature). Without a locale it is shaped in no language.
    pub fn set_locale(&mut self, locale: Option<String>) {
        let inner = Arc::make_mut(&mut self.inner);
        inner.locale = locale.filter(|locale| !locale.trim().is_empty());
        inner.fonts.set_default_language(inner.locale.as_deref());
    }

    /// Sets the families used for the generic families `sans-serif`, `serif` and `monospace`.
    pub fn set_generic_families(
        &mut self,
        sans_serif: Option<String>,
        serif: Option<String>,
        monospace: Option<String>,
    ) {
        Arc::make_mut(&mut self.inner)
            .fonts
            .set_generic_families(fagus_text::GenericFamilies {
                sans_serif,
                serif,
                monospace,
            });
    }

    /// Sets the memory budget and the time limit of each document (see [`Limits`]; by default
    /// [`Limits::new`]). A document can set its own with [`Document::limits`].
    pub fn set_limits(&mut self, limits: Limits) {
        Arc::make_mut(&mut self.inner).limits = limits;
    }

    /// The memory budget and the time limit of each document.
    pub fn limits(&self) -> Limits {
        self.inner.limits
    }

    /// Renders a document to PDF, on one of the engine's threads.
    pub fn render(&self, input: &Document) -> Result<Output, Error> {
        self.render_with_diagnostics(input)
            .map_err(|failure| failure.error)
    }

    /// Renders a document to PDF as [`Self::render`] does; when that fails, the failure keeps
    /// the diagnostics found before it.
    pub fn render_with_diagnostics(&self, input: &Document) -> Result<Output, Failure> {
        let (reply, result) = mpsc::sync_channel(1);
        let inner = Arc::clone(&self.inner);
        // The resources of this render are its own.
        let document = Document {
            loaded: Arc::default(),
            ..input.clone()
        };
        // The time limit counts from now, while the document may still wait for a thread.
        let limits = input.limits.unwrap_or(inner.limits);
        let budget =
            fagus_dom::budget::Budget::new(limits.memory_budget(), limits.time_limit(), Instant::now());
        let job: Job = Box::new(move || {
            let output = std::panic::catch_unwind(AssertUnwindSafe(|| {
                fagus_dom::budget::run(budget, || inner.render(&document))
                    .unwrap_or_else(|exceeded| Err(Failure::exceeded(exceeded)))
            }))
            .unwrap_or_else(|panic| {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "a panic".to_owned());
                Err(Error::Internal(message).into())
            });
            // Let go of the engine before answering: the caller may change it next.
            drop(inner);
            drop(document);
            // The caller waits for the answer; it cannot be gone.
            let _ = reply.send(output);
        });
        if let Err(job) = self.workers.run(job) {
            // Without a thread, the document is rendered on this one.
            job();
        }
        result
            .recv()
            .unwrap_or_else(|_| Err(Error::Internal("the rendering thread stopped".to_owned()).into()))
    }
}

impl Inner {
    fn render(&self, input: &Document) -> Result<Output, Failure> {
        if let Some(error) = input.encryption_problem() {
            return Err(Error::Pdf(error).into());
        }
        if let Some(quality) = input.image_quality
            && !(1..=100).contains(&quality)
        {
            return Err(Error::Pdf(PdfError::ImageQuality(quality)).into());
        }
        if let Some(problems) = input.factur_x_problem() {
            return Err(Error::Pdf(PdfError::Conformance {
                standard: "Factur-X".to_owned(),
                problems,
            })
            .into());
        }
        let mut timings = Timings::default();
        let start = Instant::now();
        let document = if input.xhtml {
            fagus_dom::Document::parse_xhtml(&input.html)
        } else {
            fagus_dom::Document::parse_html(&input.html)
        };
        let mut diagnostics = Vec::new();
        if document.truncated() {
            diagnostics.push(Diagnostic::new(
                Severity::Error,
                "html.too-deep",
                "elements are nested hundreds of levels deep; the rest of the document is left out",
                "html",
            ));
        }
        let sheets = self.document_sheets(input, &document, &mut diagnostics);
        for sheet in &sheets {
            if sheet.origin() == Origin::Author {
                diagnostics.extend(sheet.diagnostics().iter().cloned());
            }
        }
        diagnostics.extend(style_attribute_diagnostics(&document));
        timings.parse = start.elapsed();

        let start = Instant::now();
        let references: Vec<&Stylesheet> = sheets.iter().map(AsRef::as_ref).collect();
        let first_page_rules = RuleSet::new(&references, &MediaEnvironment::default());
        let root_style = fagus_style::ComputedStyle::default();
        let mut page = fagus_layout::page_box(&first_page_rules, &root_style, 0, None, &[]);
        let environment = MediaEnvironment {
            width: page.content_width().to_px(),
            height: page.content_height().to_px(),
        };
        let rules = RuleSet::new(&references, &environment);
        // Viewport units are relative to the viewport, the page area cut to whole pixels.
        let viewport = (page.viewport.0.to_px(), page.viewport.1.to_px());
        // The fonts first: font-relative lengths (`ex`, `ch`) refer to their metrics.
        let mut fonts = self.document_fonts(input, &rules, &mut diagnostics);
        let mut styled = fagus_style::compute_styles_with(
            &document,
            &rules,
            viewport,
            &fagus_style::ContainerSizes::default(),
            &fagus_layout::StyleFonts(&fonts),
        );
        // The viewport is the page area of the first page, which may be a named page of its
        // own size or margins (a cover without margins whose content is `100vh` tall), and whose
        // page context inherits from the root element (its font size for `em` margins, its
        // custom properties, its zoom, which zooms the margins the viewport is cut from), as in
        // Chrome: the styles are computed again with it.
        let names_pages = rules
            .page_rules()
            .iter()
            .any(|rule| rule.selectors.iter().any(fagus_css::PageSelector::names_pages));
        let (name, groups) = if names_pages {
            fagus_layout::first_page(&document, &styled, &rules, &fonts)
        } else {
            (None, Vec::new())
        };
        let root = document
            .document_element()
            .and_then(|root| styled.style(root))
            .map_or(&root_style, |root| &**root);
        let first = fagus_layout::page_box(&rules, root, 0, name.as_deref(), &groups);
        if first.viewport != page.viewport {
            let viewport = (first.viewport.0.to_px(), first.viewport.1.to_px());
            styled = fagus_style::compute_styles_with(
                &document,
                &rules,
                viewport,
                &fagus_style::ContainerSizes::default(),
                &fagus_layout::StyleFonts(&fonts),
            );
        }
        page = first;
        let viewport = (page.viewport.0.to_px(), page.viewport.1.to_px());
        timings.style = start.elapsed();

        let start = Instant::now();
        let mut images = self.document_images(input, (&document, &rules), &styled, &fonts, &mut diagnostics);
        let (mut pages, mut targets) = fagus_layout::layout_with_targets(
            &document,
            &styled,
            &rules,
            &fonts,
            &images,
            fagus_layout::Targets::default(),
            self.locale.as_deref(),
        );
        // `@container` rules and container query units apply as the sizes of their containers
        // in that layout say, which are laid out again with them.
        let containers = rules.has_container_queries()
            || styled
                .all()
                .any(|style| style.container_type != fagus_style::values::ContainerType::Normal);
        let mut styled = if containers {
            let sizes = container_sizes(&pages);
            let styled = fagus_style::compute_styles_with(
                &document,
                &rules,
                viewport,
                &sizes,
                &fagus_layout::StyleFonts(&fonts),
            );
            let mut quiet = Vec::new();
            images = self.document_images(input, (&document, &rules), &styled, &fonts, &mut quiet);
            (pages, targets) = fagus_layout::layout_with_targets(
                &document,
                &styled,
                &rules,
                &fonts,
                &images,
                fagus_layout::Targets::default(),
                self.locale.as_deref(),
            );
            styled
        } else {
            styled
        };
        // `target-counter()` and `target-text()` take what that layout found: the pages and
        // counters of the elements they point to.
        let found = targets.asked.then(|| targets.clone());
        if targets.asked {
            pages = fagus_layout::layout_with_targets(
                &document,
                &styled,
                &rules,
                &fonts,
                &images,
                targets,
                self.locale.as_deref(),
            )
            .0;
        }
        // Content wider than the page area shrinks to fit it, by at most a third, laid out
        // again on a page area as much larger, as Chrome prints it (in a vertical document,
        // content longer than the lines that fit down the page area).
        let shrink = pages.iter().map(fagus_paint::content_shrink).fold(1.0, f32::min);
        let mut shrunk = 1.0;
        if shrink < 1.0 - 1e-5 {
            let shrink = shrink.max(1.0 / 1.5);
            shrunk = shrink;
            // The viewport is as much larger: viewport units and the sizes of containers in
            // the wider layout apply, as Chrome lays the content out again.
            let exact = page.exact_scaled(1.0 / shrink);
            let viewport = (exact.0.floor(), exact.1.floor());
            let mut sizes = fagus_style::ContainerSizes::default();
            for pass in 0..if containers { 2 } else { 1 } {
                if pass > 0 {
                    sizes = container_sizes(&pages);
                }
                styled = fagus_style::compute_styles_with(
                    &document,
                    &rules,
                    viewport,
                    &sizes,
                    &fagus_layout::StyleFonts(&fonts),
                );
                let mut quiet = Vec::new();
                images = self.document_images(input, (&document, &rules), &styled, &fonts, &mut quiet);
                pages = fagus_layout::layout_shrunk(
                    &document,
                    &styled,
                    &rules,
                    &fonts,
                    &images,
                    found.clone().unwrap_or_default(),
                    self.locale.as_deref(),
                    shrink,
                )
                .0;
            }
        }
        // The documents of frames are laid out in the boxes the frames have. Frames as tall as
        // their documents (`frame-sizing`) take those heights, and the document is laid out
        // again with them.
        if images.frames().next().is_some() {
            let mut budget = frames::Budget::default();
            self.lay_out_frames(input, &document, &pages, &mut images, &mut fonts, &budget);
            if frames::fit_responsive_frames(&document, &styled, &mut images) {
                pages = fagus_layout::layout_shrunk(
                    &document,
                    &styled,
                    &rules,
                    &fonts,
                    &images,
                    found.clone().unwrap_or_default(),
                    self.locale.as_deref(),
                    shrunk,
                )
                .0;
                budget = frames::Budget::default();
                self.lay_out_frames(input, &document, &pages, &mut images, &mut fonts, &budget);
            }
            diagnostics.extend(frames::spent_warning(&budget));
        }
        timings.layout = start.elapsed();
        // The pages are held until they are painted.
        let pages_held =
            fagus_dom::budget::hold(fagus_dom::budget::Kind::Layout, fagus_layout::held_by(&pages));
        if pages.len() >= fagus_layout::MAX_PAGES {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                "layout.too-many-pages",
                format!(
                    "the document reaches the limit of {} pages; what follows is left out",
                    fagus_layout::MAX_PAGES
                ),
                "html",
            ));
        }

        // Factur-X attaches XML, which only PDF/A-3 allows (PDF/A-4 is refused above).
        let conformance = match (input.conformance, input.factur_x) {
            (None, Some(_)) => Some(Conformance::PdfA3b),
            (
                Some(
                    requested @ (Conformance::PdfA1b
                    | Conformance::PdfA2b
                    | Conformance::PdfA2u
                    | Conformance::PdfA2a),
                ),
                Some(_),
            ) => {
                let raised = match requested {
                    Conformance::PdfA2u => Conformance::PdfA3u,
                    Conformance::PdfA2a => Conformance::PdfA3a,
                    _ => Conformance::PdfA3b,
                };
                diagnostics.push(pdf_warning(
                    "pdf.conformance-raised",
                    format!(
                        "{} is written as {} because Factur-X attaches its XML, which only PDF/A-3 allows",
                        requested.name(),
                        raised.name()
                    ),
                ));
                Some(raised)
            }
            (conformance, _) => conformance,
        };
        let tagged = input.tagged || input.accessible || conformance.is_some_and(Conformance::is_tagged);

        // What the PDF needs of the document and its styles is taken now, so that they go with
        // the pages, before the PDF is written: in a long document, the computed styles take
        // more memory than anything else (about 2.7 KB per element).
        let start = Instant::now();
        let missing_glyph = missing_glyphs::warning(&document, &pages, &fonts);
        let metadata = Metadata {
            created: Some(input.created.unwrap_or_else(Date::now)),
            ..document_metadata(&document, self.locale.as_deref())
        };
        let (links, bookmarks) = interactive::collect(&document, &styled, &pages);
        let layout = input
            .report_layout
            .then(|| layout_info::collect(&document, &pages));
        let mut controls = if input.forms {
            forms::controls(&document, &styled, conformance.is_none())
        } else {
            forms::Controls::default()
        };
        // A signature visible in the box of an element shows what the element's content paints.
        let placement = input.signature.as_ref().and_then(Signature::placement);
        let signature_node = match placement {
            Some(Placement::Element(id)) => {
                let Some(node) = forms::element_by_id(&document, id) else {
                    return Err(Failure {
                        error: Error::Pdf(PdfError::Sign(format!(
                            "the document has no element with the id `{id}` for the signature"
                        ))),
                        diagnostics,
                    });
                };
                controls.lifts.insert(node, fagus_paint::Lift::Whole);
                Some(node)
            }
            _ => None,
        };
        let layer_warnings = layer_warnings(&styled, conformance, input.accessible);
        // The fields of a form are made of the widgets that painting finds, and of the document
        // and its styles, which are then kept until after painting; otherwise they go now.
        let mut structure = None;
        let mut inaccessible = Vec::new();
        let kept = if controls.lifts.is_empty() {
            if tagged {
                let built = structure::build(&document, &styled, &FxHashSet::default());
                if input.accessible {
                    inaccessible = accessibility_problems(&document, &metadata, &built);
                }
                structure = Some(built.structure);
            }
            drop(styled);
            drop(document);
            None
        } else {
            Some((document, styled))
        };
        drop(rules);
        drop(first_page_rules);
        drop(references);
        drop(sheets);
        let prepared = start.elapsed();

        let start = Instant::now();
        let (lists, widgets) = if controls.lifts.is_empty() {
            (fagus_paint::paint(&pages, &fonts, &images, tagged), Vec::new())
        } else {
            fagus_paint::paint_with_fields(&pages, &fonts, &images, tagged, &controls.lifts)
        };
        timings.paint = start.elapsed();
        diagnostics.extend(layer_warnings);
        // Installed fonts whose glyphs Fagus cannot draw gave way to the next font.
        for family in fonts.take_skipped_faces() {
            diagnostics.push(warning(
                "font.unsupported",
                format!(
                    "the installed font `{family}` has no glyphs that Fagus can draw; the next font is used"
                ),
            ));
        }
        diagnostics.extend(missing_glyph);

        let start = Instant::now();
        if let Some(profile) = input.factur_x {
            let problem = match input.declared_profile {
                None => Some((
                    "invoice.profile-unknown",
                    "the invoice XML declares no Factur-X profile".to_owned(),
                )),
                Some(declared) if declared != profile => Some((
                    "invoice.profile-mismatch",
                    format!(
                        "the invoice XML declares {}, not {}",
                        declared.level(),
                        profile.level()
                    ),
                )),
                Some(_) => None,
            };
            if let Some((code, message)) = problem {
                diagnostics.push(pdf_warning(code, message));
            }
        }
        let mut signature_problem = None;
        let mut fields = match kept {
            Some((document, styled)) => {
                let (signature_widgets, widgets): (Vec<_>, Vec<_>) = widgets
                    .into_iter()
                    .partition(|widget| Some(widget.node) == signature_node);
                let mut fields = forms::fields(&document, &styled, &controls, widgets, &pages);
                if signature_node.is_some() {
                    if signature_widgets.is_empty() {
                        signature_problem = Some(
                            "the element for the signature has no box to show it in (it is not \
                             rendered, or it is transformed)"
                                .to_owned(),
                        );
                    } else {
                        let field = forms::signature_field(
                            signature_node.and_then(|node| forms::signature_description(&document, node)),
                            forms::signature_widgets(signature_widgets),
                            &fields,
                        );
                        fields.push(field);
                    }
                }
                if tagged {
                    let controls: FxHashSet<fagus_dom::NodeId> = fields
                        .iter()
                        .flat_map(|field| &field.widgets)
                        .filter_map(|widget| widget.node)
                        .collect();
                    let built = structure::build(&document, &styled, &controls);
                    if input.accessible {
                        inaccessible = accessibility_problems(&document, &metadata, &built);
                    }
                    structure = Some(built.structure);
                }
                fields
            }
            None => Vec::new(),
        };
        // The fragments are painted: what is left goes into the PDF, which a document of many
        // pages need not hold beside them.
        let page_count = pages.len();
        drop(pages);
        drop(pages_held);
        if let Some(&Placement::Area {
            page,
            x,
            y,
            width,
            height,
        }) = placement
        {
            if page == 0 || page > page_count || !(width > 0.0 && height > 0.0) {
                signature_problem = Some(format!(
                    "the area of the signature is not on a page of the document (page {page} of {page_count})"
                ));
            } else {
                // From points to pixels.
                let pixels = |points: f32| points * 96.0 / 72.0;
                let widget = fagus_pdf::FieldWidget {
                    page: page - 1,
                    rect: [pixels(x), pixels(y), pixels(width), pixels(height)],
                    transform: [1.0, 0.0, 0.0, 1.0],
                    size: (pixels(width), pixels(height)),
                    clip: None,
                    node: None,
                    appearance: Vec::new(),
                    off: Vec::new(),
                    on: String::new(),
                    checked: false,
                    layer: None,
                };
                let field = forms::signature_field(None, vec![widget], &fields);
                fields.push(field);
            }
        }
        if let Some(problem) = signature_problem {
            return Err(Failure {
                error: Error::Pdf(PdfError::Sign(problem)),
                diagnostics,
            });
        }
        if !inaccessible.is_empty() {
            return Err(Failure {
                error: Error::Pdf(PdfError::Conformance {
                    standard: "PDF/UA-1".to_owned(),
                    problems: inaccessible.join("; "),
                }),
                diagnostics,
            });
        }
        let options = fagus_pdf::Options {
            conformance,
            attachments: input.attachments.clone(),
            xmp: input.factur_x.map(|profile| {
                factur_x_metadata(profile, input.factur_x_version.as_deref().unwrap_or("1.0"))
            }),
            links,
            bookmarks,
            structure,
            accessible: input.accessible,
            images: self.images.clone(),
            encryption: input.encryption.clone(),
            fields,
            image_quality: input.image_quality,
        };
        let written = match fagus_pdf::write(lists, &fonts, &metadata, &options) {
            Ok(written) => written,
            Err(error) => {
                return Err(Failure {
                    error: error.into(),
                    diagnostics,
                });
            }
        };
        let mut broken: Vec<&str> = images
            .iter()
            .filter(|(_, image)| {
                written
                    .broken_images
                    .iter()
                    .any(|data| Arc::ptr_eq(data, &image.data))
            })
            .map(|(url, _)| url)
            .collect();
        broken.sort_unstable();
        for url in broken {
            diagnostics.push(warning(
                "image.broken",
                format!("image `{}` could not be decoded and is left out", short(url)),
            ));
        }
        let mut pdf = written.pdf;
        if let Some(signature) = &input.signature {
            let time = metadata.created.unwrap_or_else(Date::now);
            let signed = match &input.encryption {
                Some(encryption) => fagus_pdf::sign::sign_encrypted(&pdf, signature, time, encryption),
                None => fagus_pdf::sign::sign(&pdf, signature, time),
            };
            pdf = match signed {
                Ok(signed) => signed,
                Err(error) => {
                    return Err(Failure {
                        error: error.into(),
                        diagnostics,
                    });
                }
            };
        }
        timings.pdf = start.elapsed() + prepared;

        Ok(Output {
            pdf,
            pages: page_count,
            diagnostics,
            timings,
            layout,
        })
    }

    /// The style sheets of `document` (of `input`), in cascade order: the user agent's, those of
    /// quirks mode, those of its `<style>` and `<link>` elements, and those given with it.
    fn document_sheets(
        &self,
        input: &Document,
        document: &fagus_dom::Document,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Vec<Arc<Stylesheet>> {
        let mut sheets = vec![Arc::clone(&self.user_agent)];
        if document.quirks_mode() {
            sheets.push(Arc::clone(&self.quirks));
        }
        let mut style_index = 0;
        // The author's colors and lengths as Chrome reads them: in quirks mode they may leave
        // out the `#` and the unit.
        let quirks = document.quirks_mode();
        let quirky = |css: String| fagus_css::author_css(&css, quirks).unwrap_or(css);
        // `@import` rules load their stylesheets as resources.
        let mut import = |url: &str| {
            self.load(input, url)
                .map(|css| quirky(String::from_utf8_lossy(&css).into_owned()))
        };
        // The linked style sheets are asked for together.
        self.preload(
            input,
            document
                .descendants(fagus_dom::NodeId::DOCUMENT)
                .filter_map(|node| document.element(node))
                .filter_map(linked_stylesheet),
        );
        for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
            let Some(element) = document.element(node) else {
                continue;
            };
            if element.is("style") {
                style_index += 1;
                sheets.push(self.sheets.get_or_parse(
                    &document.text_content(node),
                    quirks,
                    sheets::Origin::Style(style_index, node),
                    |css| {
                        Stylesheet::parse(
                            &quirky(css.to_owned()),
                            Origin::Author,
                            &format!("<style> {style_index}"),
                        )
                        .owned_by(node)
                    },
                    &mut import,
                ));
            } else if element.is_svg("style") {
                // The style sheet of an inline SVG is the document's, as in Chrome: its rules
                // apply to the HTML in its `<foreignObject>` elements (Mermaid's labels) and
                // around it too, and its `@font-face` rules load their fonts.
                let css = document.text_content(node);
                if !css.trim().is_empty() {
                    style_index += 1;
                    sheets.push(self.sheets.get_or_parse(
                        &css,
                        quirks,
                        sheets::Origin::Style(style_index, node),
                        |css| {
                            Stylesheet::parse(css, Origin::Author, &format!("<style> {style_index}"))
                                .owned_by(node)
                        },
                        &mut import,
                    ));
                }
            } else if let Some(href) = linked_stylesheet(element) {
                match self.load(input, href) {
                    Some(css) => sheets.push(self.sheets.get_or_parse(
                        &String::from_utf8_lossy(&css),
                        quirks,
                        sheets::Origin::Link(href.to_owned(), node),
                        |css| {
                            Stylesheet::parse_from(&quirky(css.to_owned()), Origin::Author, href)
                                .owned_by(node)
                        },
                        &mut import,
                    )),
                    None => diagnostics.push(
                        Diagnostic::new(
                            Severity::Warning,
                            "resource.not-loaded",
                            format!("stylesheet `{href}` could not be loaded"),
                            "html",
                        )
                        .at(element.line, 0),
                    ),
                }
            }
        }
        for (name, css) in &input.stylesheets {
            sheets.push(self.sheets.get_or_parse(
                css,
                quirks,
                sheets::Origin::Given(name.clone()),
                |css| Stylesheet::parse(css, Origin::Author, name),
                &mut import,
            ));
        }
        sheets
    }

    /// Loads a resource: from a `data:` URL, the resources handed over, the caller's loader, or
    /// the base directory, in that order.
    fn load(&self, input: &Document, url: &str) -> Option<Arc<Vec<u8>>> {
        let data = self.load_anew(input, url);
        // (The caller's loader may take its time.)
        fagus_dom::budget::check_time_now();
        Some(input.loaded.held_once(data?))
    }

    /// Asks the caller's loader for those of `urls` (as written in the document `input`) that
    /// only it can answer and that it was not asked for yet, together (see
    /// [`ResourceLoader::load_many`]), and keeps the answers for [`Self::load`]. The URLs must be
    /// ones that are loaded next in any case, in that order: the loader is asked for nothing it
    /// would not be asked for otherwise.
    fn preload<'a>(&self, input: &Document, urls: impl IntoIterator<Item = &'a str>) {
        let Some(loader) = &input.loader else {
            return;
        };
        if input.nesting.opaque {
            return;
        }
        let mut wanted: Vec<Cow<'a, str>> = Vec::new();
        let mut seen = FxHashSet::default();
        for url in urls {
            let url = url.trim();
            if url.is_empty() || frames::starts_with(url, "data:") {
                continue;
            }
            let url = frames::resolve(&input.nesting.directory, url);
            let resource = url.split_once('#').map_or(&*url, |(resource, _)| resource);
            if input.resources.contains_key(&*url)
                || input.resources.contains_key(resource)
                || input.loaded.was_asked(&url)
                || !seen.insert(url.clone())
            {
                continue;
            }
            wanted.push(url);
        }
        let ask = |urls: &[&str]| -> Vec<bool> {
            let mut found = Vec::with_capacity(urls.len());
            for urls in urls.chunks(BATCH) {
                found.extend(input.loaded.answered(urls, loader.load_many(urls)));
                // (The caller's loader may take its time.)
                fagus_dom::budget::check_time_now();
            }
            found
        };
        let wanted: Vec<&str> = wanted.iter().map(AsRef::as_ref).collect();
        let found = ask(&wanted);
        // Without the fragment next, as each on its own is asked for.
        let mut seen = FxHashSet::default();
        let again: Vec<&str> = wanted
            .iter()
            .zip(found)
            .filter(|(_, found)| !found)
            .filter_map(|(url, _)| url.split_once('#').map(|(resource, _)| resource))
            .filter(|resource| !input.loaded.was_asked(resource) && seen.insert(*resource))
            .collect();
        ask(&again);
    }

    /// Loads a resource as [`Self::load`] does, without looking for the same content loaded
    /// before (but each file only once).
    fn load_anew(&self, input: &Document, url: &str) -> Option<Arc<Vec<u8>>> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        if let Some(data) = resources::data_url(url) {
            return Some(Arc::new(data));
        }
        // The URLs of the document of a frame resolve against the frame's URL; one of a `data:`
        // URL loads nothing else.
        if input.nesting.opaque {
            return None;
        }
        let url = &*frames::resolve(&input.nesting.directory, url);
        // A fragment (`sprite.svg#icon`, `letterhead.pdf#page=2`) names a part of the resource,
        // which is handed over and resolved without it, unless the URL itself is.
        let resource = url.split_once('#').map_or(url, |(resource, _)| resource);
        if let Some(data) = input.resources.get(url).or_else(|| input.resources.get(resource)) {
            return Some(Arc::clone(data));
        }
        if let Some(loader) = &input.loader
            && let Some(data) = input.loaded.asked(url, || loader.load(url)).or_else(|| {
                (resource != url)
                    .then(|| input.loaded.asked(resource, || loader.load(resource)))
                    .flatten()
            })
        {
            return Some(data);
        }
        let base = input.base.as_deref()?;
        let root = input.root.as_deref().unwrap_or(base);
        input.loaded.file(resources::file_inside(base, root, url)?)
    }

    /// Loads the faces of the `@font-face` rules of the style sheets of an SVG image for its
    /// text: those from `data:` URLs, the only resources Chrome loads for an image.
    fn svg_image_faces(&self, data: &Arc<Vec<u8>>) {
        let Ok(text) = std::str::from_utf8(data) else {
            return;
        };
        if !text.contains("@font-face") {
            return;
        }
        let mut css = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("<style") {
            let Some(open) = rest[start..].find('>').map(|end| start + end + 1) else {
                break;
            };
            let Some(close) = rest[open..].find("</style").map(|end| open + end) else {
                break;
            };
            let content = rest[open..close].replace("<![CDATA[", "").replace("]]>", "");
            css.push_str(&fagus_pdf::svg::font_face_rules(&content));
            rest = &rest[close..];
        }
        if css.is_empty() {
            return;
        }
        let sheet = Stylesheet::parse(&css, Origin::Author, "svg");
        let rules = RuleSet::new(&[&sheet], &MediaEnvironment::default());
        let mut faces = Vec::new();
        for (rule, _) in rules.font_faces() {
            let Some((descriptors, sources)) = font_face::read(rule) else {
                continue;
            };
            let face = sources.into_iter().find_map(|source| match source {
                Source::Url(source) if font_face::loads(source) => {
                    let data = resources::data_url(source.url.url.as_ref())?;
                    let data = fagus_text::unpack(data).ok()?;
                    fagus_dom::budget::charge(fagus_dom::budget::Kind::Fonts, data.len() as u64);
                    Face::from_rule(Arc::new(data), 0, &descriptors)
                }
                _ => None,
            });
            faces.extend(face.map(Arc::new));
        }
        if !faces.is_empty() {
            self.images.add_svg_faces(data, faces);
        }
    }

    /// The fonts of a document: the engine's, plus the faces of its `@font-face` rules.
    fn document_fonts(
        &self,
        input: &Document,
        rules: &RuleSet,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Cow<'_, FontDb> {
        self.fonts_with_faces(&self.fonts, input, rules, diagnostics)
    }

    /// `base` with the faces of the `@font-face` rules of `rules` (of the document `input`)
    /// added, and their `@font-palette-values` rules (see [`Inner::document_fonts`]).
    fn fonts_with_faces<'a>(
        &self,
        base: &'a FontDb,
        input: &Document,
        rules: &RuleSet,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Cow<'a, FontDb> {
        if rules.font_faces().is_empty() && rules.font_palette_values().is_empty() {
            return Cow::Borrowed(base);
        }
        let mut fonts = base.clone();
        if !rules.font_palette_values().is_empty() {
            fonts.set_palette_values(
                rules
                    .font_palette_values()
                    .iter()
                    .map(palette_values::read)
                    .collect(),
            );
        }
        // The file each rule loads first is asked for with those of the others: the first source
        // of a format Chrome reads, unless an installed face named before it is taken.
        if input.loader.is_some() {
            let first = rules.font_faces().iter().filter_map(|(rule, _)| {
                let (descriptors, sources) = font_face::read(rule)?;
                for source in sources {
                    match source {
                        Source::Local(name) => {
                            let installed = self
                                .fonts
                                .local(&font_face::unquoted(name))
                                .and_then(|(data, index)| Face::from_rule(data, index, &descriptors));
                            if installed.is_some() {
                                return None;
                            }
                        }
                        Source::Url(source) if font_face::loads(source) => {
                            return Some(source.url.url.as_ref());
                        }
                        Source::Url(_) => {}
                    }
                }
                None
            });
            self.preload(input, first.collect::<Vec<&str>>());
        }
        for (rule, _) in rules.font_faces() {
            let Some((descriptors, sources)) = font_face::read(rule) else {
                continue;
            };
            // The first source that loads and parses wins: an installed face (`local()`) or a
            // file of a format Chrome reads.
            for source in sources {
                let url = match source {
                    Source::Local(name) => {
                        let local = self
                            .fonts
                            .local(&font_face::unquoted(name))
                            .and_then(|(data, index)| Face::from_rule(data, index, &descriptors));
                        if let Some(face) = local {
                            fonts.add_document_face(Arc::new(face));
                            break;
                        }
                        continue;
                    }
                    Source::Url(source) if font_face::loads(source) => source.url.url.as_ref(),
                    Source::Url(_) => continue,
                };
                let Some(data) = self.load(input, url) else {
                    diagnostics.push(warning(
                        "resource.not-loaded",
                        format!("font `{url}` could not be loaded"),
                    ));
                    continue;
                };
                // WOFF and WOFF2 are unpacked once; the cache keeps the unpacked face.
                let key = FaceKey {
                    data: {
                        let mut hasher = FxHasher::default();
                        data.hash(&mut hasher);
                        hasher.finish()
                    },
                    descriptors: format!("{descriptors:?}").to_ascii_lowercase(),
                };
                let cached = self
                    .document_faces
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&key);
                let face = cached.or_else(|| {
                    let data = fagus_text::unpack(data.to_vec()).ok()?;
                    let face = Arc::new(Face::from_rule(Arc::new(data), 0, &descriptors)?);
                    self.document_faces
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(key, Arc::clone(&face));
                    Some(face)
                });
                match face {
                    Some(face) => {
                        fagus_dom::budget::charge(fagus_dom::budget::Kind::Fonts, face.data.len() as u64);
                        fonts.add_document_face(face);
                        break;
                    }
                    None => diagnostics.push(warning(
                        "font.unsupported",
                        format!("`{url}` is not a font Fagus can read"),
                    )),
                }
            }
        }
        Cow::Owned(fonts)
    }

    /// Loads the images of `<img>` elements, of `background-image`, `mask-image`,
    /// `border-image-source` and `list-style-image`, of elements, pages and page margin boxes.
    fn document_images(
        &self,
        input: &Document,
        (document, rules): (&fagus_dom::Document, &RuleSet),
        styled: &fagus_style::StyledDocument,
        fonts: &FontDb,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Images {
        // Which `@page` rules apply is known only once the document is laid out: the images
        // of every rule and margin rule, each on its own.
        let root = fagus_style::ComputedStyle::default();
        let page_styles: Vec<fagus_style::ComputedStyle> = rules
            .page_rules()
            .iter()
            .flat_map(|rule| {
                std::iter::once(&rule.declarations)
                    .chain(rule.margin_rules.iter().map(|margin| &margin.declarations))
            })
            .map(|declarations| fagus_style::style_from_declarations(&[declarations], &root, (0.0, 0.0)))
            .collect();
        // The candidate each `<img>` with a `srcset` or in a `<picture>` shows, as Chrome chooses
        // it when it loads the document.
        let mut sources: Vec<(fagus_dom::NodeId, &str, f32)> = Vec::new();
        let mut urls: Vec<&str> = Vec::new();
        for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
            let Some(element) = document.element(node) else {
                continue;
            };
            if element.is("img") {
                match responsive::chosen_source(document, node, responsive::Medium::LoadScreen) {
                    Some((url, density)) => {
                        sources.push((node, url.trim(), density));
                        urls.push(url.trim());
                    }
                    None => urls.extend(element.attribute("src").map(str::trim)),
                }
            } else if let Some(url) = fagus_layout::embedded_image_url(element) {
                urls.push(url);
            } else if element.is("video") {
                urls.extend(element.attribute("poster").map(str::trim));
            }
        }
        for style in styled.all().map(AsRef::as_ref).chain(&page_styles) {
            let masks = style.mask.iter().flat_map(|mask| mask.images.iter());
            let border = style.border_image.iter().map(|border| &border.source);
            let marker = std::iter::once(&style.list_style_image);
            let shape = style.shape_outside.iter().filter_map(|shape| match &**shape {
                fagus_style::values::ShapeOutside::Image(image) => Some(image),
                fagus_style::values::ShapeOutside::Shape { .. } => None,
            });
            for image in style
                .background_image
                .iter()
                .chain(masks)
                .chain(border)
                .chain(marker)
                .chain(shape)
            {
                if let fagus_style::values::Image::Url(url, _) = image {
                    urls.push(url);
                }
            }
            // Images in generated content (`content: url(…)`).
            if let fagus_style::values::Content::Items(items) = &style.content {
                for item in items {
                    if let fagus_style::values::ContentItem::Image(url) = item {
                        urls.push(url.trim());
                    }
                }
            }
        }
        // Chrome chooses the sources of a `<picture>` with `media` queries again as it prints, and
        // shows the new choice only where its image is loaded already, as one the document shows
        // elsewhere (Chrome 157, measured).
        let page = MediaEnvironment::default();
        for (node, url, density) in &mut sources {
            if responsive::has_media_sources(document, *node)
                && let Some((printed, printed_density)) =
                    responsive::chosen_source(document, *node, responsive::Medium::Print(&page))
                && printed.trim() != *url
                && urls.contains(&printed.trim())
            {
                (*url, *density) = (printed.trim(), printed_density);
            }
        }
        let mut images = Images::default();
        for &(node, url, density) in &sources {
            images.insert_source(node, url, density);
        }
        // SVG elements of the document that `clip-path`, `mask` and `filter` name.
        let mut references: Vec<&str> = Vec::new();
        for style in styled.all().map(AsRef::as_ref) {
            if let Some(clip) = &style.clip_path
                && let fagus_style::values::ClipShape::Url(url) = &clip.shape
            {
                references.push(url);
            }
            for filter in style.filter.iter() {
                if let fagus_style::values::Filter::Url(url) = filter {
                    references.push(url);
                }
            }
        }
        urls.retain(|url| {
            let fragment = url.starts_with('#');
            if fragment {
                references.push(url);
            }
            !fragment
        });
        let load = |url: &str| self.load(input, url);
        svg_references(
            document,
            styled,
            fonts,
            references,
            &load,
            &mut images,
            diagnostics,
        );
        // The images are asked for together: each file once, by the first URL that names it.
        if input.loader.is_some() {
            let mut resources = FxHashSet::default();
            self.preload(
                input,
                urls.iter().copied().filter(|url| {
                    resources.insert(url.split_once('#').map_or(*url, |(resource, _)| resource))
                }),
            );
        }
        // Each URL once, though inherited properties name it in many styles.
        let mut tried = FxHashSet::default();
        let mut svg_bytes = 0;
        let mut parsed_svgs: ByData<Option<Arc<Image>>> = FxHashMap::default();
        // The same, by the address of data that is kept in `parsed_svgs`.
        let mut parsed_by_address: FxHashMap<usize, Option<Arc<Image>>> = FxHashMap::default();
        let mut loaded: FxHashMap<&str, Option<Arc<Vec<u8>>>> = FxHashMap::default();
        for url in urls {
            if url.is_empty() || !tried.insert(url) {
                continue;
            }
            // A file is loaded once for all the fragments of its URL (`sprite.svg#a1`,
            // `sprite.svg#a2`, …) and the URL without one, which do not leave the document.
            let resource = url.split_once('#').map_or(url, |(resource, _)| resource);
            let data = if resource.trim_start().starts_with("data:") {
                self.load(input, url)
            } else {
                loaded
                    .entry(resource)
                    .or_insert_with(|| self.load(input, url))
                    .clone()
            };
            let Some(data) = data else {
                diagnostics.push(warning(
                    "resource.not-loaded",
                    format!("image `{}` could not be loaded", short(url)),
                ));
                continue;
            };
            // A page of a PDF file, the one the fragment names (`letterhead.pdf#page=2`).
            if fagus_pdf::pdf_image::is_pdf(&data) {
                pdf_image(url, data, &mut images, diagnostics);
                continue;
            }
            if fagus_pdf::svg::is_svg(&data) {
                // A sprite shows the view or target its fragment names.
                let data = url
                    .split_once('#')
                    .and_then(|(_, fragment)| fagus_pdf::svg::with_fragment(&data, fragment))
                    .map_or(data, Arc::new);
                // The same data under many URLs (where the fragment changes nothing) is one
                // image, parsed once; other images are parsed up to a budget.
                if let Some(image) = parsed_by_address.get(&(Arc::as_ptr(&data) as usize)) {
                    if let Some(image) = image {
                        images.insert(url, Arc::clone(image));
                    }
                    continue;
                }
                let mut hasher = FxHasher::default();
                data.hash(&mut hasher);
                let same = parsed_svgs.entry((hasher.finish(), data.len())).or_default();
                if let Some((_, image)) = same.iter().find(|(known, _)| **known == *data) {
                    if let Some(image) = image {
                        images.insert(url, Arc::clone(image));
                    }
                    continue;
                }
                svg_bytes += data.len();
                if svg_bytes > MAX_SVG_IMAGE_BYTES {
                    diagnostics.push(warning(
                        "resource.not-loaded",
                        format!(
                            "image `{}` is not loaded: the SVG images of the document are too large",
                            short(url)
                        ),
                    ));
                    continue;
                }
                self.svg_image_faces(&data);
                let image = svg_image(Arc::clone(&data), fonts, &self.images, false);
                parsed_by_address.insert(Arc::as_ptr(&data) as usize, image.clone());
                same.push((data, image.clone()));
                match image {
                    Some(image) => {
                        // With HTML in it, it is drawn as a document of its own.
                        if let Some(document) = self.svg_document(input, &image, fonts) {
                            images.insert_document(&image.data, document);
                        }
                        images.insert(url, image);
                    }
                    None => diagnostics.push(warning(
                        "image.unsupported",
                        format!("image `{}` is not a valid SVG image", short(url)),
                    )),
                }
                continue;
            }
            // (The HTML document that an `<object>` shows is not an image.)
            let markup = frames::is_markup(&data);
            match Image::probe(data) {
                Ok(image) => images.insert(url, Arc::new(image)),
                Err(ImageError::Unsupported) if markup => {}
                Err(ImageError::Unsupported) => diagnostics.push(warning(
                    "image.unsupported",
                    format!(
                        "image `{}` is not a PNG, JPEG, GIF, WebP, BMP, TIFF, AVIF, SVG or PDF image",
                        short(url)
                    ),
                )),
                Err(ImageError::TooLarge) => diagnostics.push(warning(
                    "image.too-large",
                    format!("image `{}` has more than 200 megapixels", short(url)),
                )),
            }
        }
        // The alpha of the images that floats wrap their lines around (`shape-outside`).
        for style in styled.all() {
            if let Some(fagus_style::values::ShapeOutside::Image(fagus_style::values::Image::Url(url, _))) =
                style.shape_outside.as_deref()
                && images.alpha(url).is_none()
                && let Some(alpha) = images.get(url).and_then(|image| {
                    if image.format == fagus_layout::ImageFormat::Svg {
                        fagus_pdf::svg::alpha(image, fonts)
                    } else {
                        fagus_pdf::image_alpha(image)
                    }
                })
            {
                images.insert_alpha(url.to_string(), Arc::new(alpha));
            }
        }
        // SVG images that draw nothing (a file of `<mask>` definitions named as
        // `url(masks.svg#m)`) are no mask images, as in Chrome, which does not load such
        // `<mask>` elements either: the box is not masked.
        // Each URL is looked at once, each image's data (which many URLs may share) once.
        let mut checked = FxHashSet::default();
        let mut blank: FxHashMap<usize, bool> = FxHashMap::default();
        for style in styled.all() {
            for image in style.mask.iter().flat_map(|mask| mask.images.iter()) {
                if let fagus_style::values::Image::Url(url, _) = image
                    && checked.insert(&**url)
                    && !images.is_blank(url)
                    && let Some(image) = images.get(url)
                    && image.format == fagus_layout::ImageFormat::Svg
                    && *blank
                        .entry(Arc::as_ptr(&image.data) as usize)
                        .or_insert_with(|| fagus_pdf::svg::draws_nothing(&image.data, fonts, &self.images))
                {
                    images.insert_blank(url.to_string());
                }
            }
        }
        // Inline `<svg>` elements, drawn as SVG images in the color and font of their element.
        for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
            let Some(element) = document.element(node) else {
                continue;
            };
            // An `<svg>` right inside a `<foreignObject>` is one of its own, as in Chrome.
            let nested = document
                .parent_element(node)
                .and_then(|parent| document.element(parent))
                .is_some_and(|parent| {
                    parent.namespace() == &fagus_dom::ns!(svg) && !parent.is_svg("foreignObject")
                });
            if !element.is_svg("svg") || nested {
                continue;
            }
            let Some(style) = styled.style(node) else {
                continue;
            };
            // The painting properties (`fill`, `stroke`, …) that CSS declares for the elements
            // around it are inherited, the nearest first; those for its own elements go into
            // their `style` attributes.
            let mut inherited = inherited_svg_attributes(style).to_vec();
            for ancestor in std::iter::successors(document.parent_element(node), |&ancestor| {
                document.parent_element(ancestor)
            }) {
                for (name, value) in styled
                    .style(ancestor)
                    .iter()
                    .flat_map(|style| style.svg_paint.iter())
                    .filter(|(name, _)| !fagus_style::SVG_GEOMETRY.contains(name))
                {
                    if !inherited.iter().any(|(known, _)| known == name) {
                        inherited.push((name, value.clone()));
                    }
                }
            }
            // Elements whose transforms refer to their bounding boxes (`transform-box:
            // fill-box`) are measured in a document without them first.
            let boxes = if document
                .descendants(node)
                .any(|element| svg_transform::measured(document, styled, element))
            {
                let measure = svg_transform::Marks::Measure;
                let declared =
                    svg_transform::with_transforms(document, styled, &measure, svg_declarations, None);
                let markup = document.svg_document(node, &inherited, &declared);
                svg_transform::boxes(document, node, &markup, fonts)
            } else {
                FxHashMap::default()
            };
            let marks = svg_transform::Marks::Boxes(&boxes);
            let mut markup = {
                let declared =
                    svg_transform::with_transforms(document, styled, &marks, svg_declarations, Some(&images));
                document.svg_document(node, &inherited, &declared)
            };
            // Symbols of sprite files (`<use href="icons.svg#pin">`), copied in. Data loaded under
            // several URLs is held once.
            let mut loaded = FxHashMap::default();
            let sprites: Vec<(&str, Arc<Vec<u8>>)> = fagus_pdf::svg::external_uses(&markup)
                .into_iter()
                .filter_map(|url| {
                    let data = self.load(input, &xml_unescaped(url))?;
                    Some((url, held_once(&mut loaded, data)))
                })
                .collect();
            if !sprites.is_empty() {
                let sprites: Vec<(&str, &[u8])> = sprites
                    .iter()
                    .map(|(url, data)| (*url, data.as_slice()))
                    .collect();
                if let Some(resolved) = fagus_pdf::svg::with_external_uses(&markup, &sprites) {
                    markup = resolved;
                }
            }
            // Images it refers to by URL (`<image href="logo.png">`), copied in.
            let pictures: Vec<(&str, Arc<Vec<u8>>)> = fagus_pdf::svg::external_images(&markup)
                .into_iter()
                .filter_map(|url| {
                    let data = self.load(input, &xml_unescaped(url))?;
                    Some((url, held_once(&mut loaded, data)))
                })
                .collect();
            if !pictures.is_empty() {
                let pictures: Vec<(&str, &[u8])> = pictures
                    .iter()
                    .map(|(url, data)| (*url, data.as_slice()))
                    .collect();
                if let Some(resolved) = fagus_pdf::svg::with_external_images(&markup, &pictures) {
                    markup = resolved;
                }
            }
            let markup = Arc::new(markup.into_bytes());
            // An SVG of no size, such as one that only holds definitions, draws nothing.
            let (width, height, _) = fagus_pdf::svg::natural_size(&markup);
            let empty = width == Some(0.0) || height == Some(0.0);
            match svg_image(Arc::clone(&markup), fonts, &self.images, true) {
                Some(image) => {
                    images.insert_inline(node, image);
                    if let Some(foreign) = foreign_objects(document, node, &markup, fonts, &self.images) {
                        images.insert_foreign(node, Arc::new(foreign));
                    }
                }
                None if empty => {}
                None => diagnostics.push(
                    Diagnostic::new(
                        Severity::Warning,
                        "image.unsupported",
                        "an inline SVG could not be read",
                        "document",
                    )
                    .at(element.line, 0),
                ),
            }
        }
        // `<iframe>`, `<object>` and `<embed>` elements that show documents of their own.
        self.register_frames(input, document, &mut images);
        images
    }
}

/// The URL of the style sheet that a `<link rel="stylesheet">` element links.
fn linked_stylesheet(element: &fagus_dom::ElementData) -> Option<&str> {
    if element.is("link")
        && element.attribute("rel").is_some_and(|rel| {
            rel.split_ascii_whitespace()
                .any(|value| value.eq_ignore_ascii_case("stylesheet"))
        })
    {
        element.attribute("href")
    } else {
        None
    }
}

/// `data`, or the same data loaded before (which `loaded` holds by hash and length): what a
/// document loads under thousands of URLs (`icons.svg?v=1`, `icons.svg?v=2`, …) is held once.
fn held_once(loaded: &mut FxHashMap<(u64, usize), Arc<Vec<u8>>>, data: Arc<Vec<u8>>) -> Arc<Vec<u8>> {
    let mut hasher = FxHasher::default();
    data.hash(&mut hasher);
    match loaded.entry((hasher.finish(), data.len())) {
        std::collections::hash_map::Entry::Occupied(known) if **known.get() == *data => {
            Arc::clone(known.get())
        }
        std::collections::hash_map::Entry::Occupied(_) => data,
        std::collections::hash_map::Entry::Vacant(entry) => Arc::clone(entry.insert(data)),
    }
}

/// An attribute value as written in markup, with the entities of `&`, `<`, `>`, `"` and `'`
/// replaced by their characters.
fn xml_unescaped(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// What was made of data, by a hash and the length of the data, with the data, which data loaded
/// under several URLs shares.
type ByData<T> = FxHashMap<(u64, usize), Vec<(Arc<Vec<u8>>, T)>>;

/// The most bytes of SVG images that a document parses, each URL (with its fragment) an image of
/// its own.
const MAX_SVG_IMAGE_BYTES: usize = 128 << 20;

/// The most bytes of SVG documents that the clip paths, masks and filters of a document load
/// (`url(filters.svg#id)`): what loads under thousands of URLs is parsed once, but each load
/// reads it.
const MAX_REFERENCED_BYTES: usize = 256 << 20;

/// Finds the SVG elements that the URLs `references` name (`url(#id)` in the document, or
/// `url(filters.svg#id)` in an SVG document that `load` loads) and keeps them, made ready for
/// the boxes they apply to, in `images`.
fn svg_references(
    document: &fagus_dom::Document,
    styled: &fagus_style::StyledDocument,
    fonts: &FontDb,
    mut references: Vec<&str>,
    load: &dyn Fn(&str) -> Option<Arc<Vec<u8>>>,
    images: &mut Images,
    diagnostics: &mut Vec<Diagnostic>,
) {
    references.sort_unstable();
    references.dedup();
    let no_boxes = FxHashMap::default();
    let marks = svg_transform::Marks::Boxes(&no_boxes);
    let declared = svg_transform::with_transforms(document, styled, &marks, svg_declarations, None);
    let inherited = |element: fagus_dom::NodeId| {
        styled
            .style(element)
            .map(|style| inherited_svg_attributes(style).to_vec())
            .unwrap_or_default()
    };
    // SVG documents loaded for their elements: by URL, the index of the document (in
    // `documents`) parsed from its data, which documents loaded under many URLs (`defs.svg?v=1`,
    // `defs.svg?v=2`, …) share, by a hash and the length of the data. They load up to
    // [`MAX_REFERENCED_BYTES`].
    let mut external: FxHashMap<&str, Option<usize>> = FxHashMap::default();
    let mut documents: Vec<Option<fagus_dom::Document>> = Vec::new();
    let mut by_data: ByData<usize> = FxHashMap::default();
    let mut loaded_bytes = 0;
    for url in references {
        let Some(id) = url.strip_prefix('#') else {
            // An element of an SVG document of its own, as the document defines it.
            let reference = url.rsplit_once('#').and_then(|(resource, id)| {
                let index = (*external.entry(resource).or_insert_with(|| {
                    if loaded_bytes >= MAX_REFERENCED_BYTES {
                        return None;
                    }
                    let data = load(resource)?;
                    loaded_bytes += data.len();
                    let mut hasher = FxHasher::default();
                    data.hash(&mut hasher);
                    let same = by_data.entry((hasher.finish(), data.len())).or_default();
                    if let Some((_, index)) = same.iter().find(|(known, _)| **known == *data) {
                        return Some(*index);
                    }
                    let document = std::str::from_utf8(&data)
                        .ok()
                        .filter(|text| fagus_pdf::svg::is_svg(text.as_bytes()))
                        .map(fagus_dom::Document::parse_xhtml);
                    documents.push(document);
                    same.push((data, documents.len() - 1));
                    Some(documents.len() - 1)
                }))?;
                let svg = documents[index].as_ref()?;
                let markup =
                    svg.svg_definition(id, &|_| Vec::new(), &|_| fagus_dom::SvgDeclared::default())?;
                fagus_pdf::svg_reference::reference(&markup, id, fonts)
            });
            match reference {
                Some(reference) => images.insert_reference(url, Arc::new(reference)),
                None => diagnostics.push(warning(
                    "resource.not-loaded",
                    format!(
                        "`{}` names no SVG clip path, mask or filter that Fagus can apply",
                        short(url)
                    ),
                )),
            }
            continue;
        };
        let reference = document
            .svg_definition(id, &inherited, &declared)
            .and_then(|markup| {
                // The percentages of a clip path's lengths are of the viewport it is in.
                let viewport = document.svg_element(id).map_or((1.0, 1.0), |element| {
                    svg_transform::viewport_size(document, styled, element)
                });
                fagus_pdf::svg_reference::reference_in(&markup, id, fonts, viewport)
            });
        match reference {
            Some(reference) => images.insert_reference(url, Arc::new(reference)),
            None => diagnostics.push(warning(
                "resource.not-loaded",
                format!(
                    "`{}` names no SVG clip path, mask or filter that Fagus can apply",
                    short(url)
                ),
            )),
        }
    }
}

/// The `<foreignObject>` elements of the inline `<svg>` element `svg`, whose SVG document is
/// `markup`, where the SVG draws them; `None` if it draws none.
fn foreign_objects(
    document: &fagus_dom::Document,
    svg: fagus_dom::NodeId,
    markup: &Arc<Vec<u8>>,
    fonts: &FontDb,
    images: &fagus_pdf::Images,
) -> Option<fagus_layout::ForeignObjects> {
    let (size, fit, placed) = fagus_pdf::svg::foreign_objects(markup, fonts, images)?;
    let elements = document.foreign_objects(svg);
    let objects: Vec<fagus_layout::ForeignObject> = placed
        .into_iter()
        .filter_map(|(index, transform, rect)| {
            Some(fagus_layout::ForeignObject {
                node: *elements.get(&index)?,
                transform,
                rect,
            })
        })
        .collect();
    if objects.is_empty() {
        return None;
    }
    // Shapes after a `<foreignObject>` go over its HTML: the SVG is drawn in layers between them.
    let order: Vec<usize> = objects
        .iter()
        .filter_map(|object| {
            elements
                .iter()
                .find(|(_, node)| **node == object.node)
                .map(|(index, _)| *index)
        })
        .collect();
    let layers = fagus_pdf::foreign_layers::layers(markup, &order)
        .filter(|_| order.len() == objects.len())
        .and_then(|layers| {
            layers
                .into_iter()
                .map(|layer| svg_image(Arc::new(layer), fonts, images, true))
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();
    Some(fagus_layout::ForeignObjects {
        size,
        fit,
        objects,
        layers,
    })
}

/// The declarations that CSS makes for an SVG element, for its `style` attribute: its painting
/// properties, and the font and spacing of its text where CSS gives them other values than its
/// parent's (`.label text { font: 9px Inter; letter-spacing: 1px }`), which win over the
/// element's presentation attributes as in Chrome.
fn svg_declarations(
    document: &fagus_dom::Document,
    styled: &fagus_style::StyledDocument,
    element: fagus_dom::NodeId,
) -> String {
    let Some(style) = styled.style(element) else {
        return String::new();
    };
    let mut declarations: Vec<String> = style
        .svg_paint
        .iter()
        // The geometry properties are attributes (see `svg_geometry`).
        .filter(|(name, _)| !fagus_style::SVG_GEOMETRY.contains(name))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    let Some(parent_node) = document.parent_element(element) else {
        return declarations.join("; ");
    };
    let Some(parent) = styled.style(parent_node) else {
        return declarations.join("; ");
    };
    // The root's font comes in its attributes already (see `inherited_svg_attributes`), and it
    // takes the spacing of the HTML around it.
    let root = document
        .element(parent_node)
        .is_none_or(|data| data.namespace() != &fagus_dom::ns!(svg));
    if !root {
        let font = inherited_svg_attributes(style);
        let parent_font = inherited_svg_attributes(parent);
        // The color is left to `currentColor`, which the root's attribute resolves.
        for ((name, value), (_, parent_value)) in font.iter().zip(&parent_font).skip(1) {
            if value != parent_value {
                let unit = if *name == "font-size" { "px" } else { "" };
                declarations.push(format!("{name}: {value}{unit}"));
            }
        }
        // The compositing of its own elements (that of the root is the HTML box's).
        if style.opacity < 1.0 {
            declarations.push(format!("opacity: {}", style.opacity.max(0.0)));
        }
        if style.mix_blend_mode != fagus_style::values::BlendMode::Normal {
            declarations.push(format!("mix-blend-mode: {}", blend_keyword(style.mix_blend_mode)));
        }
        if style.isolation == fagus_style::values::Isolation::Isolate {
            declarations.push("isolation: isolate".to_owned());
        }
        // Its filter from a style sheet (its own attribute and `style` are its markup's). One
        // that names no `<filter>` filters nothing, as in Chrome (the SVG renderer would not
        // draw the element).
        let filters: Vec<fagus_style::values::Filter> = style
            .filter
            .iter()
            .filter(|filter| match filter {
                fagus_style::values::Filter::Url(url) => url.strip_prefix('#').is_none_or(|id| {
                    document
                        .svg_element(id)
                        .and_then(|node| document.element(node))
                        .is_some_and(|data| data.is_svg("filter"))
                }),
                _ => true,
            })
            .cloned()
            .collect();
        if let Some(filter) = svg_transform::svg_filter(&filters, style.color, style.zoom, None) {
            declarations.push(format!("filter: {filter}"));
        }
    }
    let (letter, word) = if root {
        (0.0, 0.0)
    } else {
        (parent.letter_spacing, parent.word_spacing)
    };
    if style.letter_spacing.to_bits() != letter.to_bits() {
        declarations.push(format!("letter-spacing: {}px", style.letter_spacing));
    }
    if style.word_spacing.to_bits() != word.to_bits() {
        declarations.push(format!("word-spacing: {}px", style.word_spacing));
    }
    declarations.join("; ")
}

/// The CSS keyword of a blend mode (`color-dodge`).
fn blend_keyword(mode: fagus_style::values::BlendMode) -> String {
    let mut keyword = String::new();
    for character in format!("{mode:?}").chars() {
        if character.is_ascii_uppercase() && !keyword.is_empty() {
            keyword.push('-');
        }
        keyword.push(character.to_ascii_lowercase());
    }
    keyword
}

/// Adds the page of the PDF file `data` that the fragment of `url` names to `images`, or says
/// why it cannot be shown.
fn pdf_image(url: &str, data: Arc<Vec<u8>>, images: &mut Images, diagnostics: &mut Vec<Diagnostic>) {
    use fagus_pdf::pdf_image::PdfImageError;
    let fragment = url.split_once('#').map(|(_, fragment)| fragment);
    let problem = match fagus_pdf::pdf_image::probe(data, fragment) {
        Ok(image) => {
            images.insert(url, Arc::new(image));
            return;
        }
        Err(PdfImageError::Broken) => ("image.broken", "is a PDF file that could not be read".to_owned()),
        Err(PdfImageError::Password) => (
            "image.unsupported",
            "is a PDF file that needs a password, which Fagus does not open".to_owned(),
        ),
        Err(PdfImageError::Version) => (
            "image.unsupported",
            "is a PDF 2.0 file, newer than the PDF 1.7 Fagus writes".to_owned(),
        ),
        Err(PdfImageError::TooLarge) => (
            "image.too-large",
            "is a PDF file larger than 128 MiB, or its page holds more than 128 MiB of content".to_owned(),
        ),
        Err(PdfImageError::NoPage(page, count)) => (
            "image.broken",
            format!(
                "names page {page} of a PDF file of {count} page{}",
                if count == 1 { "" } else { "s" }
            ),
        ),
    };
    diagnostics.push(warning(
        problem.0,
        format!("image `{}` {}", short(url), problem.1),
    ));
}

/// An SVG image (or an `inline` SVG element) with its natural size (none for a size its root
/// leaves out), or `None` if usvg cannot read it.
fn svg_image(
    data: Arc<Vec<u8>>,
    fonts: &FontDb,
    images: &fagus_pdf::Images,
    inline: bool,
) -> Option<Arc<Image>> {
    let (width, height) = fagus_pdf::svg::size(&data, fonts, images, inline)?;
    let (natural_width, natural_height, ratio) = if inline {
        fagus_pdf::svg::natural_size(&data)
    } else {
        fagus_pdf::svg::image_natural_size(&data)
    };
    Some(Arc::new(Image {
        data,
        format: fagus_layout::ImageFormat::Svg,
        width: natural_width.unwrap_or(width),
        height: natural_height.unwrap_or(height),
        natural: (natural_width.is_some(), natural_height.is_some()),
        ratio,
        inline,
        orientation: 1,
        filters: Arc::from([]),
        pad: 0,
    }))
}

/// What an inline SVG inherits from the HTML around it, as attributes of its root: the color
/// that `currentColor` stands for and the font of its text.
fn inherited_svg_attributes(style: &fagus_style::ComputedStyle) -> [(&'static str, String); 5] {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round();
    let color = style.color;
    let families: Vec<String> = style
        .font_family
        .iter()
        .map(|family| match family.as_str() {
            "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy" | "system-ui" => family.clone(),
            fagus_text::STANDARD => "serif".to_owned(),
            _ => format!("'{}'", family.replace(['\'', '\\'], "")),
        })
        .collect();
    [
        (
            "color",
            format!(
                "rgba({}, {}, {}, {})",
                channel(color.r),
                channel(color.g),
                channel(color.b),
                color.a.clamp(0.0, 1.0)
            ),
        ),
        ("font-family", families.join(", ")),
        ("font-size", format!("{}", style.font_size)),
        ("font-weight", format!("{}", style.font_weight.round())),
        (
            "font-style",
            match style.font_style {
                fagus_style::values::FontStyle::Normal => "normal",
                fagus_style::values::FontStyle::Italic => "italic",
                fagus_style::values::FontStyle::Oblique(_) => "oblique",
            }
            .to_owned(),
        ),
    ]
}

/// The XMP of a Factur-X invoice: its properties and the PDF/A extension schema that declares
/// them (Factur-X 1.0, §6.2.2; ZUGFeRD 2 uses the same).
fn factur_x_metadata(profile: FacturXProfile, version: &str) -> fagus_pdf::Extension {
    const NAMESPACE: &str = "urn:factur-x:pdfa:CrossIndustryDocument:invoice:1p0#";
    let property = |name: &str, description: &str| {
        format!(
            "<rdf:li rdf:parseType=\"Resource\"><pdfaProperty:name>{name}</pdfaProperty:name>\
             <pdfaProperty:valueType>Text</pdfaProperty:valueType>\
             <pdfaProperty:category>external</pdfaProperty:category>\
             <pdfaProperty:description>{description}</pdfaProperty:description></rdf:li>"
        )
    };
    fagus_pdf::Extension {
        descriptions: format!(
            "<rdf:Description rdf:about=\"\" xmlns:fx=\"{NAMESPACE}\">\
             <fx:DocumentType>INVOICE</fx:DocumentType>\
             <fx:DocumentFileName>{}</fx:DocumentFileName>\
             <fx:Version>{}</fx:Version>\
             <fx:ConformanceLevel>{}</fx:ConformanceLevel>\
             </rdf:Description>",
            profile.file_name(),
            xml_escaped(version),
            profile.level(),
        ),
        schemas: format!(
            "<rdf:li rdf:parseType=\"Resource\">\
             <pdfaSchema:schema>Factur-X PDFA Extension Schema</pdfaSchema:schema>\
             <pdfaSchema:namespaceURI>{NAMESPACE}</pdfaSchema:namespaceURI>\
             <pdfaSchema:prefix>fx</pdfaSchema:prefix>\
             <pdfaSchema:property><rdf:Seq>{}{}{}{}</rdf:Seq></pdfaSchema:property></rdf:li>",
            property("DocumentFileName", "The name of the embedded XML document"),
            property(
                "DocumentType",
                "The type of the hybrid document in capital letters, e.g. INVOICE or ORDER"
            ),
            property(
                "Version",
                "The actual version of the standard applying to the embedded XML document"
            ),
            property(
                "ConformanceLevel",
                "The conformance level of the embedded XML document"
            ),
        ),
    }
}

/// `text` with the characters that are markup in XML escaped.
fn xml_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            character => out.push(character),
        }
    }
    out
}

/// The diagnostics of the `style` attributes of a document: unknown properties and invalid
/// values, as those of style sheets, at the line of the element. Each different attribute is
/// reported once, at the first element that has it.
fn style_attribute_diagnostics(document: &fagus_dom::Document) -> Vec<Diagnostic> {
    let quirks = document.quirks_mode();
    let mut seen = FxHashSet::default();
    let mut found = Vec::new();
    for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
        let Some(element) = document.element(node) else {
            continue;
        };
        let Some(css) = element.attribute("style") else {
            continue;
        };
        if css.trim().is_empty() || !seen.insert(css) {
            continue;
        }
        // Colors and lengths as Chrome reads them, in quirks mode or not (as the cascade does).
        let css = fagus_css::author_css(css, quirks).unwrap_or_else(|| css.to_owned());
        for mut diagnostic in fagus_css::style_attribute_diagnostics(&css, "html", element.line) {
            diagnostic.message = format!(
                "`style` attribute of <{}>: {}",
                element.local_name(),
                diagnostic.message
            );
            found.push(diagnostic);
        }
    }
    found
}

fn warning(code: &'static str, message: String) -> Diagnostic {
    Diagnostic::new(Severity::Warning, code, message, "resources")
}

/// What a document says about itself: its title (`<title>`), language (`<html lang>`, else the
/// engine's locale, as Chrome takes the language of its user interface), and the author,
/// description and keywords of its `<meta>` elements.
fn document_metadata(document: &fagus_dom::Document, locale: Option<&str>) -> Metadata {
    let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let meta = |name: &str| {
        document
            .descendants(fagus_dom::NodeId::DOCUMENT)
            .filter_map(|node| document.element(node))
            .filter(|element| element.is("meta"))
            .find(|element| {
                element
                    .attribute("name")
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case(name))
            })
            .and_then(|element| element.attribute("content"))
            .map(normalize)
            .filter(|content| !content.is_empty())
    };
    Metadata {
        title: document
            .find("title")
            .map(|title| normalize(&document.text_content(title)))
            .filter(|title| !title.is_empty()),
        language: document
            .document_element()
            .and_then(|root| document.element(root))
            .and_then(|root| root.attribute("lang"))
            .or(locale)
            .map(str::trim)
            .filter(|lang| !lang.is_empty())
            .map(str::to_owned),
        authors: meta("author").into_iter().collect(),
        description: meta("description"),
        keywords: meta("keywords")
            .map(|keywords| {
                keywords
                    .split(',')
                    .map(str::trim)
                    .filter(|keyword| !keyword.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        ..Metadata::default()
    }
}

/// What a document lacks for PDF/UA that only its author can add.
fn accessibility_problems(
    document: &fagus_dom::Document,
    metadata: &Metadata,
    built: &structure::Built,
) -> Vec<String> {
    let mut problems = Vec::new();
    if metadata.title.is_none() {
        problems.push("the document has no title (add a <title>)".to_owned());
    }
    if metadata.language.is_none() {
        problems.push("the document has no language (add `lang` to <html>)".to_owned());
    }
    for &image in &built.without_alt {
        let element = document.element(image);
        let line = element.map_or(0, |element| element.line);
        if element.is_some_and(|element| element.is_svg("svg")) {
            problems.push(format!(
                "the SVG in line {line} has no alternative text (add a <title> or `aria-label`, \
                 or `aria-hidden=\"true\"` if it is decoration)"
            ));
            continue;
        }
        if element.is_some_and(|element| &*element.name.local == "math") {
            problems.push(format!(
                "the formula in line {line} has no alternative text (add `alttext`)"
            ));
            continue;
        }
        let source = element
            .and_then(|element| element.attribute("src"))
            .unwrap_or_default();
        problems.push(format!(
            "the image `{}` in line {line} has no alternative text (add `alt`, empty if it is decoration)",
            short(source.trim())
        ));
    }
    // Headings start at h1 and go down one level at a time (PDF/UA-1, 7.4.2).
    let mut last = 0;
    for &(heading, level) in &built.headings {
        if level > last + 1 {
            let line = document.element(heading).map_or(0, |element| element.line);
            problems.push(if last == 0 {
                format!("the first heading, an <h{level}> in line {line}, is not an <h1> (headings start at h1)")
            } else {
                format!(
                    "the <h{level}> in line {line} follows an <h{last}> (headings skip no level: use an <h{}>)",
                    last + 1
                )
            });
        }
        last = level;
    }
    problems
}

/// Warnings about layers (`-fagus-layer`) that the standards asked for do not allow: none in
/// PDF/A-1, none that depend on printing in the other levels of PDF/A and in PDF/UA-1.
fn layer_warnings(
    styled: &fagus_style::StyledDocument,
    conformance: Option<Conformance>,
    accessible: bool,
) -> Vec<Diagnostic> {
    let standard = match conformance {
        Some(conformance) => conformance.name(),
        None if accessible => "PDF/UA-1",
        None => return Vec::new(),
    };
    let layers: Vec<&fagus_style::values::Layer> =
        styled.all().filter_map(|style| style.layer.as_deref()).collect();
    if layers.is_empty() {
        return Vec::new();
    }
    if let Some(conformance) = conformance.filter(|conformance| conformance.part() == 1) {
        return vec![pdf_warning(
            "pdf.layers-unsupported",
            format!(
                "{} has no layers: what `-fagus-layer` puts on one is printed as if it had none",
                conformance.name()
            ),
        )];
    }
    let decided: std::collections::BTreeSet<&str> = layers
        .iter()
        .filter(|layer| layer.shown != fagus_style::values::LayerShown::Always)
        .map(|layer| &*layer.name)
        .collect();
    decided
        .into_iter()
        .map(|name| {
            pdf_warning(
                "pdf.layer-shown",
                format!(
                    "{standard} does not let what shows depend on printing: the layer `{name}` shows on screen and in print"
                ),
            )
        })
        .collect()
}

fn pdf_warning(code: &'static str, message: String) -> Diagnostic {
    Diagnostic::new(Severity::Warning, code, message, "pdf")
}

/// A URL for messages: `data:` URLs are cut short.
fn short(url: &str) -> Cow<'_, str> {
    if url.len() > 80 {
        Cow::Owned(format!("{}…", url.chars().take(60).collect::<String>()))
    } else {
        Cow::Borrowed(url)
    }
}

/// The content boxes of the query containers on laid-out pages (`container-type`), by element:
/// the first fragment of each.
fn container_sizes(pages: &[fagus_layout::Page]) -> fagus_style::ContainerSizes {
    fn walk(fragment: &fagus_layout::Fragment, sizes: &mut fagus_style::ContainerSizes) {
        if let fagus_layout::FragmentKind::Box {
            style,
            node: Some(node),
            ..
        } = &fragment.kind
            && style.container_type != fagus_style::values::ContainerType::Normal
        {
            let width = fragment.rect.width.to_px();
            let basis = width;
            let across = style.border_width.left
                + style.border_width.right
                + style.padding.left.resolve(basis)
                + style.padding.right.resolve(basis);
            let down = style.border_width.top
                + style.border_width.bottom
                + style.padding.top.resolve(basis)
                + style.padding.bottom.resolve(basis);
            let size = fagus_css::ContainerSize {
                width: (width - across).max(0.0),
                height: (style.container_type == fagus_style::values::ContainerType::Size)
                    .then(|| (fragment.rect.height.to_px() - down).max(0.0)),
                font_size: style.font_size,
            };
            sizes.entry(*node).or_insert(size);
        }
        for child in &fragment.children {
            walk(child, sizes);
        }
    }
    let mut sizes = fagus_style::ContainerSizes::default();
    for page in pages {
        for fragment in &page.fragments {
            walk(fragment, &mut sizes);
        }
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut engine = Engine::new();
        engine
            .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
            .expect("fonts");
        engine
    }

    /// The bytes of a PDF followed by the decoded contents of its object streams.
    fn decoded(pdf: &[u8]) -> Vec<u8> {
        let find = |haystack: &[u8], needle: &[u8]| {
            haystack.windows(needle.len()).position(|window| window == needle)
        };
        let mut all = pdf.to_vec();
        let mut rest = pdf;
        while let Some(start) = find(rest, b"/Type/ObjStm") {
            rest = &rest[start..];
            let length = find(rest, b"/Length ").and_then(|at| {
                let digits: Vec<u8> = rest[at + 8..]
                    .iter()
                    .copied()
                    .take_while(u8::is_ascii_digit)
                    .collect();
                String::from_utf8(digits).ok()?.parse::<usize>().ok()
            });
            let (Some(length), Some(data)) = (length, find(rest, b"stream\n").map(|at| at + 7)) else {
                break;
            };
            let mut decoder = flate2::read::ZlibDecoder::new(&rest[data..data + length]);
            std::io::Read::read_to_end(&mut decoder, &mut all).expect("an object stream");
            rest = &rest[data + length..];
        }
        all
    }

    /// Whether a PDF contains `text`, in its objects or in its object streams.
    fn contains(pdf: &[u8], text: &str) -> bool {
        decoded(pdf)
            .windows(text.len())
            .any(|window| window == text.as_bytes())
    }

    fn codes(output: &Output) -> Vec<&'static str> {
        output
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect()
    }

    const CREATED: Date = Date::new(2026, 11, 2, 12, 0, 0);

    /// An invoice XML that declares a guideline.
    fn invoice(guideline: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <rsm:CrossIndustryInvoice xmlns:rsm=\"urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100\" \
             xmlns:ram=\"urn:un:unece:uncefact:data:standard:ReusableAggregateBusinessInformationEntity:100\">\
             <rsm:ExchangedDocumentContext><ram:GuidelineSpecifiedDocumentContextParameter>\
             <ram:ID>{guideline}</ram:ID>\
             </ram:GuidelineSpecifiedDocumentContextParameter></rsm:ExchangedDocumentContext>\
             </rsm:CrossIndustryInvoice>"
        )
        .into_bytes()
    }

    /// The pages of `html` laid out with the user agent style sheet and no fonts, the content
    /// shrunk by `shrink`.
    fn shrunk_pages(html: &str, shrink: f32) -> Vec<fagus_layout::Page> {
        shrunk_pages_in(html, shrink, &FontDb::new())
    }

    /// [`shrunk_pages`] with the faces of `fonts`.
    fn shrunk_pages_in(html: &str, shrink: f32, fonts: &FontDb) -> Vec<fagus_layout::Page> {
        let document = fagus_dom::Document::parse_html(html);
        let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let css: String = document
            .descendants(fagus_dom::NodeId::DOCUMENT)
            .filter(|&node| document.element(node).is_some_and(|data| data.is("style")))
            .map(|node| document.text_content(node))
            .collect();
        let author = Stylesheet::parse(&css, Origin::Author, "style");
        let rules = RuleSet::new(&[&ua, &author], &MediaEnvironment::default());
        let styled = fagus_style::compute_styles(&document, &rules, (500.0, 300.0));
        let images = Images::default();
        fagus_layout::layout_shrunk(
            &document,
            &styled,
            &rules,
            fonts,
            &images,
            fagus_layout::Targets::default(),
            None,
            shrink,
        )
        .0
    }

    #[test]
    fn measures_lines_with_the_right_margins_of_their_boxes() {
        let page = "<style>@page { size: 600px 400px; margin: 50px } body { margin: 0 }</style>";
        let extent =
            |content: &str| fagus_paint::content_extent(&shrunk_pages(&format!("{page}{content}"), 1.0)[0]);
        // An inline block as wide as the line with margins of 2px (a slider's): the line, and
        // with it the document, reaches over both, as Chrome measures it to shrink the page.
        let block = |margin: &str| {
            format!(
                "<div><span style='display: inline-block; width: 100%; height: 10px; {margin}'></span></div>"
            )
        };
        assert!((extent(&block("margin: 2px")) - 504.0).abs() < 0.01);
        assert!((extent(&block("margin-left: 2px")) - 502.0).abs() < 0.01);
        assert!((extent(&block("margin: 2px 10%")) - 600.0).abs() < 0.01);
        // So does an inline box's right margin where it ends.
        let inline = "<div style='white-space: nowrap'><span style='display: inline-block; width: 480px'></span>\
                      <span style='margin-right: 40px'><span style='display: inline-block; width: 10px'></span></span></div>";
        assert!((extent(inline) - 530.0).abs() < 0.01);
        // A box that ends within the line keeps its margin inside the page area.
        assert!(
            (extent(
                "<div><span style='display: inline-block; width: 50%; margin-right: 20px'></span></div>"
            ) - 500.0)
                .abs()
                < 0.01
        );
    }

    #[test]
    fn measures_boxes_without_the_spaces_removed_at_the_ends_of_their_lines() {
        // An inline box of a French opening quote and a space (as `quotes: "« "` makes it in
        // `::before`) ends the first line inside a box that goes on to the next: the space is
        // removed there, so the line reaches no further than the quote, and Chrome does not
        // shrink the page.
        let mut fonts = FontDb::new();
        fonts.add_default_fonts();
        let page = "<style>@page { size: 400px 300px; margin: 20px } body { margin: 0 }
                    p { margin: 0; width: 360px; font: 16px Inter }</style>";
        let mut ended = 0;
        for word in 1..16 {
            let html = format!(
                "{page}<p>aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk {} \
                 <span><span>« </span>nnnnnnnnnnnnnnnnn ooo</span> ppp</p>",
                "m".repeat(word)
            );
            let pages = shrunk_pages_in(&html, 1.0, &fonts);
            let extent = fagus_paint::content_extent(&pages[0]);
            assert!(extent <= 360.0, "{word}: {extent}");
            ended += usize::from(extent > 355.0);
        }
        // Some of the quotes end their lines.
        assert!(ended > 0);
    }

    #[test]
    fn sets_margin_boxes_without_the_faces_the_content_does_not_use() {
        // A document's regular and bold faces; only the regular one sets the content, so Chrome
        // has not loaded the bold one when it sets the margin boxes, whose bold text takes the
        // next family. Where the content is bold too, they take the bold face.
        let web = |file: &str, weight: f32| {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts/").to_owned() + file;
            let data = fagus_text::unpack(std::fs::read(path).expect("web font")).expect("unpacked");
            let rule = fagus_text::Descriptors {
                family: "Web".to_owned(),
                weight: Some((weight, weight)),
                ..fagus_text::Descriptors::default()
            };
            Arc::new(Face::from_rule(Arc::new(data), 0, &rule).expect("face"))
        };
        let mut fonts = FontDb::new();
        fonts.add_default_fonts();
        fonts.add_document_face(web("Inter-Latin.woff2", 400.0));
        let bold = fonts.add_document_face(web("Inter-Bold-Latin.woff", 700.0));
        let head = |content: &str| {
            let html = format!(
                "<style>@page {{ size: 400px 300px; margin: 50px;
                   @top-left {{ content: 'Kopf'; font: 700 10px Web, sans-serif }} }}
                   body {{ margin: 0; font: 10px Web }}</style>{content}"
            );
            let pages = shrunk_pages_in(&html, 1.0, &fonts);
            let mut stack: Vec<&fagus_layout::Fragment> = pages[0].fragments[1..].iter().collect();
            while let Some(fragment) = stack.pop() {
                if let fagus_layout::FragmentKind::Text(run) = &fragment.kind {
                    return Some(run.face);
                }
                stack.extend(fragment.children.iter());
            }
            None
        };
        let plain = head("<p>Text</p>");
        assert!(plain.is_some_and(|face| face != bold), "{plain:?} {bold:?}");
        assert_eq!(head("<p>Text <b>fett</b></p>"), Some(bold));
    }

    #[test]
    fn measures_and_shrinks_content_wider_than_the_page() {
        let page = "<style>@page { size: 600px 400px; margin: 50px } body { margin: 0 }</style>";
        // A box wider than the page area of 500 pixels, and one turned past its edge: Chrome
        // shrinks the page by the width they reach.
        let wide = format!("{page}<div style='width: 600px; height: 10px'></div>");
        let pages = shrunk_pages(&wide, 1.0);
        assert!((fagus_paint::content_extent(&pages[0]) - 600.0).abs() < 0.01);
        let turned = format!(
            "{page}<div style='margin-left: 444px; width: 56px; height: 56px; transform: rotate(-4deg)'></div>"
        );
        let extent = fagus_paint::content_extent(&shrunk_pages(&turned, 1.0)[0]);
        let half = 28.0 * (libm::cosf((4.0_f32).to_radians()) + libm::sinf((4.0_f32).to_radians()));
        assert!((extent - (472.0 + half)).abs() < 0.01, "{extent}");
        // Content that clips its overflow reaches no further than itself.
        let clipped = format!(
            "{page}<div style='overflow: hidden'><div style='width: 600px; height: 10px'></div></div>"
        );
        assert!((fagus_paint::content_extent(&shrunk_pages(&clipped, 1.0)[0]) - 500.0).abs() < 0.01);
        // A fixed box wider than the page does not widen the document; an absolute one does.
        let fixed = format!("{page}<div style='position: fixed; width: 600px; height: 10px'></div>");
        assert!((fagus_paint::content_extent(&shrunk_pages(&fixed, 1.0)[0]) - 500.0).abs() < 0.01);
        let absolute = fixed.replace("fixed", "absolute");
        assert!((fagus_paint::content_extent(&shrunk_pages(&absolute, 1.0)[0]) - 600.0).abs() < 0.01);
        // Preserved spaces that hang at the end of a line in an inline box that goes on reach
        // nowhere, though the box takes them.
        let hanging = format!(
            "{page}<p style='margin: 0; font: 20px/20px Inter'>{} <span style='white-space: pre-wrap'>a{}b</span></p>",
            "x".repeat(40),
            " ".repeat(80)
        );
        let extent = fagus_paint::content_extent(&shrunk_pages(&hanging, 1.0)[0]);
        assert!(extent <= 500.0 + 0.01, "{extent}");
        // Shrunk to 5/6, the content is laid out on a page area 6/5 as large, which the page
        // draws shrunk.
        let pages = shrunk_pages(&wide, 500.0 / 600.0);
        assert!((pages[0].scale - 500.0 / 600.0).abs() < 1e-6);
        let content = &pages[0].fragments[0];
        assert_eq!(content.rect.width.to_px(), 600.0);
        assert_eq!(content.rect.x.to_px(), 50.0);
    }

    #[test]
    fn shrinks_pages_where_justified_items_round_past_the_line() {
        // Chrome rounds each item of a justified line up to layout units once it has widened
        // it: the line of the generated `(3) ` and the paragraph's text reaches 1/64 pixel past
        // its 446 pixels, which shrinks the page by as much (Chrome 157 on macOS at 9.5pt: lines
        // laid out 447 pixels wide, drawn at 446/446.016). Inter at 9.5pt is set narrower on
        // Linux, where the items of this line fit; at 13px, a size of whole 1/64 pixels, they
        // round past it again.
        let mut fonts = FontDb::new();
        fonts
            .add_font_dir(Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fonts"
            )))
            .expect("fonts");
        let html =
            "<style>@page { size: 548px 300px; margin: 0 51px } body { margin: 0; font: 13px/1.5 Inter }
                      p { margin: 0; text-align: justify } p::before { content: '(3) ' }</style>
                    <p>Es darf keine Person durch Ausgaben, die dem Zweck des Vereins fremd sind, oder durch
                    unverhältnismäßig hohe Vergütungen begünstigt werden.</p>";
        let extent = fagus_paint::content_extent(&shrunk_pages_in(html, 1.0, &fonts)[0]);
        assert!((extent - (446.0 + 1.0 / 64.0)).abs() < 1e-3, "{extent}");
        // Left aligned, the content reaches no further than its block.
        let left = html.replace("justify", "left");
        let extent = fagus_paint::content_extent(&shrunk_pages_in(&left, 1.0, &fonts)[0]);
        assert!((extent - 446.0).abs() < 1e-3, "{extent}");
    }

    #[test]
    fn does_not_shrink_pages_for_punctuation_that_hangs() {
        // Hanging punctuation past the end of a line, or past the start of a right-to-left one,
        // is ink overflow (CSS Text 3, §8.2): the page keeps its scale.
        let mut fonts = FontDb::new();
        fonts
            .add_font_dir(Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fonts"
            )))
            .expect("fonts");
        let page = "<style>@page { size: 600px 400px; margin: 50px } body { margin: 0; font: 20px Inter }
                      p { margin: 0; text-align: right; hanging-punctuation: first force-end }";
        for (style, content) in [
            ("", "Ab cd."),
            ("p { direction: rtl }", "\u{201C}Ab cd"),
            (":root { writing-mode: vertical-rl }", "Ab cd."),
        ] {
            let html = format!("{page} {style}</style><p>{content}</p>");
            let pages = shrunk_pages_in(&html, 1.0, &fonts);
            let shrink = fagus_paint::content_shrink(&pages[0]);
            assert!(shrink >= 1.0 - 1e-6, "{style}: {shrink}");
            // Without hanging punctuation, nothing reaches past the page area either.
            let plain = html.replace("hanging-punctuation: first force-end", "");
            assert!(fagus_paint::content_shrink(&shrunk_pages_in(&plain, 1.0, &fonts)[0]) >= 1.0 - 1e-6);
        }
    }

    #[test]
    fn shrinks_vertical_documents_by_the_length_of_their_lines() {
        let page = "<style>@page { size: 600px 400px; margin: 50px } body { margin: 0 }";
        let shrink = |mode: &str, content: &str| {
            let html = format!("{page} :root {{ writing-mode: {mode} }}</style>{content}");
            shrunk_pages(&html, 1.0)
                .iter()
                .map(fagus_paint::content_shrink)
                .fold(1.0, f32::min)
        };
        let wide = "<div style='width: 600px; height: 10px'></div>";
        let long = "<div style='width: 10px; height: 450px'></div>";
        // A horizontal document shrinks by the width its content reaches, the page area being
        // 500 pixels wide.
        assert!((shrink("horizontal-tb", wide) - 500.0 / 600.0).abs() < 1e-4);
        // A vertical one by the length of its lines down the page area of 300 pixels, as Chrome
        // shrinks it: what reaches past the side goes on the next page.
        for mode in ["vertical-lr", "vertical-rl", "sideways-rl"] {
            assert!((shrink(mode, long) - 300.0 / 450.0).abs() < 1e-4, "{mode}");
            assert!((shrink(mode, wide) - 1.0).abs() < 1e-4, "{mode}");
        }
        // `sideways-lr` lines run up the page: their content reaches past its top, which does
        // not count.
        assert!((shrink("sideways-lr", long) - 1.0).abs() < 1e-4);
        // Page margins of 10.3 pixels leave an area 379.41 pixels tall, which the root is laid
        // out on 380 tall: that it reaches does not shrink the document.
        let html = "<style>@page { size: 600px 400px; margin: 10.3px } body { margin: 0 }
                    :root { writing-mode: vertical-rl }</style><p>x</p>";
        let pages = shrunk_pages(html, 1.0);
        assert!(fagus_paint::content_shrink(&pages[0]) >= 1.0);
    }

    #[test]
    fn detects_profiles() {
        let cases = [
            ("urn:factur-x.eu:1p0:minimum", FacturXProfile::Minimum),
            ("urn:factur-x.eu:1p0:basicwl", FacturXProfile::BasicWl),
            (
                "urn:cen.eu:en16931:2017#compliant#urn:factur-x.eu:1p0:basic",
                FacturXProfile::Basic,
            ),
            ("urn:cen.eu:en16931:2017", FacturXProfile::En16931),
            (
                "urn:cen.eu:en16931:2017#conformant#urn:factur-x.eu:1p0:extended",
                FacturXProfile::Extended,
            ),
            (
                "urn:cen.eu:en16931:2017#conformant#urn:zugferd.de:2p0:extended",
                FacturXProfile::Extended,
            ),
            (
                "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0",
                FacturXProfile::XRechnung,
            ),
        ];
        for (guideline, profile) in cases {
            assert_eq!(
                FacturXProfile::detect(&invoice(guideline)),
                Some(profile),
                "{guideline}"
            );
        }
        assert_eq!(FacturXProfile::detect(&invoice("urn:example:other")), None);
        assert_eq!(FacturXProfile::detect(b"<Invoice/>"), None);
        let corpus = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/corpus/invoice/factur-x.xml"
        ));
        assert_eq!(
            FacturXProfile::detect(&corpus.expect("corpus")),
            Some(FacturXProfile::En16931)
        );
    }

    #[test]
    fn signs_documents() {
        let p12 = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/signing/rsa.p12"
        ))
        .expect("fixture");
        let signature = Signature::pkcs12(&p12, "fagus-test-only").expect("PKCS#12 file");
        let document = Document::html("<title>Rechnung</title><p>Rechnung RE-1</p>")
            .created(CREATED)
            .conformance(Conformance::PdfA3b)
            .factur_x(invoice("urn:cen.eu:en16931:2017"))
            .sign(signature.clone().reason("Rechnung"));
        let first = engine().render(&document).expect("rendered").pdf;
        assert_eq!(
            engine().render(&document).expect("rendered").pdf,
            first,
            "reproducible"
        );
        let unsigned = engine()
            .render(&Document {
                signature: None,
                ..document.clone()
            })
            .expect("rendered")
            .pdf;
        // The signature follows the file as it would be without it, at the creation date.
        assert!(first.starts_with(&unsigned));
        let update = String::from_utf8_lossy(&first[unsigned.len()..]).into_owned();
        assert!(
            update.contains("/M(D:20261102120000Z)/Reason(Rechnung)"),
            "{update}"
        );
        // A signature of its own time.
        let later = Date::new(2026, 11, 3, 12, 0, 0);
        let dated = engine()
            .render(&document.sign(signature.time(later)))
            .expect("rendered")
            .pdf;
        assert!(String::from_utf8_lossy(&dated).contains("/M(D:20261103120000Z)"));
        // A signer that fails fails the render.
        let failing = Signature::with_signer(
            vec![
                std::fs::read(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/signing/rsa.pem"
                ))
                .expect("fixture"),
            ],
            |_: &[u8], _| Err("offline".to_owned()),
        );
        let error = engine()
            .render(&Document::html("<p>x</p>").sign(failing))
            .expect_err("failed");
        assert!(matches!(error, Error::Pdf(PdfError::Sign(_))), "{error}");
    }

    #[test]
    fn writes_factur_x_invoices() {
        let document = Document::html("<title>Rechnung</title><p>Rechnung RE-1</p>")
            .created(CREATED)
            .factur_x(invoice("urn:cen.eu:en16931:2017"));
        let output = engine().render(&document).expect("rendered");
        let pdf = &output.pdf;
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(contains(pdf, "/F(factur-x.xml)/UF(factur-x.xml)"));
        assert!(contains(pdf, "/AFRelationship/Alternative"));
        assert!(contains(
            pdf,
            "<pdfaid:part>3</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>"
        ));
        assert!(contains(
            pdf,
            "<fx:DocumentFileName>factur-x.xml</fx:DocumentFileName>"
        ));
        assert!(contains(
            pdf,
            "<fx:ConformanceLevel>EN 16931</fx:ConformanceLevel>"
        ));
        assert!(contains(pdf, "<pdfaSchema:prefix>fx</pdfaSchema:prefix>"));
        assert!(pdf.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn raises_pdf_a_2_for_factur_x() {
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .conformance(Conformance::PdfA2u)
            .factur_x(invoice("urn:factur-x.eu:1p0:minimum"));
        let output = engine().render(&document).expect("rendered");
        assert_eq!(codes(&output), ["pdf.conformance-raised"]);
        assert_eq!(
            output.diagnostics[0].message,
            "PDF/A-2u is written as PDF/A-3u because Factur-X attaches its XML, which only PDF/A-3 allows"
        );
        assert!(contains(
            &output.pdf,
            "<pdfaid:part>3</pdfaid:part><pdfaid:conformance>U</pdfaid:conformance>"
        ));
        assert!(contains(&output.pdf, "/AFRelationship/Data"));
        assert!(contains(
            &output.pdf,
            "<fx:ConformanceLevel>MINIMUM</fx:ConformanceLevel>"
        ));
    }

    #[test]
    fn names_x_rechnung_files() {
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x(invoice(
                "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0",
            ));
        let output = engine().render(&document).expect("rendered");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(contains(&output.pdf, "/F(xrechnung.xml)"));
        assert!(contains(
            &output.pdf,
            "<fx:ConformanceLevel>XRECHNUNG</fx:ConformanceLevel>"
        ));
    }

    #[test]
    fn writes_the_version_and_description_given() {
        // The defaults.
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x(invoice("urn:cen.eu:en16931:2017"));
        let pdf = engine().render(&document).expect("rendered").pdf;
        assert!(contains(&pdf, "<fx:Version>1.0</fx:Version>"));
        assert!(contains(&pdf, "/Desc(Factur-X/ZUGFeRD invoice)"));

        // Set before and after the invoice.
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x_version("3.0")
            .factur_x(invoice(
                "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0",
            ))
            .factur_x_description("XRechnung Invoice");
        let output = engine().render(&document).expect("rendered");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(contains(&output.pdf, "<fx:Version>3.0</fx:Version>"));
        assert!(!contains(&output.pdf, "<fx:Version>1.0</fx:Version>"));
        assert!(contains(&output.pdf, "/Desc(XRechnung Invoice)"));
        assert!(!contains(&output.pdf, "Factur-X/ZUGFeRD invoice"));
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x_description("Factur-X/ZUGFeRD Invoice")
            .factur_x(invoice("urn:cen.eu:en16931:2017"));
        let pdf = engine().render(&document).expect("rendered").pdf;
        assert!(contains(&pdf, "/Desc(Factur-X/ZUGFeRD Invoice)"));

        // Markup in the XMP and parentheses in the PDF string are escaped.
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x(invoice("urn:cen.eu:en16931:2017"))
            .factur_x_version("1.0 <&>\"'")
            .factur_x_description("Rechnung ) ( \\ endobj");
        let pdf = engine().render(&document).expect("rendered").pdf;
        assert!(contains(
            &pdf,
            "<fx:Version>1.0 &lt;&amp;&gt;&quot;&apos;</fx:Version>"
        ));
        assert!(contains(&pdf, "/Desc(Rechnung \\) \\( \\\\ endobj)"));
    }

    #[test]
    fn refuses_empty_invoice_versions_and_descriptions() {
        let refused = |document: Document| match engine().render(&document) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("rendered"),
        };
        let document = || Document::html("<p>Rechnung</p>").factur_x(invoice("urn:cen.eu:en16931:2017"));
        assert_eq!(
            refused(document().factur_x_version(" ")),
            "the document does not meet Factur-X: the invoice version is empty"
        );
        assert_eq!(
            refused(document().factur_x_description("")),
            "the document does not meet Factur-X: the invoice description is empty"
        );
        assert_eq!(
            refused(document().factur_x_version("3.0\u{0}")),
            "the document does not meet Factur-X: the invoice version contains control characters"
        );
        assert_eq!(
            refused(document().factur_x_description("XRechnung\nInvoice")),
            "the document does not meet Factur-X: the invoice description contains control characters"
        );
        // Without an invoice, they have nothing to apply to.
        let document = Document::html("<p>Brief</p>").factur_x_version("");
        assert!(engine().render(&document).is_ok());
    }

    #[test]
    fn warns_about_profiles_the_xml_does_not_declare() {
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x_profile(invoice("urn:factur-x.eu:1p0:basicwl"), FacturXProfile::Extended);
        let output = engine().render(&document).expect("rendered");
        assert_eq!(codes(&output), ["invoice.profile-mismatch"]);
        assert!(contains(
            &output.pdf,
            "<fx:ConformanceLevel>EXTENDED</fx:ConformanceLevel>"
        ));

        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x(b"<Invoice/>".to_vec());
        let output = engine().render(&document).expect("rendered");
        assert_eq!(codes(&output), ["invoice.profile-unknown"]);
        assert!(contains(
            &output.pdf,
            "<fx:ConformanceLevel>EN 16931</fx:ConformanceLevel>"
        ));
    }

    #[test]
    fn writes_links_and_bookmarks() {
        let document = Document::html(
            "<style>body { font-family: Inter }</style>\
             <h1>Rechnung</h1><h2 id=summen>Summen</h2><h3>Steuern</h3><h2>Zahlung</h2>\
             <p><a href=\"#summen\">Zu den Summen</a> <a href=\"https://quittio.dev\">Website</a>\
             <a href=\"javascript:alert(1)\">Skript</a></p>",
        )
        .created(CREATED);
        let pdf = engine().render(&document).expect("rendered").pdf;
        assert!(contains(&pdf, "/URI(https://quittio.dev/)"));
        assert!(!contains(&pdf, "javascript"));
        assert!(contains(&pdf, "/Outlines"));
        for title in ["(Rechnung)", "(Summen)", "(Steuern)", "(Zahlung)"] {
            assert!(contains(&pdf, title), "{title}");
        }
        // Two link annotations: the internal one and the web link.
        let links = decoded(&pdf)
            .windows(13)
            .filter(|window| window == b"/Subtype/Link")
            .count();
        assert_eq!(links, 2);
    }

    #[test]
    fn links_to_anchors_without_boxes_of_their_own() {
        // LibreOffice puts `<a name>` around the headings of sheets; others put empty anchors
        // between blocks. Neither has a line in Fagus, but each leads to the content after its
        // start, as in Chrome. Chrome keeps the link to an anchor that is not rendered too (to a
        // destination it never defines); it leads to where the anchor would be.
        let document = Document::html(
            "<style>body { font-family: Inter }</style>\
             <p><a href=\"#t0\">Blatt 1</a> <a href=\"#t1\">Blatt 2</a> <a href=\"#t2\">Weg</a></p>\
             <a name=t0><h1>Kostenstellen</h1></a>\
             <a name=t1></a><h2>Reisekosten</h2>\
             <div style=\"display: none\"><a name=t2>versteckt</a></div>",
        )
        .created(CREATED);
        let pdf = engine().render(&document).expect("rendered").pdf;
        let links = decoded(&pdf)
            .windows(13)
            .filter(|window| window == b"/Subtype/Link")
            .count();
        assert_eq!(links, 3);
    }

    #[test]
    fn splits_link_areas_where_inline_boxes_start_and_end() {
        // As Chrome: the text of a link and each inline box in it are areas of their own.
        let count = |html: &str| {
            let document = Document::html(format!(
                "<style>body {{ font-family: Inter }}</style><p>{html}</p><p id=t>T</p>"
            ))
            .created(CREATED);
            let pdf = engine().render(&document).expect("rendered").pdf;
            decoded(&pdf)
                .windows(13)
                .filter(|window| window == b"/Subtype/Link")
                .count()
        };
        assert_eq!(count("<a href=\"#t\">alpha <b>beta</b> gamma</a>"), 3);
        assert_eq!(count("<a href=\"#t\">alpha<span>beta</span></a>"), 2);
        assert_eq!(count("<a href=\"#t\"><span>beta</span></a>"), 1);
        assert_eq!(count("<a href=\"#t\">alpha beta</a>"), 1);
    }

    #[test]
    fn leaves_out_links_and_text_of_boxes_without_opacity() {
        // Docusaurus' heading anchors have `opacity: 0`: Chrome draws neither their `#` nor
        // their links, but keeps those of a box that is only nearly transparent.
        let document = Document::html(
            "<style>body { font-family: Inter } .anchor { opacity: 0 } .anchor::before { content: 'Raute' }</style>\
             <h2 id=summen>Summen<a class=anchor href=\"#summen\"></a></h2>\
             <p style=\"opacity: 0.001\"><a href=\"https://quittio.dev/a\">Unsichtbar</a></p>\
             <p style=\"opacity: 0.01\"><a href=\"https://quittio.dev/b\">Blass</a></p>",
        )
        .created(CREATED);
        let pdf = engine().render(&document).expect("rendered").pdf;
        let links = decoded(&pdf)
            .windows(13)
            .filter(|window| window == b"/Subtype/Link")
            .count();
        assert_eq!(links, 1);
        assert!(contains(&pdf, "/URI(https://quittio.dev/b)"));
        assert!(!contains(&pdf, "quittio.dev/a"));
    }

    #[test]
    fn reads_woff_and_woff2_fonts() {
        let document = Document::html(
            "<style>
               @font-face { font-family: Web; src: url(Inter-Latin.woff2) format('woff2'); }
               @font-face { font-family: Web; src: url(Inter-Bold-Latin.woff) format('woff'); font-weight: 700; }
               p { font-family: Web }
             </style>
             <p>Normal <b>Fett</b></p>",
        )
        .base_directory(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts"))
        .created(CREATED);
        let output = engine().render(&document).expect("rendered");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        // Both faces are embedded under the family of the fonts.
        assert_eq!(
            decoded(&output.pdf)
                .windows(9)
                .filter(|window| window == b"/FontFile")
                .count(),
            2,
            "two embedded fonts"
        );
    }

    #[test]
    fn embeds_instances_of_variable_fonts() {
        let render = |body: &str| {
            let document = Document::html(format!(
                "<style>
                   @font-face {{ font-family: Web; src: url(Inter-Variable-Latin.woff); font-weight: 100 900; }}
                   p {{ font-family: Web }}
                 </style>{body}"
            ))
            .base_directory(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts"))
            .created(CREATED);
            let output = engine().render(&document).expect("rendered");
            assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
            decoded(&output.pdf)
                .windows(9)
                .filter(|window| window == b"/FontFile")
                .count()
        };
        // One instance for each weight and size on the optical axis.
        assert_eq!(
            render("<p>Normal <b>Fett</b> <span style='font-weight: 350'>Leicht</span></p>"),
            3
        );
        assert_eq!(render("<p>Eins</p><p>Zwei</p>"), 1);
        assert_eq!(render("<p>Text <big>Groß</big></p>"), 2);
        assert_eq!(
            render("<p style='font-optical-sizing: none'>Text <big>Groß</big></p>"),
            1
        );
    }

    #[test]
    fn embeds_pingfang_with_the_outlines_of_its_hvgl_table() {
        // macOS keeps PingFang's glyphs in an `hvgl` table, which PDF fonts cannot hold.
        let path =
            "/System/Library/PrivateFrameworks/FontServices.framework/Resources/Reserved/PingFangUI.ttc";
        let Ok(data) = std::fs::read(path) else { return };
        let mut engine = Engine::new();
        engine.add_font_data(data).expect("PingFang");
        let document =
            Document::html("<p lang='zh-CN' style='font-family: \"PingFang SC\"'>中文测试 <b>粗体</b></p>")
                .created(CREATED);
        let output = engine.render(&document).expect("rendered");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        // TrueType fonts of the instances, named as Core Text names them, with their glyphs.
        assert!(contains(&output.pdf, "+PingFangSC-Regular"));
        assert!(contains(&output.pdf, "+PingFangSC-Semibold"));
        assert!(!contains(&output.pdf, "/Type3"));
        let fonts = decoded(&output.pdf)
            .windows(10)
            .filter(|window| window == b"/FontFile2")
            .count();
        assert_eq!(fonts, 2);
    }

    #[test]
    fn takes_characters_from_the_faces_of_their_unicode_range() {
        let render = |body: &str| {
            let document = Document::html(format!(
                "<style>
                   @font-face {{ font-family: Web; src: url(Inter-Latin.woff2); unicode-range: U+0-2F, U+3A-FF; }}
                   @font-face {{ font-family: Web; src: url(Inter-Bold-Latin.woff); unicode-range: U+30-39; }}
                   p {{ font-family: Web }}
                 </style>{body}"
            ))
            .base_directory(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts"))
            .created(CREATED);
            let output = engine().render(&document).expect("rendered");
            assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
            decoded(&output.pdf)
                .windows(9)
                .filter(|window| window == b"/FontFile")
                .count()
        };
        // Digits come from the second rule's font, the rest from the first one's.
        assert_eq!(render("<p>Rechnung</p>"), 1);
        assert_eq!(render("<p>Rechnung 2026</p>"), 2);
        assert_eq!(render("<p>2026</p>"), 1);
    }

    #[test]
    fn loads_the_first_source_chrome_loads_and_installed_faces() {
        let render = |src: &str| {
            let document = Document::html(format!(
                "<style>@font-face {{ font-family: Web; src: {src} }} p {{ font-family: Web }}</style><p>Rechnung</p>"
            ))
            .base_directory(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts"))
            .created(CREATED);
            let pdf = decoded(&engine().render(&document).expect("rendered").pdf);
            let has = |name: &[u8]| pdf.windows(name.len()).any(|window| window == name);
            (has(b"+Inter-Bold"), has(b"+Inter-Italic"))
        };
        // Formats and technologies Chrome does not load are passed over, unloaded.
        assert_eq!(
            render(
                "url(missing.eot), url(Inter-Latin.woff2) format('x-unknown'), \
                 url(Inter-Latin.woff2) tech(incremental), url(Inter-Bold-Latin.woff) format('woff')"
            ),
            (true, false)
        );
        // `local()` finds an installed face by its full or PostScript name; an unknown one
        // passes to the next source.
        assert_eq!(
            render("local('Inter Italic'), url(Inter-Bold-Latin.woff)"),
            (false, true)
        );
        assert_eq!(
            render("local(Inter-Italic), url(Inter-Bold-Latin.woff)"),
            (false, true)
        );
        assert_eq!(
            render("local('Nowhere Sans'), url(Inter-Bold-Latin.woff)"),
            (true, false)
        );
    }

    #[test]
    fn sets_faces_by_size_adjust_and_their_metric_overrides() {
        let render = |descriptors: &str| {
            let document = Document::html(format!(
                "<style>
                   @font-face {{ font-family: Web; src: url(Inter-Latin.woff2); {descriptors} }}
                   p {{ font: 20px Web; margin: 0 }} span {{ font-size: 10px }}
                 </style><p>Rechnung</p>"
            ))
            .base_directory(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts"))
            .created(CREATED)
            .tagged(false);
            streams(&engine().render(&document).expect("rendered").pdf)
        };
        // The face is set at 150 % of the 20 pixels.
        assert!(render("").contains(" 20 Tf"));
        assert!(render("size-adjust: 150%").contains(" 30 Tf"));
        // A larger ascent moves the baseline down: the text matrix's vertical offset.
        let baseline = |content: &str| -> Option<f32> {
            let matrix = content.split(" Tm").next()?;
            matrix.split_whitespace().last()?.parse().ok()
        };
        let plain = baseline(&render("")).expect("a baseline");
        let lower = baseline(&render("ascent-override: 200%; descent-override: 0%")).expect("a baseline");
        assert!((lower - plain - (40.0 - 19.0)).abs() < 1.0, "{plain} {lower}");
    }

    #[test]
    fn rejects_web_fonts_that_claim_to_be_huge() {
        let mut woff2 = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/webfonts/Inter-Latin.woff2"
        ))
        .expect("font");
        woff2[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(fagus_text::unpack(woff2).is_err());
    }

    #[test]
    fn checks_pdf_a() {
        // PDF/A-2 allows only PDF/A files as attachments.
        let document = Document::html("<p>Anhang</p>")
            .created(CREATED)
            .conformance(Conformance::PdfA2b)
            .attach(
                Attachment::new("daten.csv", "text/csv", b"a;b".to_vec())
                    .relationship(Relationship::Supplement),
            );
        let error = engine().render(&document).expect_err("not PDF/A-2");
        assert!(
            matches!(error, Error::Pdf(PdfError::Conformance { .. })),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "the document does not meet PDF/A-2b: PDF/A-2 allows only PDF/A files as attachments; \
             attach `daten.csv` to a PDF/A-3 document instead"
        );
        // PDF/A-3 takes it, with a description.
        let error = engine()
            .render(&document.clone().conformance(Conformance::PdfA3b))
            .expect_err("no description");
        assert_eq!(
            error.to_string(),
            "the document does not meet PDF/A-3b: the attached file `daten.csv` has no \
             description, which PDF/A-3 and PDF/UA require"
        );
    }

    /// A PNG of a red and a blue pixel with an alpha channel, both opaque.
    const OPAQUE_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR42mP4z8AAQv8BD/kD/Zh51wAAAAAASUVORK5CYII=";

    /// A PNG of one red pixel at half its opacity.
    const TRANSLUCENT_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DQAAAEgQGA8wvcoAAAAABJRU5ErkJggg==";

    /// A PNG of one red pixel, with 16 bits per component.
    const SIXTEEN_BIT_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABEAIAAADA54+dAAAADUlEQVR42mP4/58BCAAK/AH/zJeXGQAAAABJRU5ErkJggg==";

    #[test]
    fn writes_pdf_a_1b_without_transparency() {
        // What PDF/A-1 can take: hidden content, isolation and an image whose alpha channel is
        // all opaque are drawn without transparency, an image of 16 bits per component (PDF 1.5)
        // with 8.
        let html = format!(
            "<title>Archiv</title><p>Archiv</p><p style=\"opacity: 0\">verborgen</p>\
             <div style=\"width: 0; height: 0; overflow: hidden\"><p>abgeschnitten</p></div>\
             <div style=\"isolation: isolate\"><p>isoliert</p></div>\
             <img src=\"{OPAQUE_PNG}\" alt=\"\"><img src=\"{SIXTEEN_BIT_PNG}\" alt=\"\">\
             <p><a href=\"https://quittio.dev\">Link</a></p>"
        );
        let document = Document::html(&html)
            .created(CREATED)
            .conformance(Conformance::PdfA1b);
        let output = engine().render(&document).expect("rendered");
        let pdf = &output.pdf;
        assert!(pdf.starts_with(b"%PDF-1.4\n"), "{:?}", &pdf[..9]);
        assert!(contains(
            pdf,
            "<pdfaid:part>1</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>"
        ));
        // PDF 1.4 has no object streams and no cross-reference streams.
        for forbidden in [
            "/ObjStm",
            "/XRef",
            "/SMask",
            "/Transparency",
            "/BM",
            "/CA ",
            "/ca ",
        ] {
            assert!(!contains(pdf, forbidden), "{forbidden}");
        }
        assert!(contains(pdf, "\nxref\n0 "));
        assert!(contains(pdf, "trailer\n<</Size "));
        assert!(contains(pdf, "/ViewerPreferences<</DisplayDocTitle true>>"));
        assert!(contains(pdf, "/StructTreeRoot"), "tagged unless told otherwise");
        assert!(pdf.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn refuses_transparency_in_pdf_a_1b() {
        let html = format!(
            "<p style=\"color: rgb(0 0 0 / 50%)\">grau</p>\
             <p style=\"break-before: page; opacity: 0.5\">halb</p><img src=\"{TRANSLUCENT_PNG}\" alt=\"\">"
        );
        let document = Document::html(&html)
            .created(CREATED)
            .conformance(Conformance::PdfA1b);
        let error = engine().render(&document).expect_err("transparent");
        assert!(
            matches!(error, Error::Pdf(PdfError::Conformance { .. })),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "the document does not meet PDF/A-1b: PDF/A-1 forbids transparency, which the \
             document uses for `opacity` (first on page 2), text in translucent colors or color \
             fonts (first on page 1) and images with transparent pixels (first on page 2); \
             PDF/A-2b and later allow it"
        );
        // PDF/A-2b keeps it.
        engine()
            .render(&document.clone().conformance(Conformance::PdfA2b))
            .expect("PDF/A-2b");
    }

    #[test]
    fn writes_pdf_a_4() {
        for (conformance, level) in [
            (Conformance::PdfA4, ""),
            (Conformance::PdfA4e, "<pdfaid:conformance>E</pdfaid:conformance>"),
            (Conformance::PdfA4f, "<pdfaid:conformance>F</pdfaid:conformance>"),
        ] {
            let document = Document::html("<title>Archiv</title><p>Archiv</p>")
                .created(CREATED)
                .conformance(conformance);
            let pdf = engine().render(&document).expect("rendered").pdf;
            assert!(pdf.starts_with(b"%PDF-2.0\n"));
            assert!(contains(
                &pdf,
                &format!("<pdfaid:part>4</pdfaid:part><pdfaid:rev>2020</pdfaid:rev>{level}<xmpTPg:NPages>")
            ));
            // PDF/A-4 forbids the document information dictionary: the title is in XMP alone.
            let tail = String::from_utf8_lossy(&pdf[pdf.len() - 400..]);
            let trailer = &tail[tail.rfind("/Type/XRef").expect("cross-reference stream")..];
            assert!(!trailer.contains("/Info"), "{}: {trailer}", conformance.name());
            assert!(!contains(&pdf, "/Producer("), "{}", conformance.name());
            assert!(contains(
                &pdf,
                "<dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">Archiv"
            ));
            assert!(contains(&pdf, "<pdf:PDFVersion>2.0</pdf:PDFVersion>"));
            assert_eq!(
                contains(&pdf, "/EmbeddedFiles"),
                conformance == Conformance::PdfA4f,
                "{}",
                conformance.name()
            );
        }
        // PDF/A-4f takes attachments, PDF/A-4 and PDF/A-1b do not.
        let document = Document::html("<p>Anhang</p>").created(CREATED).attach(
            Attachment::new("daten.csv", "text/csv", b"a;b".to_vec())
                .description("Daten")
                .relationship(Relationship::Supplement),
        );
        let pdf = engine()
            .render(&document.clone().conformance(Conformance::PdfA4f))
            .expect("PDF/A-4f")
            .pdf;
        assert!(contains(&pdf, "/AFRelationship/Supplement"));
        for (conformance, message) in [
            (
                Conformance::PdfA4,
                "the document does not meet PDF/A-4: PDF/A-4 allows only PDF/A files as \
                 attachments; attach `daten.csv` to a PDF/A-4f document instead",
            ),
            (
                Conformance::PdfA1b,
                "the document does not meet PDF/A-1b: PDF/A-1 allows no attachments; attach \
                 `daten.csv` to a PDF/A-3 or PDF/A-4f document instead",
            ),
        ] {
            let error = engine()
                .render(&document.clone().conformance(conformance))
                .expect_err("no attachments");
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn keeps_pdf_ua_1_to_pdf_a_2_and_3() {
        let document = Document::html("<html lang=\"de\"><title>Brief</title><h1>Brief</h1>")
            .created(CREATED)
            .accessible();
        for conformance in [Conformance::PdfA1b, Conformance::PdfA4, Conformance::PdfA4f] {
            let error = engine()
                .render(&document.clone().conformance(conformance))
                .expect_err("PDF/UA-1 is PDF 1.7");
            assert_eq!(
                error.to_string(),
                format!(
                    "the PDF could not be written: {} and PDF/UA-1 need different PDF versions and \
                     cannot be written together (PDF/UA-1 goes with PDF/A-2 and PDF/A-3)",
                    conformance.name()
                )
            );
        }
        engine()
            .render(&document.conformance(Conformance::PdfA3a))
            .expect("PDF/A-3a and PDF/UA-1");
    }

    #[test]
    fn keeps_factur_x_on_pdf_a_3() {
        let document = Document::html("<p>Rechnung</p>")
            .created(CREATED)
            .factur_x(invoice("urn:cen.eu:en16931:2017"));
        let output = engine()
            .render(&document.clone().conformance(Conformance::PdfA1b))
            .expect("raised");
        assert_eq!(
            output.diagnostics[0].message,
            "PDF/A-1b is written as PDF/A-3b because Factur-X attaches its XML, which only PDF/A-3 allows"
        );
        assert!(contains(
            &output.pdf,
            "<pdfaid:part>3</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>"
        ));
        let error = engine()
            .render(&document.conformance(Conformance::PdfA4f))
            .expect_err("Factur-X requires PDF/A-3");
        assert_eq!(
            error.to_string(),
            "the document does not meet Factur-X: the invoice must be PDF/A-3, not PDF/A-4f \
             (choose 3b, 3u or 3a)"
        );
    }

    #[test]
    fn keeps_the_diagnostics_of_failures() {
        // PDF/UA refuses a document without a title; the CSS warning found before stays.
        let document = Document::html("<style>p { colr: red }</style><p>Brief</p>")
            .created(CREATED)
            .accessible();
        let failure = engine()
            .render_with_diagnostics(&document)
            .expect_err("not PDF/UA");
        assert!(
            matches!(failure.error, Error::Pdf(PdfError::Conformance { .. })),
            "{failure}"
        );
        assert_eq!(failure.to_string(), failure.error.to_string());
        let codes: Vec<_> = failure
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        assert_eq!(codes, ["css.unknown-property"]);
        // A PDF/A refusal when the PDF is written keeps them too.
        let document = Document::html("<style>p { colr: red }</style><p>Anhang</p>")
            .created(CREATED)
            .conformance(Conformance::PdfA2b)
            .attach(
                Attachment::new("daten.csv", "text/csv", b"a;b".to_vec())
                    .relationship(Relationship::Supplement),
            );
        let failure = engine()
            .render_with_diagnostics(&document)
            .expect_err("not PDF/A-2");
        assert_eq!(failure.diagnostics.len(), 1, "{:?}", failure.diagnostics);
    }

    #[test]
    fn keeps_the_web_font_cache_bounded() {
        let data = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fonts/Inter-Regular.ttf"
            ))
            .expect("font"),
        );
        let face = Arc::new(
            Face::from_rule(
                Arc::clone(&data),
                0,
                &fagus_text::Descriptors {
                    family: "Web".to_owned(),
                    ..fagus_text::Descriptors::default()
                },
            )
            .expect("face"),
        );
        let mut cache = FaceCache::default();
        let key = |index: u64| FaceKey {
            data: index,
            descriptors: "web".to_owned(),
        };
        let count = FaceCache::MAX_BYTES / data.len() * 2;
        for index in 0..count as u64 {
            cache.insert(key(index), Arc::clone(&face));
        }
        assert!(cache.bytes <= FaceCache::MAX_BYTES);
        assert!(cache.get(&key(0)).is_none(), "the oldest went first");
        assert!(cache.get(&key(count as u64 - 1)).is_some());
    }

    #[test]
    fn survives_absurd_values() {
        // Infinite and huge values would overflow krilla's arithmetic, and a panic while a page
        // is drawn aborts the process: such content is clipped away or left out instead.
        for css in [
            "translate: 1e305px",
            "rotate: 1e305deg",
            "translate: calc(NaN * 1px)",
            "transform: scale(1e5) scale(1e5) scale(1e5)",
            "box-shadow: 1e305px 6px 1px black",
            "background: conic-gradient(from 494967296e30deg, red, blue)",
            "background: linear-gradient(1e305deg, red, blue)",
        ] {
            let html = format!("<div style=\"{css}\"><div style=\"transform: scale(2)\">Text</div></div>");
            engine().render(&Document::html(html)).expect(css);
        }
        // Spans beyond what fits a PDF integer are clamped as in HTML.
        let table =
            "<table><tr><th colspan=4294967295 rowspan=2147483648>Kopf</th></tr><tr><td>1</td></tr></table>";
        engine().render(&Document::html(table)).expect("huge spans");
    }

    #[test]
    fn renders_deep_documents_on_small_stacks() {
        // The engine renders on threads of its own, with a stack for the deepest documents.
        let html = format!(
            "<style>div {{ padding: 1px; border: 1px solid }}</style>{}<p>tief</p>{}",
            "<div>".repeat(2000),
            "</div>".repeat(2000)
        );
        let engine = engine();
        let output = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || engine.render(&Document::html(html)))
            .expect("thread")
            .join()
            .expect("no stack overflow")
            .expect("rendered");
        // Beyond the deepest level, the boxes follow each other.
        assert!(output.pages > 1);
    }

    /// A blue PNG of 2 × 2 pixels.
    const PNG: &str = "data:image/png;base64,\
        iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGNQd7wJRAwQCgAgYgUFHdy6BAAAAABJRU5ErkJggg==";

    #[test]
    fn writes_pdf_a_with_images() {
        // PDF/A does not allow viewers to smooth images.
        let document = Document::html(format!("<img src=\"{PNG}\" style=\"width: 20mm\">"))
            .created(CREATED)
            .conformance(Conformance::PdfA3b);
        let pdf = engine().render(&document).expect("PDF/A-3b").pdf;
        assert!(!contains(&pdf, "/Interpolate true"));
    }

    #[test]
    fn writes_images_without_interpolation_as_chrome() {
        // Chrome leaves smoothing to viewers, whatever `image-rendering` says (Chrome 157).
        let document = Document::html(format!(
            "<img src=\"{PNG}\" style=\"width: 20mm\"><img src=\"{PNG}\" style=\"width: 2mm; image-rendering: pixelated\">"
        ))
        .created(CREATED);
        let pdf = engine().render(&document).expect("rendered").pdf;
        assert!(!contains(&pdf, "/Interpolate"));
    }

    #[test]
    fn writes_pdf_a_with_shadows_and_any_gradient() {
        // Blurred shadows are images, conic gradients images in PDF/A, and repeating gradients
        // repeat their stops: PDF/A allows neither interpolation nor PostScript functions.
        let document = Document::html(
            "<div style=\"width: 40mm; height: 20mm; box-shadow: 0 2px 12px rgb(0 0 0 / 30%), \
             inset 0 0 4px red\"></div>\
             <div style=\"width: 40mm; height: 20mm; \
             background: repeating-linear-gradient(45deg, #2563eb 0 8px, #bfdbfe 8px 16px)\"></div>\
             <div style=\"width: 40mm; height: 20mm; \
             background: conic-gradient(from 90deg at 30% 50%, red, yellow, green, blue, red)\"></div>",
        )
        .created(CREATED)
        .conformance(Conformance::PdfA3b);
        let pdf = engine().render(&document).expect("PDF/A-3b").pdf;
        assert!(!contains(&pdf, "/FunctionType 4"));
    }

    #[test]
    fn tags_the_text_of_underlined_links() {
        // The underline is decoration; the text drawn over it is the link's content.
        let html = "<html lang=de><p>Vor <a href=\"https://quittio.dev\">Link</a> nach</p>";
        let pdf = engine()
            .render(&Document::html(html).created(CREATED))
            .expect("rendered")
            .pdf;
        let content = streams(&pdf);
        assert_eq!(content.matches("<</MCID ").count(), 3, "{content}");
        assert!(content.contains("/Artifact"), "the underline is an artifact");
    }

    #[test]
    fn tags_positioned_boxes_of_the_page_area_but_not_margin_boxes() {
        // A layer as large as the sheet in a margin box (fold marks outside the page area) is a
        // header artifact, whatever it holds; an address field positioned in the page area is
        // content.
        let html = "<html lang=de><style>
               @page { size: A4; margin: 15mm 20mm; @top-left-corner { content: element(sheet) } }
               body { margin: 0 } .sheet { position: running(sheet) }
               .address { position: absolute; top: calc(45mm - 15mm); left: 0 }</style>
             <div class=sheet><div style='position: relative; width: 210mm; height: 297mm'>\
               <div style='position: absolute; top: 105mm'>Falz</div></div></div>
             <div class=address>Kunde AG</div><p>Text</p>";
        let pdf = engine()
            .render(&Document::html(html).created(CREATED))
            .expect("rendered")
            .pdf;
        let content = streams(&pdf);
        assert_eq!(content.matches("<</MCID ").count(), 2, "{content}");
        assert!(
            content.contains("/Artifact<</Attached[/Top]/Subtype/Header/Type/Pagination>>BDC"),
            "{content}"
        );
    }

    #[test]
    fn writes_what_the_document_says_about_itself() {
        let html = "<head><title> Rechnung\n RE-1 </title><meta name=author content=\"Muster GmbH\">\
                    <meta name=Description content=\"Rechnung für Oktober\">\
                    <meta name=keywords content=\"Rechnung, Oktober,\"></head><p>Text";
        let mut german = engine();
        german.set_locale(Some("de-DE".to_owned()));
        let pdf = german
            .render(&Document::html(html).created(CREATED))
            .expect("rendered")
            .pdf;
        for entry in [
            "/Title(Rechnung RE-1)",
            "/Author(Muster GmbH)",
            "/Keywords(Rechnung, Oktober)",
            "/Lang(de-DE)",
            "/ViewerPreferences<</DisplayDocTitle true>>",
        ] {
            assert!(contains(&pdf, entry), "{entry}");
        }
        // `lang` comes before the locale; without a title the viewer shows the file name.
        let pdf = german
            .render(&Document::html("<html lang=en><p>Text").created(CREATED))
            .expect("rendered")
            .pdf;
        assert!(contains(&pdf, "/Lang(en)"));
        assert!(!contains(&pdf, "/DisplayDocTitle"));
    }

    #[test]
    fn links_images_and_outlines_aria_headings() {
        let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let html = format!(
            "<h1>Rechnung</h1><div role=heading aria-level=2>Positionen</div>\
             <p><a href=\"https://quittio.dev\"><img src=\"{image}\" alt=Logo width=80 height=40></a></p>"
        );
        let pdf = engine()
            .render(&Document::html(html).created(CREATED))
            .expect("rendered")
            .pdf;
        // The image reaches above the line of the link, so it is an area of its own, as in Chrome.
        let links = decoded(&pdf)
            .windows(13)
            .filter(|window| window == b"/Subtype/Link")
            .count();
        assert_eq!(links, 2);
        assert!(contains(&pdf, "(Positionen)"));
        assert!(contains(&pdf, "/Count 1"), "the heading nests below the <h1>");
    }

    #[test]
    fn leaves_aria_headings_with_bookmark_level_none_out_of_the_outline() {
        let html = "<style>.quiet { bookmark-level: none }</style><h1>Rechnung</h1>\
                    <div role=heading aria-level=2>Positionen</div>\
                    <div role=heading aria-level=2 class=quiet>Intern</div>\
                    <h2 class=quiet>Anhang</h2>";
        let pdf = engine()
            .render(&Document::html(html).created(CREATED).tagged(false))
            .expect("rendered")
            .pdf;
        assert!(contains(&pdf, "(Positionen)"));
        assert!(!contains(&pdf, "(Intern)"));
        assert!(!contains(&pdf, "(Anhang)"));
    }

    #[test]
    fn exports_the_errors_inside_error() {
        let mut engine = Engine::new();
        let error = engine.add_font_data(b"no font".to_vec()).expect_err("not a font");
        assert!(matches!(error, Error::Font(FontError::InvalidFont)));
        let refused = engine
            .render(&Document::html("<p>Text</p>").accessible())
            .expect_err("no title");
        assert!(matches!(refused, Error::Pdf(PdfError::Conformance { .. })));
    }

    #[test]
    fn warns_of_characters_without_glyphs_and_keeps_their_text() {
        // The test fonts are Inter's, which has no CJK and no snowman.
        let html = "<p>Hallo</p>\n<p>漢字 ☃ 漢</p>";
        let output = engine()
            .render(&Document::html(html).created(CREATED))
            .expect("rendered");
        assert_eq!(codes(&output), ["font.missing-glyph"]);
        let warning = &output.diagnostics[0];
        assert_eq!(warning.line, 2);
        assert_eq!(
            warning.message,
            "no font has glyphs for 3 characters in 4 places: `漢` (U+6F22), `字` (U+5B57), `☃` \
             (U+2603); they are drawn as empty boxes (add a font that has them)"
        );
        // Every empty box keeps its character for text extraction, the first one too.
        let mut streams = String::new();
        let pdf = &output.pdf;
        let mut at = 0;
        while let Some(start) = pdf[at..].windows(7).position(|window| window == b"stream\n") {
            let body = &pdf[at + start + 7..];
            let mut inflated = Vec::new();
            if std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(body), &mut inflated).is_ok() {
                streams.push_str(&String::from_utf8_lossy(&inflated).to_ascii_lowercase());
            }
            at += start + 7;
        }
        assert_eq!(streams.matches("/actualtext<feff6f22>").count(), 2, "{streams}");
        assert_eq!(streams.matches("/actualtext<feff5b57>").count(), 1);
        assert_eq!(streams.matches("/actualtext<feff2603>").count(), 1);
        // PDF/A refuses them, naming each character.
        let refused = engine()
            .render(
                &Document::html(html)
                    .created(CREATED)
                    .conformance(Conformance::PdfA2b),
            )
            .expect_err("refused");
        let message = refused.to_string();
        assert!(message.contains("Inter has no glyph for `漢`"), "{message}");
        assert!(!message.contains("``"), "{message}");
    }

    #[test]
    fn writes_tagged_pdf() {
        let html = "<!doctype html><html lang=de><title>Rechnung</title><h1>Rechnung</h1><p>Danke.</p>";
        // Tagged by default.
        let tagged = engine()
            .render(&Document::html(html).created(CREATED))
            .expect("tagged")
            .pdf;
        assert!(contains(&tagged, "/StructTreeRoot"));
        assert!(contains(&tagged, "/ParentTree"));
        let plain = engine()
            .render(&Document::html(html).created(CREATED).tagged(false))
            .expect("plain")
            .pdf;
        assert!(!contains(&plain, "/StructTreeRoot"));
        assert!(!contains(&plain, "/ParentTree"));
    }

    #[test]
    fn checks_pdf_ua() {
        let page = |head: &str, image: &str| {
            Document::html(format!(
                "<!doctype html><html lang=de><head>{head}</head><body><h1>Rechnung</h1>\
                 <p>Bitte überweisen Sie den Betrag. <a href=\"https://quittio.dev\">Quittio</a></p>\
                 <img src=\"{PNG}\"{image}></body></html>"
            ))
            .created(CREATED)
            .accessible()
        };
        let title = "<title>Rechnung RE-1</title>";
        let pdf = engine()
            .render(&page(title, " alt=\"Logo der Muster GmbH\""))
            .expect("PDF/UA")
            .pdf;
        assert!(contains(&pdf, "pdfuaid:part"));
        assert!(contains(&pdf, "/StructTreeRoot"));
        // Decoration needs no text.
        engine().render(&page(title, " alt=\"\"")).expect("decoration");

        let message = |document: &Document| match engine().render(document) {
            Err(Error::Pdf(PdfError::Conformance { problems, .. })) => problems,
            other => panic!("not refused: {:?}", other.map(|output| output.pages)),
        };
        assert!(message(&page(title, "")).contains("has no alternative text"));
        assert!(message(&page("", " alt=\"Logo\"")).contains("has no title"));
        let without_language = Document::html(format!("{title}<p>Text</p>")).accessible();
        assert!(message(&without_language).contains("has no language"));

        // Headings start at h1 and skip no level on the way down; going up is fine.
        let headings = |headings: &str| {
            Document::html(format!("<html lang=de>{title}{headings}"))
                .created(CREATED)
                .accessible()
        };
        engine()
            .render(&headings(
                "<h1>A</h1><h2>B</h2><h3>C</h3><h2>D</h2><h1>E</h1><h2>F</h2>",
            ))
            .expect("in order");
        assert!(message(&headings("<h2>A</h2><h3>B</h3>")).contains("an <h2> in line 1, is not an <h1>"));
        assert!(message(&headings("<h1>A</h1><h3>B</h3>")).contains("follows an <h1>"));
        // Hidden headings are not met.
        engine()
            .render(&headings("<h1>A</h1><h3 hidden>B</h3><h2>C</h2>"))
            .expect("hidden");
    }

    #[test]
    fn draws_text_shadows() {
        let render = |css: &str| {
            engine()
                .render(
                    &Document::html(format!("<p style='font: 20px Inter; {css}'>Schatten</p>"))
                        .created(CREATED)
                        .tagged(false),
                )
                .expect("rendered")
                .pdf
        };
        let images = |pdf: &[u8]| {
            pdf.windows(b"/Subtype/Image".len())
                .filter(|window| window == b"/Subtype/Image")
                .count()
        };
        // A sharp shadow is text again, a blurred one an image with a soft mask.
        assert_eq!(images(&render("text-shadow: 2px 2px red")), 0);
        assert!(images(&render("text-shadow: 0 0 4px blue")) >= 1);
        assert_eq!(images(&render("")), 0);
    }

    /// The content streams of a PDF, decompressed.
    fn streams(pdf: &[u8]) -> String {
        String::from_utf8_lossy(&stream_bytes(pdf)).into_owned()
    }

    /// The streams of a PDF that draw text, decompressed.
    fn page_streams(pdf: &[u8]) -> String {
        let mut pages = String::new();
        let mut rest = pdf;
        while let Some(start) = rest.windows(7).position(|window| window == b"stream\n") {
            rest = &rest[start + 7..];
            let end = rest
                .windows(9)
                .position(|window| window == b"endstream")
                .unwrap_or(rest.len());
            let mut inflated = Vec::new();
            let _ =
                std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&rest[..end]), &mut inflated);
            let text = String::from_utf8_lossy(&inflated);
            if ["BT ", " Tf", "ET"]
                .iter()
                .all(|operator| text.contains(operator))
            {
                pages.push_str(&text);
            }
            rest = &rest[(end + 9).min(rest.len())..];
        }
        pages
    }

    /// The streams of a PDF, decompressed, as they are.
    fn stream_bytes(pdf: &[u8]) -> Vec<u8> {
        let mut all = Vec::new();
        let mut rest = pdf;
        while let Some(start) = rest.windows(7).position(|window| window == b"stream\n") {
            rest = &rest[start + 7..];
            let end = rest
                .windows(9)
                .position(|window| window == b"endstream")
                .unwrap_or(rest.len());
            let mut decoder = flate2::read::ZlibDecoder::new(&rest[..end]);
            let _ = std::io::Read::read_to_end(&mut decoder, &mut all);
            // Past `endstream`, which ends in `stream` too.
            rest = &rest[(end + 9).min(rest.len())..];
        }
        all
    }

    #[test]
    fn keeps_stylesheets_and_images_for_the_next_documents_as_long_as_they_are_the_same() {
        let engine = engine();
        let images = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus/images/");
        let read = |name: &str| std::fs::read(format!("{images}{name}")).expect("image");
        let svg = |color: &str| {
            format!(
                "<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'><rect width='40' height='20' fill='{color}'/><text y='15'>Logo</text></svg>"
            )
            .into_bytes()
        };
        let render = |color: &str, logo: Vec<u8>, png: Vec<u8>, sheet: &str| {
            let html = format!(
                "<style>p {{ color: {color} }}</style><p>Hallo <img src=logo.svg> <img src=a.png width=20></p>"
            );
            let output = engine
                .render(
                    &Document::html(html)
                        .resource("logo.svg", logo)
                        .resource("a.png", png)
                        .stylesheet("app.css", sheet)
                        .created(CREATED)
                        .tagged(false),
                )
                .expect("rendered");
            assert!(output.diagnostics.is_empty(), "{:?}", codes(&output));
            stream_bytes(&output.pdf)
        };
        let first = render("#ff0000", svg("#00ff00"), read("logo.png"), "img { margin: 1px }");
        assert_eq!(
            first,
            render("#ff0000", svg("#00ff00"), read("logo.png"), "img { margin: 1px }")
        );
        // Another text, logo, image or stylesheet under the same names is another document.
        for other in [
            render("#0000ff", svg("#00ff00"), read("logo.png"), "img { margin: 1px }"),
            render("#ff0000", svg("#ffff00"), read("logo.png"), "img { margin: 1px }"),
            render(
                "#ff0000",
                svg("#00ff00"),
                read("pattern.png"),
                "img { margin: 1px }",
            ),
            render("#ff0000", svg("#00ff00"), read("logo.png"), "img { margin: 2px }"),
        ] {
            assert_ne!(first, other);
        }
        assert_eq!(
            first,
            render("#ff0000", svg("#00ff00"), read("logo.png"), "img { margin: 1px }")
        );
    }

    #[test]
    fn writes_form_controls_in_the_engines_locale_unless_lang_says_otherwise() {
        let render = |engine: &Engine, html: &str| {
            let pdf = engine
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            // The locale is also the language of a document without one: only the pages count.
            page_streams(&pdf)
        };
        let controls = "<input type=submit> <input type=date value=2026-11-04>";
        let english = render(&engine(), controls);
        let mut german = engine();
        german.set_locale(Some("de-DE".to_owned()));
        let by_option = render(&german, controls);
        assert_ne!(english, by_option);
        // (`lang` on the body, which leaves the language of the PDF alone.)
        assert_eq!(by_option, render(&engine(), &format!("<body lang=de>{controls}")));
        // `lang` comes first; an empty option is no option.
        assert_eq!(english, render(&german, &format!("<body lang=en>{controls}")));
        german.set_locale(Some(String::new()));
        assert_eq!(english, render(&german, controls));
    }

    #[test]
    fn blends_boxes_and_background_layers() {
        let render = |html: &str| {
            let pdf = engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "")
        };
        let blended = render(
            "<div style='background: red; height: 50px'><div style='mix-blend-mode: multiply;
               background: lime; height: 20px'></div></div>",
        );
        assert!(blended.contains("/BM/Multiply"), "a blend mode");
        let layers = render(
            "<div style='height: 50px; background-image: linear-gradient(red, blue), linear-gradient(lime, red);
               background-blend-mode: screen'></div>",
        );
        assert!(layers.contains("/BM/Screen"));
        assert!(!render("<div style='isolation: isolate; height: 10px'>x</div>").contains("/BM/"));
    }

    #[test]
    fn clips_to_rounded_corners() {
        const PIXELS: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR4nGP4z8AAQg0AD3oDfnfpf5cAAAAASUVORK5CYII=";
        let render = |html: &str| {
            streams(
                &engine()
                    .render(&Document::html(html).created(CREATED).tagged(false))
                    .expect("rendered")
                    .pdf,
            )
        };
        // Whether a clip's path has curves: those of the corners.
        let rounded = |content: &str| {
            let clips: Vec<&str> = content.split("W\nn").collect();
            clips.len() > 1
                && clips[..clips.len() - 1]
                    .iter()
                    .any(|before| before.rsplit('q').next().is_some_and(|path| path.contains(" c")))
        };
        let overflow = "<div style='overflow: hidden; width: 100px; height: 50px; {}'><div style='height: 100px; background: red'></div></div>";
        assert!(rounded(&render(
            &overflow.replace("{}", "border-radius: 20px; border: 2px solid")
        )));
        assert!(!rounded(&render(&overflow.replace("{}", ""))));
        // Images are clipped to the curve of their content edge.
        assert!(rounded(&render(&format!(
            "<img src='{PIXELS}' style='width: 40px; border-radius: 50%'>"
        ))));
        // Padding as wide as the radius leaves square corners at the content edge.
        assert!(!rounded(&render(&format!(
            "<img src='{PIXELS}' style='width: 40px; border-radius: 4px; padding: 4px; overflow: visible'>"
        ))));
    }

    #[test]
    fn answers_container_queries_with_the_sizes_of_a_layout() {
        let render = |width: &str| {
            let html = format!(
                "<style>.card {{ container-type: inline-size; width: {width} }}
                   @container (min-width: 300px) {{ p {{ color: red }} }}
                   .bar {{ width: 50cqw; height: 10px; background: blue }}</style>
                 <div class=card><p>Text</p><div class=bar></div></div>"
            );
            let pdf = engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            streams(&pdf)
        };
        let wide = render("400px");
        assert!(wide.contains("1 0 0 rg"), "the rule applies: {wide}");
        // Half of the container: 200px, as 150 points.
        assert!(wide.contains(" 0 0 1 rg") && wide.contains("245 "), "{wide}");
        let narrow = render("200px");
        assert!(!narrow.contains("1 0 0 rg"));
    }

    #[test]
    fn takes_the_viewport_from_the_first_page() {
        // The first page is a named page without margins: `50vw` is half of its 400px, not of
        // the 300px the default page leaves, as in Chrome.
        let html = "<style>@page { size: 400px 600px; margin: 50px } @page cover { margin: 0 }
               body { margin: 0 } .cover { page: cover }
               .half { container-type: inline-size; width: 50vw }
               @container (min-width: 199px) { .half p { color: red } }</style>
             <div class=cover><div class=half><p>Half</p></div></div>";
        let pdf = engine()
            .render(&Document::html(html).created(CREATED).tagged(false))
            .expect("rendered")
            .pdf;
        let content = streams(&pdf);
        assert!(content.contains("1 0 0 rg"), "50vw of the first page: {content}");
        // As where `:nth()` selects the first page of the document or of its page group.
        for selector in [":nth(1)", ":nth(1 of cover)"] {
            let html = html
                .replace("@page cover", &format!("@page {selector}"))
                .replace("page: cover", "page: cover; break-before: page");
            let pdf = engine()
                .render(&Document::html(&html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            let content = streams(&pdf);
            assert!(content.contains("1 0 0 rg"), "{selector}: {content}");
        }
    }

    #[test]
    fn makes_sheets_as_large_as_chrome_does() {
        // 80mm by 200mm, 226.77pt by 566.93pt: whole points, then 1/300 inch.
        let pdf = engine()
            .render(
                &Document::html("<style>@page { size: 80mm 200mm; margin: 4mm }</style><p>Bon</p>")
                    .created(CREATED)
                    .tagged(false),
            )
            .expect("rendered")
            .pdf;
        let objects = String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "");
        assert!(objects.contains("/MediaBox[00227.04567.12]"), "{objects}");
    }

    #[test]
    fn keeps_the_page_size_before_a_size_that_chrome_does_not_take_and_says_why() {
        // `80mm auto` and `A6` are dropped as in Chrome: the page keeps 80mm by 200mm.
        let output = engine()
            .render_with_diagnostics(
                &Document::html(
                    "<style>@page { size: 80mm 200mm; margin: 4mm }\n@page { size: 80mm auto }\n@page { size: A6 }</style><p>Bon</p>",
                )
                .created(CREATED)
                .tagged(false),
            )
            .expect("rendered");
        let objects = String::from_utf8_lossy(&decoded(&output.pdf)).replace(' ', "");
        assert!(objects.contains("/MediaBox[00227.04567.12]"), "{objects}");
        let found: Vec<_> = output
            .diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.code, diagnostic.line, diagnostic.message.as_str()))
            .collect();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!((found[0].0, found[0].1), ("css.invalid-value", 2), "{found:?}");
        assert!(found[0].2.contains("give both lengths"), "{found:?}");
        assert_eq!((found[1].0, found[1].1), ("css.invalid-value", 3), "{found:?}");
        assert!(found[1].2.contains("write `105mm 148mm`"), "{found:?}");
    }

    #[test]
    fn reports_where_elements_and_table_rows_ended_up_only_when_asked() {
        // Pages of 300pt by 225pt with margins of 15pt: a title 50px (37.5pt) tall, a table of
        // rows 20px (15pt) tall with a header that repeats, and a total after it.
        let rows = (0..20).fold(String::new(), |mut rows, row| {
            let id = if row == 7 { " id=eighth" } else { "" };
            let _ = std::fmt::Write::write_fmt(&mut rows, format_args!("<tr{id}><td>Row {row}</td></tr>"));
            rows
        });
        let html = format!(
            "<!DOCTYPE html><style>@page {{ size: 400px 300px; margin: 20px }}
             body {{ margin: 0; font: 14px/20px Inter }} h1 {{ margin: 0; height: 50px; font-size: 14px }}
             table {{ border-spacing: 0 }} td, th {{ padding: 0 }}</style>
             <h1 id=title>Invoice</h1><table id=items><thead><tr><th>Item</th></tr></thead>
             <tbody>{rows}</tbody></table><p id=total style='margin: 0'>Total <b id=sum>12.00</b></p>
             <span id=hidden style='display: none'></span>"
        );
        let document = Document::html(html).created(CREATED).tagged(false);
        let output = engine().render(&document).expect("rendered");
        assert_eq!(output.layout, None);
        let output = engine().render(&document.report_layout(true)).expect("rendered");
        let layout = output.layout.expect("layout");
        assert_eq!(output.pages, 2);
        assert_eq!(
            layout.pages,
            [LayoutPage {
                width: 300.0,
                height: 225.0
            }; 2]
        );
        let ids: Vec<&str> = layout
            .elements
            .iter()
            .map(|element| element.id.as_str())
            .collect();
        assert_eq!(ids, ["title", "items", "eighth", "total", "sum", "hidden"]);
        let title = layout.element("title").expect("title");
        assert_eq!(title.tag, "h1");
        assert_eq!(title.boxes.len(), 1);
        let LayoutBox {
            page, x, y, height, ..
        } = title.boxes[0];
        assert_eq!((page, x, y, height), (1, 15.0, 15.0, 37.5));
        // The page area is 195pt tall: the title, the header and nine rows fit on the first
        // page, the header and the other eleven rows on the second.
        let items = layout.element("items").expect("table");
        assert_eq!(items.pages(), [1, 2]);
        assert_eq!(items.rows.len(), 21);
        let header = &items.rows[0];
        assert_eq!((header.section, header.index), (RowSection::Header, 0));
        assert_eq!(header.pages(), [1, 2]);
        assert_eq!(header.boxes[1].y, 15.0);
        let body: Vec<Vec<usize>> = items.rows[1..].iter().map(LayoutRow::pages).collect();
        assert_eq!(body[8], [1]);
        assert_eq!(body[9], [2]);
        assert_eq!(items.rows[8].id.as_deref(), Some("eighth"));
        assert_eq!(items.rows[8].index, 7);
        assert_eq!(items.rows[10].boxes[0].y, 30.0);
        // A row with an `id` has the box of its row.
        assert_eq!(layout.element("eighth").expect("row").boxes, items.rows[8].boxes);
        let total = layout.element("total").expect("total");
        assert_eq!(total.pages(), [2]);
        assert_eq!(total.boxes[0].y, 15.0 + 15.0 * 12.0);
        let sum = layout.element("sum").expect("sum");
        // An inline box is as tall as its font, inside its line.
        let (line, inline) = (total.boxes[0], sum.boxes[0]);
        assert!(inline.x > line.x && inline.y >= line.y, "{inline:?} {line:?}");
        assert!(
            inline.y + inline.height <= line.y + line.height,
            "{inline:?} {line:?}"
        );
        assert_eq!(layout.element("hidden").expect("hidden").boxes, []);
    }

    #[test]
    fn grows_a_page_with_fit_content_from_the_height_of_size() {
        // The page area of `size` is 40mm less 8mm of margins, 120.94px: `100vh` is that cut to
        // whole pixels, and the page grows with the 150px below it, rounded up to whole points.
        let render = |body: &str| {
            let output = engine()
                .render(
                    &Document::html(format!(
                        "<!DOCTYPE html><style>@page {{ size: 80mm 40mm; margin: 4mm; -fagus-page-height: fit-content }}
                         body {{ margin: 0 }}</style>{body}"
                    ))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered");
            assert_eq!(output.pages, 1);
            let objects = String::from_utf8_lossy(&decoded(&output.pdf)).replace(' ', "");
            let start = objects.find("/MediaBox[").expect("media box") + "/MediaBox[".len();
            objects[start..].split(']').next().unwrap_or_default().to_owned()
        };
        assert_eq!(render("<p style='margin: 0'>Bon</p>"), "00227.04113.04");
        // 120px + 150px + 30.24px of margins are 225.18pt, made 226pt and that 226.08pt on the
        // 1/300 inch grid of Chrome's sheets.
        assert_eq!(
            render("<div style='height: 100vh'></div><div style='height: 150px'></div>"),
            "00227.04226.08"
        );
    }

    #[test]
    fn lays_shrunk_content_out_on_a_viewport_as_much_larger() {
        // A box 600px wide on a page area 400px wide shrinks the content to 2/3, laid out
        // again on a viewport of 600px: `50vw` is 300px there, a container as wide as the page
        // area 600px, as in Chrome.
        let html = "<style>@page { size: 400px 500px; margin: 0 } body { margin: 0 }
               .wide { width: 600px; height: 10px }
               .half { container-type: inline-size; width: 50vw }
               .full { container-type: inline-size }
               @container (min-width: 250px) { .half p { color: red } }
               @container (min-width: 500px) { .full p { color: blue } }</style>
             <div class=wide></div><div class=half><p>Half</p></div><div class=full><p>Full</p></div>";
        let pdf = engine()
            .render(&Document::html(html).created(CREATED).tagged(false))
            .expect("rendered")
            .pdf;
        let streams = streams(&pdf);
        assert!(
            streams.contains("1 0 0 rg"),
            "50vw of the wider viewport: {streams}"
        );
        assert!(
            streams.contains("0 0 1 rg"),
            "the container in the wider layout: {streams}"
        );
    }

    #[test]
    fn reports_invalid_declarations_of_style_attributes() {
        let output = engine()
            .render(
                &Document::html(
                    "<!doctype html>\n<p style='color: green'>a</p>\n<p style='colr: red'>b</p>\n<p style='colr: red'>c</p>",
                )
                .created(CREATED),
            )
            .expect("rendered");
        let found: Vec<String> = output.diagnostics.iter().map(ToString::to_string).collect();
        assert_eq!(
            found,
            [
                "html:3: warning[css.unknown-property]: `style` attribute of <p>: unknown property `colr`; the declaration is ignored"
            ],
            "once for the same attribute"
        );
    }

    #[test]
    fn draws_images_as_list_markers() {
        // Two pixels: opaque red, half transparent blue.
        const PIXELS: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR4nGP4z8AAQg0AD3oDfnfpf5cAAAAASUVORK5CYII=";
        let render = |css: &str| {
            let pdf = engine()
                .render(&Document::html(format!("<ul style='{css}'><li>Punkt</li></ul>")).created(CREATED))
                .expect("rendered");
            (
                String::from_utf8_lossy(&decoded(&pdf.pdf)).replace(' ', ""),
                pdf.diagnostics,
            )
        };
        let (image, _) = render(&format!("list-style-image: url({PIXELS})"));
        assert!(image.contains("/Subtype/Image"), "an image marker");
        // In a tagged PDF, the image is the item's label.
        assert!(image.contains("/S/Lbl"));
        let (fallback, diagnostics) = render("list-style-image: url(missing.png)");
        assert!(!fallback.contains("/Subtype/Image"), "the type's marker");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "resource.not-loaded")
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn draws_border_images_in_place_of_borders() {
        // Two pixels: opaque red, half transparent blue.
        const PIXELS: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR4nGP4z8AAQg0AD3oDfnfpf5cAAAAASUVORK5CYII=";
        let render = |html: &str| {
            let pdf = engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            (
                streams(&pdf),
                String::from_utf8_lossy(&decoded(&pdf)).replace(' ', ""),
            )
        };
        let (content, _) = render(
            "<div style='height: 20px; border: 6px solid lime; border-image: linear-gradient(red, blue) 1'></div>",
        );
        assert!(
            content.contains(" sh") || content.contains("/Pattern"),
            "a gradient: {content}"
        );
        assert!(!content.contains("0 1 0 rg"), "no lime border");
        // A raster image is cut into parts, each an image of its own.
        let (_, objects) = render(&format!(
            "<div style='height: 20px; border: 6px solid lime; border-image: url({PIXELS}) 1 round'></div>"
        ));
        assert!(objects.contains("/Width1/Height1"), "pixels of the corners");
        // Without the image, the border is drawn (stroked along its middle, as Chrome does).
        let (fallback, _) = render(
            "<div style='height: 20px; border: 6px solid lime; border-image: url(missing.png) 1'></div>",
        );
        assert!(fallback.contains("0 1 0 RG 6 w"), "{fallback}");
    }

    #[test]
    fn draws_pages_with_their_images_bleed_and_marks() {
        // A pixel of opaque red.
        const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let render = |html: &str| {
            engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf
        };
        // The images of `@page` rules are loaded, though which rules apply is known only once
        // the document is laid out.
        let pdf = render(&format!(
            "<style>@page {{ size: 300px 200px; background: url({PIXEL}) no-repeat }}</style><p>Text</p>"
        ));
        let objects = String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "");
        assert!(objects.contains("/Subtype/Image"), "the page's image: {objects}");
        // The sheet is as large as the page, its bleed and the room for the marks around it.
        let pdf = render("<style>@page { size: 300px 200px; bleed: 10px; marks: crop }</style><p>Text</p>");
        let objects = String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "");
        assert!(objects.contains("/TrimBox[43.543.5268.5193.5]"), "{objects}");
        assert!(objects.contains("/BleedBox[3636276201]"), "{objects}");
    }

    #[test]
    fn turns_pages_as_page_orientation_says() {
        let render = |orientation: &str| {
            let html = format!(
                "<style>@page {{ size: 288px 192px; margin: 0; page-orientation: {orientation} }}
                   body {{ margin: 0 }} a {{ display: block; width: 100px; height: 20px }}</style>
                 <a href='https://example.com/'></a>"
            );
            let pdf = engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "")
        };
        // Laid out upright, the page lies on its sheet turned, its links with it: the link at
        // its top left ends at the bottom left after a turn to the left.
        let left = render("rotate-left");
        assert!(left.contains("/MediaBox[00144216]"), "{left}");
        assert!(left.contains("/Rect[001575]"), "{left}");
        let right = render("rotate-right");
        assert!(right.contains("/MediaBox[00144216]"), "{right}");
        assert!(right.contains("/Rect[129141144216]"), "{right}");
        assert!(render("upright").contains("/MediaBox[00216144]"));
    }

    #[test]
    fn imports_stylesheets_and_resolves_their_urls_against_them() {
        // A pixel of opaque red.
        const PIXEL: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let pixel = resources::data_url(&format!("data:image/png;base64,{PIXEL}")).expect("a PNG");
        let output = engine()
            .render(
                &Document::html(
                    "<link rel=stylesheet href='css/main.css'><div class=logo></div><p class=late>x</p>",
                )
                .resource(
                    "css/main.css",
                    b"@import 'late.css'; .logo { height: 20px; background: url(img/pixel.png) }".to_vec(),
                )
                .resource("css/late.css", b".late { color: rgb(255, 0, 0) }".to_vec())
                .resource("css/img/pixel.png", pixel)
                .created(CREATED)
                .tagged(false),
            )
            .expect("rendered");
        assert!(codes(&output).is_empty(), "{:?}", output.diagnostics);
        let objects = String::from_utf8_lossy(&decoded(&output.pdf)).replace(' ', "");
        assert!(
            objects.contains("/Subtype/Image"),
            "the image beside the stylesheet"
        );
        assert!(streams(&output.pdf).contains("1 0 0 rg"), "the imported color");
    }

    #[test]
    fn scopes_linked_stylesheets_to_the_parent_of_their_link() {
        // An `@scope` without a prelude in a linked stylesheet applies below the link's parent
        // (WPT `scope-implicit-002-print`).
        let output = engine()
            .render(
                &Document::html(
                    "<p class=a>outside</p><div><link rel=stylesheet href=scope.css><p class=a>inside</p></div>",
                )
                .resource("scope.css", b"@scope { .a { color: rgb(255, 0, 0) } }".to_vec())
                .created(CREATED)
                .tagged(false),
            )
            .expect("rendered");
        let streams = streams(&output.pdf);
        assert_eq!(streams.matches("1 0 0 rg").count(), 1, "{streams}");
    }

    #[test]
    fn repeats_tiny_tiles_as_a_pattern() {
        // A pixel of opaque red, repeated more often than tiles are drawn one by one.
        const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let pdf = engine()
            .render(
                &Document::html(format!(
                    "<div style='height: 100px; background: url({PIXEL})'></div>"
                ))
                .created(CREATED)
                .tagged(false),
            )
            .expect("rendered")
            .pdf;
        let objects = String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "");
        assert!(objects.contains("/PatternType1"), "a tiling pattern: {objects}");
        assert_eq!(objects.matches("/Subtype/Image").count(), 1, "the image once");
    }

    #[test]
    fn draws_bmp_and_tiff_images() {
        // As an element, a background, a mask, a border image and in an SVG, which takes them
        // as PNG images of their pixels.
        let media = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus/images/media/");
        let read = |name: &str| std::fs::read(format!("{media}{name}")).expect("an image");
        for name in [
            "logo-alpha.bmp",
            "band-rle8.bmp",
            "scan-g4.tif",
            "receipt-jpeg.tif",
        ] {
            let output = engine()
                .render(
                    &Document::html(
                        "<img src=i style='width: 40px'>\
                         <div style='width: 20px; height: 20px; background: url(i); mask-image: url(i)'></div>\
                         <div style='width: 10px; border: 5px solid; border-image: url(i) 30%'></div>\
                         <svg width=20 height=20><image href=i width=20 height=20 /></svg>",
                    )
                    .resource("i", read(name))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered");
            assert_eq!(codes(&output), Vec::<&str>::new(), "{name}");
            let objects = String::from_utf8_lossy(&decoded(&output.pdf)).replace(' ', "");
            assert!(
                objects.matches("/Subtype/Image").count() >= 2,
                "{name}: {objects}"
            );
        }
        // A BMP whose color table is cut short is not one Fagus (or Chrome) reads; a TIFF of a
        // compression Fagus does not read (CCITT Group 3, 2) is broken.
        let mut short = read("band-rle8.bmp");
        short.truncate(60);
        let mut group3 = read("scan-g4.tif");
        let compression = group3
            .windows(4)
            .position(|entry| entry == [3, 1, 3, 0])
            .expect("the compression tag");
        group3[compression + 8] = 2;
        for (name, data, code) in [
            ("short.bmp", short, "image.unsupported"),
            ("group3.tif", group3, "image.broken"),
        ] {
            let output = engine()
                .render(
                    &Document::html("<img src=i style='width: 40px'>")
                        .resource("i", data)
                        .created(CREATED),
                )
                .expect("rendered without the image");
            assert_eq!(codes(&output), [code], "{name}");
        }
    }

    #[test]
    fn draws_avif_images() {
        // As an element, a background, a mask, a border image and in an SVG (as a PNG of its
        // pixels), and chosen by `<picture>` and `image-set()`.
        let media = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus/images/media/");
        let read = |name: &str| std::fs::read(format!("{media}{name}")).expect("an image");
        for name in [
            "photo.avif",
            "stamp-turned.avif",
            "avif/alpha.avif",
            "avif/grid.avif",
            "avif/icc.avif",
            "avif/pq.avif",
        ] {
            let output = engine()
                .render(
                    &Document::html(
                        "<img src=i style='width: 40px'>\
                         <picture><source type=image/avif srcset=i><img src=missing.png style='width: 30px'></picture>\
                         <div style='width: 20px; height: 20px; background: image-set(\"i\" type(\"image/avif\")); mask-image: url(i)'></div>\
                         <div style='width: 10px; border: 5px solid; border-image: url(i) 30%'></div>\
                         <svg width=20 height=20><image href=i width=20 height=20 /></svg>",
                    )
                    .resource("i", read(name))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered");
            assert_eq!(codes(&output), Vec::<&str>::new(), "{name}");
            let objects = String::from_utf8_lossy(&decoded(&output.pdf)).replace(' ', "");
            assert!(
                objects.matches("/Subtype/Image").count() >= 2,
                "{name}: {objects}"
            );
            // The ICC profile goes with the pixels.
            assert_eq!(objects.contains("/ICCBased"), name.contains("icc"), "{name}");
        }
        // A file of the brand without its items is not an image Fagus (or Chrome) reads; one
        // whose AV1 data is broken does not decode.
        let mut broken = read("avif/opaque.avif");
        let mdat = broken.windows(4).position(|kind| kind == b"mdat").expect("mdat") + 4;
        broken[mdat..mdat + 8].fill(0xFF);
        for (name, data, code) in [
            (
                "empty.avif",
                b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf".to_vec(),
                "image.unsupported",
            ),
            ("broken.avif", broken, "image.broken"),
        ] {
            let output = engine()
                .render(
                    &Document::html("<img src=i style='width: 40px'>")
                        .resource("i", data)
                        .created(CREATED),
                )
                .expect("rendered without the image");
            assert_eq!(codes(&output), [code], "{name}");
        }
    }

    #[test]
    fn leaves_out_images_that_do_not_decode() {
        // A PNG whose header reads well, its pixels not: their checksum is wrong.
        const BROKEN: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
        let output = engine()
            .render(
                &Document::html(format!("<p>Text</p><img src='{BROKEN}' style='width: 20px'>"))
                    .created(CREATED),
            )
            .expect("rendered without the image");
        assert_eq!(codes(&output), ["image.broken"]);
        assert!(!String::from_utf8_lossy(&decoded(&output.pdf)).contains("/Subtype /Image"));
    }

    #[test]
    fn masks_boxes_with_image_layers() {
        let render = |html: &str| {
            let pdf = engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf;
            String::from_utf8_lossy(&decoded(&pdf)).replace(' ', "")
        };
        let faded = render(
            "<div style='height: 40px; background: red; mask-image: linear-gradient(black, transparent)'></div>",
        );
        assert!(faded.contains("/Type/Mask/S/Alpha"), "{faded}");
        let luminance = render(
            "<div style='height: 40px; background: red; mask: linear-gradient(white, black) luminance'></div>",
        );
        assert!(luminance.contains("/Type/Mask/S/Luminosity") && !luminance.contains("/S/Alpha"));
        // A mask whose images are all missing masks nothing, as in Chrome.
        let missing =
            render("<div style='height: 40px; background: red; mask-image: url(missing.png)'></div>");
        assert!(!missing.contains("/Type/Mask"));
    }

    #[test]
    fn filters_boxes_and_images() {
        // Two pixels: opaque red, half transparent blue.
        const PIXELS: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR4nGP4z8AAQg0AD3oDfnfpf5cAAAAASUVORK5CYII=";
        let pdf = |html: &str| {
            engine()
                .render(&Document::html(html).created(CREATED).tagged(false))
                .expect("rendered")
                .pdf
        };
        let render = |html: &str| {
            let pdf = pdf(html);
            (
                streams(&pdf),
                String::from_utf8_lossy(&decoded(&pdf)).replace(' ', ""),
            )
        };
        // Colors are filtered, red to its luminance.
        let (gray, _) = render("<div style='filter: grayscale(1); background: red; height: 10px'></div>");
        assert!(gray.contains("0.21176471 0.21176471 0.21176471 rg"), "{gray}");
        // The pixels of an image too, and its alpha stays.
        let pixels = stream_bytes(&pdf(&format!("<img src='{PIXELS}' style='filter: invert(1)'>")));
        let has = |bytes: &[u8]| pixels.windows(bytes.len()).any(|window| window == bytes);
        assert!(has(b"\x00\xff\xff\xff\xff\x00"), "cyan and yellow");
        assert!(has(b"\xff\x80"), "the alpha");
        // A drop shadow: a group with the shadow's alpha, moved, below what casts it.
        let (shadow, objects) = render(
            "<div style='filter: drop-shadow(2px 3px 0 rgba(0, 0, 255, 0.5)); background: red; height: 10px'></div>",
        );
        assert!(
            shadow.contains("0 0 1 rg") && shadow.contains("1 0 0 rg"),
            "{shadow}"
        );
        assert!(objects.contains("/ca0.50196"), "a group with half the alpha");
        // The page draws the group, then the box.
        assert!(
            shadow.find(" Do") < shadow.find("1 0 0 rg"),
            "the shadow first: {shadow}"
        );
    }

    #[test]
    fn makes_up_faces_unless_font_synthesis_forbids_it() {
        let render = |css: &str, html: &str| {
            let document = Document::html(format!(
                "<style>
                   @font-face {{ font-family: Web; src: url(Inter-Latin.woff2); }}
                   p {{ font-family: Web; {css} }}
                 </style><p>{html}</p>"
            ))
            .base_directory(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/webfonts"))
            .created(CREATED)
            .tagged(false);
            streams(&engine().render(&document).expect("rendered").pdf)
        };
        // A bold face made up is filled and stroked, an italic one slanted (by a quarter, in
        // Chrome's points of a hair less than 0.75 a pixel).
        let fill_and_stroke = |content: &str| content.contains("2 Tr");
        let slanted = |content: &str| content.contains(" -0.18749999 -0.74999994 ");
        assert!(fill_and_stroke(&render("", "<b>Fett</b>")));
        assert!(!fill_and_stroke(&render("font-synthesis: style", "<b>Fett</b>")));
        assert!(!fill_and_stroke(&render(
            "font-synthesis-weight: none",
            "<b>Fett</b>"
        )));
        assert!(slanted(&render("", "<i>Kursiv</i>")));
        assert!(!slanted(&render("font-synthesis: none", "<i>Kursiv</i>")));
        assert!(slanted(&render("font-synthesis: weight style", "<i>Kursiv</i>")));
    }

    #[test]
    fn styles_inline_svg_with_css() {
        let render = |html: &str| {
            streams(
                &engine()
                    .render(&Document::html(html).created(CREATED).tagged(false))
                    .expect("rendered")
                    .pdf,
            )
        };
        // A rule for the path beats its attribute.
        let content = render(
            "<style>.blue { fill: rgb(0 0 255) }</style>\
             <svg width=10 height=10 viewBox='0 0 10 10'><path class=blue fill=red d='M0 0h10v10H0z'/></svg>",
        );
        assert!(content.contains("0 0 1 rg"), "{content}");
        assert!(!content.contains("1 0 0 rg"));
        // Colors of CSS Color 4, as Tailwind writes them, reach the SVG renderer in sRGB.
        let content = render(
            "<style>.brand { fill: oklch(45% 0.25 264) }</style>\
             <svg width=10 height=10 viewBox='0 0 10 10'><path class=brand d='M0 0h10v10H0z'/></svg>",
        );
        assert!(
            content.contains(" rg") && !content.contains("0 0 0 rg"),
            "{content}"
        );
        // Painting properties are inherited from the elements around the SVG.
        let content = render(
            "<div style='fill: #00ff00'><svg width=10 height=10 viewBox='0 0 10 10'>\
             <rect width=10 height=10 /></svg></div>",
        );
        assert!(content.contains("0 1 0 rg"), "{content}");
    }

    #[test]
    fn lays_out_foreign_objects_in_the_svg() {
        let render = |svg: &str| {
            streams(
                &engine()
                    .render(
                        &Document::html(format!("<body style='margin: 0'>{svg}</body>"))
                            .created(CREATED)
                            .tagged(false),
                    )
                    .expect("rendered")
                    .pdf,
            )
        };
        // The SVG is drawn twice its size: the box of 40 × 10 user units at (15, 25) is clipped
        // to 80 × 20 pixels at (30, 50) from the page area's corner (37 pixels into the page),
        // and its content scaled by two (1.5 points per pixel of the box, a hair less as Chrome
        // draws).
        let content = render(
            "<svg width=200 height=100 viewBox='0 0 100 50' style='display: block'>\
             <g transform='translate(10 20)'><foreignObject x=5 y=5 width=40 height=10>\
             <div style='background: rgb(0 0 255); height: 30px'></div></foreignObject></g></svg>",
        );
        assert!(content.contains("0 0 1 rg"), "{content}");
        assert!(
            content.contains("50.249996 776.67 m 110.24999 776.67 l 110.24999 761.67 l 50.249996 761.67 l"),
            "{content}"
        );
        assert!(content.contains("1.4999999 0 0 -1.4999999 "), "{content}");
        // A `<switch>` takes the `<foreignObject>` behind `requiredFeatures`, as draw.io writes
        // it, and leaves out the fallback.
        let content = render(
            "<svg width=100 height=50><switch><g requiredFeatures='http://www.w3.org/TR/SVG11/feature#Extensibility'>\
             <foreignObject width=100 height=50><div style='background: rgb(0 0 255); height: 30px'></div>\
             </foreignObject></g><rect width=100 height=50 fill='rgb(255 0 0)'/></switch></svg>",
        );
        assert!(content.contains("0 0 1 rg"), "{content}");
        assert!(!content.contains("1 0 0 rg"), "{content}");
    }

    #[test]
    fn draws_shapes_after_a_foreign_object_over_it() {
        let content = streams(
            &engine()
                .render(
                    &Document::html(
                        "<body style='margin: 0'><svg width=100 height=50>\
                         <rect width=100 height=50 fill='rgb(255 0 0)'/>\
                         <foreignObject width=100 height=50><div style='background: rgb(0 0 255); height: 30px'></div>\
                         </foreignObject><circle cx=20 cy=20 r=10 fill='rgb(0 255 0)'/></svg></body>",
                    )
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf,
        );
        // The rectangle, the HTML, then the circle over it, as Chrome draws them.
        let at = |color: &str| {
            content
                .find(color)
                .unwrap_or_else(|| panic!("{color}: {content}"))
        };
        assert!(at("1 0 0 rg") < at("0 0 1 rg"), "{content}");
        assert!(at("0 0 1 rg") < at("0 1 0 rg"), "{content}");
    }

    #[test]
    fn applies_inline_svg_style_sheets_to_the_document() {
        // Mermaid's way: the SVG's style sheet styles the HTML of its labels, and as a style
        // sheet of the document, the text of other SVGs and the HTML around them too.
        let content = streams(
            &engine()
                .render(
                    &Document::html(
                        "<body style='margin: 0'><svg width=100 height=50><style>.label { background: \
                         rgb(0 0 255); height: 30px } text { fill: rgb(0 255 0) }</style>\
                         <foreignObject width=100 height=50><div class=label></div></foreignObject></svg>\
                         <svg width=100 height=50><text y=20>Text</text></svg></body>",
                    )
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf,
        );
        assert!(content.contains("0 0 1 rg"), "{content}");
        assert!(content.contains("0 1 0 rg"), "{content}");
    }

    #[test]
    fn gives_svg_text_the_font_and_spacing_that_css_declares() {
        // A barcode's caption: CSS sets `font` and `letter-spacing` on the `<text>`, which win
        // over its `font-size` attribute, as in Chrome.
        let render = |css: &str| {
            streams(
                &engine()
                    .render(
                        &Document::html(format!(
                            "<style>body {{ margin: 0; font: 16px Inter }} {css}</style>\
                             <div class=code><svg width=200 height=40><text x=100 y=30 \
                             font-size=20 text-anchor=middle>LS-8</text></svg></div>"
                        ))
                        .created(CREATED)
                        .tagged(false),
                    )
                    .expect("rendered")
                    .pdf,
            )
        };
        let plain = render("");
        assert!(plain.contains("/f0 20 Tf") || plain.contains(" 20 Tf"), "{plain}");
        let styled = render(".code svg text { font: 9px Inter; letter-spacing: 1px }");
        assert!(styled.contains(" 9 Tf"), "{styled}");
        assert!(!styled.contains(" 20 Tf"), "{styled}");
    }

    #[test]
    fn sizes_svg_images_without_a_width_or_height_as_chrome() {
        let render = |svg: &str| {
            streams(
                &engine()
                    .render(
                        &Document::html(format!(
                            "<body style='margin: 0'><img src=\"data:image/svg+xml,{svg}\"></body>"
                        ))
                        .created(CREATED)
                        .tagged(false),
                    )
                    .expect("rendered")
                    .pdf,
            )
        };
        let rect = "<rect width='1000' height='500' fill='rgb(255 0 255)'/>";
        // A height and a `viewBox`: 50 × 25 pixels, its viewport that box, the content drawn
        // over all of it (0.05 pixels, 0.0375 points, a unit) from the box's corner.
        let content = render(&format!(
            "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1000 500' height='25'>{rect}</svg>"
        ));
        assert!(
            content.contains("27.749998 814.17 m 65.24999 814.17 l 65.24999 795.42 l"),
            "{content}"
        );
        assert!(
            content.contains("0.037499998 0 0 -0.037499998 27.749998 814.17 cm"),
            "{content}"
        );
        // With `preserveAspectRatio="none"` it has no natural size or ratio at all (Blink's
        // `SVGImage::GetIntrinsicSizingInfo`): 300 × 150 pixels, 225 × 112.5 points.
        let content = render(&format!(
            "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1000 500' height='25' \
             preserveAspectRatio='none'>{rect}</svg>"
        ));
        assert!(
            content.contains("27.749998 814.17 m 252.75 814.17 l 252.75 701.67 l"),
            "{content}"
        );
    }

    #[test]
    fn positions_fixed_backgrounds_in_the_window_chrome_loaded_the_page_in() {
        // `background-attachment: fixed` is positioned in the 780 × 380 pixels of the window,
        // from the top left of the page (Chrome 157 on Linux): centered, a 378 × 234.5 image
        // lies at (201, 72.75) px, (150.75, 69.5625) pt from the bottom of a page 300 pt high.
        let content = streams(
            &engine()
                .render(
                    &Document::html(format!(
                        "<style>@page {{ size: 600px 400px; margin: 30px 50px }} body {{ margin: 0 }}</style>\
                         <div style='height: 340px; background: url({PNG}) fixed center / 378px 234.5px no-repeat'></div>"
                    ))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf,
        );
        assert!(
            content.contains("283.49997 0 0 175.87498 150.74998 69.562515 cm"),
            "{content}"
        );
        // In a transformed box a fixed background scrolls with it (CSS Transforms 1, §6; WPT
        // `background-attachment-fixed-inside-transform-1`): centered in the box of 500 × 340
        // pixels at (50, 30), at (111, 82.75) px, moved down 10 px: (83.25, 54.5625) pt.
        let content = streams(
            &engine()
                .render(
                    &Document::html(format!(
                        "<style>@page {{ size: 600px 400px; margin: 30px 50px }} body {{ margin: 0 }}</style>\
                         <div style='transform: translateY(10px)'><div style='height: 340px; \
                         background: url({PNG}) fixed center / 378px 234.5px no-repeat'></div></div>"
                    ))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf,
        );
        assert!(
            content.contains("283.49997 0 0 175.87498 83.24999 54.56253 cm"),
            "{content}"
        );
    }

    #[test]
    fn positions_fixed_root_backgrounds_in_the_page_area_less_the_roots_margins() {
        // The root's fixed layers are sized and positioned in the page area, 500 × 340 pixels at
        // (50, 30), less the root's margins: `100% 100%` puts a 100 × 50 layer at (405, 310) px,
        // (303.75, 30) pt from the bottom of a page 300 pt high (Chrome 157).
        let content = streams(
            &engine()
                .render(
                    &Document::html(format!(
                        "<style>@page {{ size: 600px 400px; margin: 30px 50px }} html {{ margin: 10px 40px 0 5px; \
                         background: url({PNG}) fixed 100% 100% / 100px 50px no-repeat }}</style><p>Text</p>"
                    ))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf,
        );
        assert!(content.contains(" 303.74997 30.000"), "{content}");
    }

    #[test]
    fn applies_filters_of_svg_documents_of_their_own() {
        // `filter: url(filters.svg#id)` names a filter of an SVG document that the document
        // loads (WPT `filter-external-002`), as `url(data:…#id)` does one of a `data:` URL.
        let render = |css: &str| {
            engine()
                .render(
                    &Document::html(format!("<div style='width: 20px; height: 20px; background: red; {css}'></div>"))
                        .resource(
                            "filters.svg",
                            br#"<svg xmlns="http://www.w3.org/2000/svg"><filter id="f" color-interpolation-filters="sRGB"><feColorMatrix type="hueRotate" values="150"/></filter></svg>"#.to_vec(),
                        )
                        .created(CREATED)
                        .tagged(false),
                )
                .expect("rendered")
        };
        let output = render("filter: url(filters.svg#f)");
        assert!(codes(&output).is_empty(), "{:?}", output.diagnostics);
        // Red turned by the hue rotation: drawn otherwise than without the filter.
        assert_ne!(streams(&output.pdf), streams(&render("").pdf));
        let output = render("filter: url(filters.svg#missing)");
        assert_eq!(codes(&output), ["resource.not-loaded"]);
    }

    #[test]
    fn loads_images_with_fragments_from_resources_and_loaders() {
        // `sprite.svg#a` from the data handed over as `sprite.svg`, or from a loader that knows
        // the file only by that name; the file is asked for once.
        struct Sprite(Mutex<Vec<String>>);
        impl ResourceLoader for Sprite {
            fn load(&self, url: &str) -> Option<Vec<u8>> {
                self.0.lock().expect("not poisoned").push(url.to_owned());
                (url == "sprite.svg").then(|| SPRITE.as_bytes().to_vec())
            }
        }
        const SPRITE: &str = "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'>\
            <view id='a' viewBox='0 0 5 5'/><rect width='10' height='10' fill='red'/></svg>";
        let html = "<img src='sprite.svg#a' alt=''><img src='sprite.svg' alt=''>";
        let handed = engine()
            .render(&Document::html(html).resource("sprite.svg", SPRITE.as_bytes().to_vec()))
            .expect("rendered");
        assert_eq!(codes(&handed), Vec::<&str>::new());
        let sprite = Arc::new(Sprite(Mutex::default()));
        let loaded = engine()
            .render(&Document::html(html).loader(Arc::clone(&sprite) as Arc<dyn ResourceLoader>))
            .expect("rendered");
        assert_eq!(codes(&loaded), Vec::<&str>::new());
        assert_eq!(
            *sprite.0.lock().expect("not poisoned"),
            ["sprite.svg#a", "sprite.svg"]
        );
    }

    #[test]
    fn asks_a_loader_once_for_each_url_in_a_render() {
        // A document with container queries is styled and laid out twice, one wider than the
        // page again to shrink it, and each time its images are collected: the loader is asked
        // for each URL once all the same, also for one it does not have.
        struct Counting(Mutex<Vec<String>>);
        impl ResourceLoader for Counting {
            fn load(&self, url: &str) -> Option<Vec<u8>> {
                self.0.lock().expect("not poisoned").push(url.to_owned());
                (url != "missing.png").then(|| resources::data_url(PNG).expect("a PNG"))
            }
        }
        let asked = |html: &str| {
            let loader = Arc::new(Counting(Mutex::default()));
            let output = engine()
                .render(&Document::html(html).loader(Arc::clone(&loader) as Arc<dyn ResourceLoader>))
                .expect("rendered");
            assert_eq!(codes(&output), ["resource.not-loaded"]);
            loader.0.lock().expect("not poisoned").clone()
        };
        let images = "<img src='logo.png' alt=''><img src='missing.png' alt=''>\
                      <p style='background: url(paper.png)'>Text</p>";
        let expected = ["logo.png", "missing.png", "paper.png"];
        assert_eq!(asked(images), expected);
        let containers = format!(
            "<style>.card {{ container-type: inline-size }} \
             @container (min-width: 100px) {{ p {{ color: red }} }}</style>\
             <div class='card'>{images}</div>"
        );
        assert_eq!(asked(&containers), expected);
        let wide = format!("{containers}<div style='width: 1000px'>wider than the page</div>");
        assert_eq!(asked(&wide), expected);
        // An image that only the second layout names is asked for then.
        let late = format!(
            "<style>.card {{ container-type: inline-size }} \
             @container (min-width: 100px) {{ span {{ background: url(late.png) }} }}</style>\
             <div class='card'>{images}<span>late</span></div>"
        );
        assert_eq!(asked(&late), ["logo.png", "missing.png", "paper.png", "late.png"]);
    }

    /// A loader of the files it holds, which records what it is asked for: `one` for a URL asked
    /// for on its own, `many` for URLs asked for together. Without `together` it has no
    /// `load_many` of its own.
    struct Batches {
        files: Vec<(&'static str, Vec<u8>)>,
        together: bool,
        /// How many of the answers to a batch it gives.
        answers: usize,
        asked: Mutex<Vec<String>>,
    }

    impl Batches {
        fn new(together: bool) -> Self {
            let font = std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fonts/Inter-Regular.ttf"
            ))
            .expect("font");
            let css = "@font-face { font-family: Doc; src: url(font.ttf) }\
                       @font-face { font-family: Other; src: url(missing.woff2), url(other.ttf) }\
                       @font-face { font-family: Installed; src: local(\"Inter Regular\"), url(unused.ttf) }\
                       body { font-family: Doc, Other } p { background: url(paper.png) }";
            let sprite = "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'>\
                          <view id='a' viewBox='0 0 5 5'/><rect width='10' height='10' fill='red'/></svg>";
            let png = resources::data_url(PNG).expect("a PNG");
            Self {
                files: vec![
                    ("style.css", css.as_bytes().to_vec()),
                    ("font.ttf", font.clone()),
                    ("other.ttf", font),
                    ("logo.png", png.clone()),
                    ("paper.png", png.clone()),
                    ("small.png", png),
                    ("sprite.svg", sprite.as_bytes().to_vec()),
                ],
                together,
                answers: usize::MAX,
                asked: Mutex::default(),
            }
        }

        fn file(&self, url: &str) -> Option<Vec<u8>> {
            self.files
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, data)| data.clone())
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("not poisoned").clone()
        }
    }

    impl ResourceLoader for Batches {
        fn load(&self, url: &str) -> Option<Vec<u8>> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push(format!("one {url}"));
            self.file(url)
        }

        fn load_many(&self, urls: &[&str]) -> Vec<Option<Vec<u8>>> {
            if !self.together {
                return urls.iter().map(|url| self.load(url)).collect();
            }
            self.asked
                .lock()
                .expect("not poisoned")
                .push(format!("many {}", urls.join(" ")));
            urls.iter().take(self.answers).map(|url| self.file(url)).collect()
        }
    }

    /// A document with resources of every kind that is asked for in batches, and some that are
    /// not: a second source of a font, a source after an installed face, the candidates of a
    /// `srcset` that are not chosen.
    const BATCHED: &str = "<link rel=stylesheet href=style.css><link rel=stylesheet href=missing.css>\
        <img src=logo.png alt=''><img src=small.png srcset='large.png 2x, huge.png 3x' alt=''>\
        <img src='sprite.svg#a' alt=''><img src=sprite.svg alt=''><img src=logo.png alt=''>\
        <img src=missing.png alt=''><p>Text</p>";

    #[test]
    fn asks_a_loader_for_the_resources_of_each_phase_together() {
        let render = |document: Document| {
            engine()
                .render(&document.created(CREATED).tagged(false))
                .expect("rendered")
        };
        let batched = Arc::new(Batches::new(true));
        let together =
            render(Document::html(BATCHED).loader(Arc::clone(&batched) as Arc<dyn ResourceLoader>));
        // The style sheets, then the first file of each `@font-face` rule, then the images, each
        // in the order of the document; what is not found with a fragment once more without it;
        // the second source of a font only once the first is known to be missing.
        assert_eq!(
            batched.asked(),
            [
                "many style.css missing.css",
                "many font.ttf missing.woff2",
                "one other.ttf",
                "many logo.png small.png sprite.svg#a missing.png paper.png",
                "many sprite.svg",
            ]
        );
        // A loader without `load_many` is asked for the same URLs, one by one.
        let single = Arc::new(Batches::new(false));
        let one_by_one =
            render(Document::html(BATCHED).loader(Arc::clone(&single) as Arc<dyn ResourceLoader>));
        let urls = |loader: &Batches| {
            let mut urls: Vec<String> = loader
                .asked()
                .iter()
                .flat_map(|asked| asked.split(' ').skip(1).map(str::to_owned).collect::<Vec<_>>())
                .collect();
            urls.sort();
            urls
        };
        assert!(single.asked().iter().all(|asked| asked.starts_with("one ")));
        assert_eq!(urls(&single), urls(&batched));
        // The PDF is the same, and the same as with the resources handed over.
        assert_eq!(codes(&together), codes(&one_by_one));
        assert_eq!(together.pdf, one_by_one.pdf);
        let mut handed = Document::html(BATCHED);
        for (url, data) in &Batches::new(false).files {
            handed = handed.resource(*url, data.clone());
        }
        let handed = render(handed);
        assert_eq!(codes(&together), codes(&handed));
        assert_eq!(together.pdf, handed.pdf);
    }

    #[test]
    fn asks_for_what_a_batch_did_not_answer_no_more() {
        // An answer shorter than the batch: the rest is not found, and not asked for again.
        let loader = Arc::new(Batches {
            answers: 1,
            ..Batches::new(true)
        });
        let output = engine()
            .render(&Document::html(BATCHED).loader(Arc::clone(&loader) as Arc<dyn ResourceLoader>))
            .expect("rendered");
        assert_eq!(
            loader.asked(),
            [
                "many style.css missing.css",
                "many font.ttf missing.woff2",
                "one other.ttf",
                "many logo.png small.png sprite.svg#a missing.png paper.png",
                "many sprite.svg",
            ]
        );
        assert_eq!(
            codes(&output)
                .iter()
                .filter(|code| **code == "resource.not-loaded")
                .count(),
            // `missing.css`, `missing.woff2`, and all the images but `logo.png` and the sprite.
            5
        );
    }

    #[test]
    fn asks_for_at_most_a_batch_of_urls_at_once() {
        struct Sizes(Mutex<Vec<usize>>);
        impl ResourceLoader for Sizes {
            fn load(&self, _: &str) -> Option<Vec<u8>> {
                None
            }

            fn load_many(&self, urls: &[&str]) -> Vec<Option<Vec<u8>>> {
                self.0.lock().expect("not poisoned").push(urls.len());
                vec![None; urls.len()]
            }
        }
        let html = (0..150).fold(String::new(), |mut html, index| {
            use std::fmt::Write as _;
            let _ = write!(html, "<img src='{index}.png' alt=''>");
            html
        });
        let loader = Arc::new(Sizes(Mutex::default()));
        engine()
            .render(&Document::html(html).loader(Arc::clone(&loader) as Arc<dyn ResourceLoader>))
            .expect("rendered");
        assert_eq!(*loader.0.lock().expect("not poisoned"), [BATCH, BATCH, 22]);
    }

    #[test]
    fn draws_the_images_that_inline_svgs_refer_to() {
        // `<image href>` in an inline SVG loads its image as the document does (WPT
        // `baseline-shift-length-percentage-svg`).
        struct Logo(bool);
        impl ResourceLoader for Logo {
            fn load(&self, url: &str) -> Option<Vec<u8>> {
                (self.0 && url == "logo.png").then(|| resources::data_url(PNG).expect("a PNG"))
            }
        }
        let drawn = |found: bool| {
            let pdf = engine()
                .render(
                    &Document::html(
                        "<svg width=40 height=40><image href=logo.png width=40 height=40 /></svg>",
                    )
                    .loader(Arc::new(Logo(found)))
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf;
            streams(&pdf).matches(" Do").count()
        };
        assert!(drawn(true) > drawn(false));
    }

    #[test]
    fn lays_out_inline_svgs_without_a_view_box_in_their_box() {
        // An icon sized by CSS alone (`<svg class=icon><use href=#icon>`): its viewport is its
        // box, 40 × 20 pixels, which the percentages of its content (a `<symbol>` drawn by
        // `<use>` is 100% large) are of, not the 300 × 150 of an SVG without a size (Chrome 157).
        let content = streams(
            &engine()
                .render(
                    &Document::html(
                        "<body style='margin: 0'><svg style='width: 40px; height: 20px; display: block'>\
                         <rect width='50%' height='50%' fill='rgb(255 0 0)'/></svg></body>",
                    )
                    .created(CREATED)
                    .tagged(false),
                )
                .expect("rendered")
                .pdf,
        );
        assert!(
            content.contains("1 0 0 rg 0 0 m 20 0 l 20 10 l 0 10 l"),
            "{content}"
        );
    }

    #[test]
    fn draws_the_html_of_svg_images() {
        // draw.io's way: a `<switch>` that takes the `<foreignObject>`, with a fallback text.
        let svg = "<?xml version='1.0'?><!DOCTYPE svg PUBLIC '-//W3C//DTD SVG 1.1//EN' 'svg11.dtd'>\
                   <svg xmlns='http://www.w3.org/2000/svg' width='100' height='50' viewBox='0 0 100 50'>\
                   <style>.own { background: rgb(0 0 255); height: 30px }</style>\
                   <rect width='100' height='50' fill='rgb(0 255 0)'/><switch>\
                   <foreignObject width='100' height='50' requiredFeatures='http://www.w3.org/TR/SVG11/feature%23Extensibility'>\
                   <div xmlns='http://www.w3.org/1999/xhtml' class='own'></div></foreignObject>\
                   <text y='20' fill='rgb(255 255 0)'>Fallback</text></switch></svg>";
        let render = |body: &str| {
            streams(
                &engine()
                    .render(
                        &Document::html(format!(
                            "<style>div {{ background-color: rgb(255 0 0) !important }}</style>\
                             <body style='margin: 0'>{body}</body>"
                        ))
                        .created(CREATED)
                        .tagged(false),
                    )
                    .expect("rendered")
                    .pdf,
            )
        };
        let content = render(&format!("<img src=\"data:image/svg+xml,{svg}\" width=200>"));
        let at = |color: &str| {
            content
                .find(color)
                .unwrap_or_else(|| panic!("{color}: {content}"))
        };
        // The HTML in the SVG's own style, not the document's, over the rectangle before it,
        // without the fallback; the box 100 × 30 user units scaled by two (1.5 points a pixel, a
        // hair less as Chrome draws).
        assert!(at("0 1 0 rg") < at("0 0 1 rg"), "{content}");
        assert!(!content.contains("1 0 0 rg"), "{content}");
        assert!(!content.contains("1 1 0 rg"), "{content}");
        assert!(content.contains("1.4999999 0 0 -1.4999999 "), "{content}");
        // As a background, the same.
        let content = render(&format!(
            "<p style=\"width: 100px; height: 50px; background: url(&quot;data:image/svg+xml,{svg}&quot;)\"></p>"
        ));
        assert!(content.contains("0 0 1 rg"), "{content}");
        assert!(!content.contains("1 0 0 rg"), "{content}");
    }

    #[test]
    fn loads_the_font_faces_of_svg_style_sheets() {
        let italic = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fonts/Inter-Italic.ttf"
        ))
        .expect("font");
        // `own.ttf`, and an SVG image whose rule loads the face from `source`.
        let render = |html: &str, source: &str| {
            let image = format!(
                "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='20'><style>\
                 @font-face {{ font-family: Own; src: url({source}) }}</style>\
                 <text y='15' font-family='Own'>Fagus</text></svg>"
            );
            engine()
                .render(
                    &Document::html(html)
                        .resource("own.ttf", italic.clone())
                        .resource("image.svg", image.into_bytes())
                        .created(CREATED)
                        .tagged(false),
                )
                .expect("rendered")
                .pdf
        };
        // An inline SVG's rules are the document's: a relative URL loads.
        let inline = "<svg width=100 height=20><style>@font-face { font-family: Own; src: url(own.ttf) }</style>\
                      <text y=15 font-family=Own>Fagus</text></svg>";
        assert!(contains(&render(inline, ""), "Inter-Italic"));
        // An SVG image loads only `data:` URLs, as in Chrome.
        let encoded = italic.iter().fold(String::new(), |mut encoded, byte| {
            let _ = std::fmt::Write::write_fmt(&mut encoded, format_args!("%{byte:02X}"));
            encoded
        });
        assert!(contains(
            &render("<img src=image.svg>", &format!("data:font/ttf,{encoded}")),
            "Inter-Italic"
        ));
        assert!(!contains(
            &render("<img src=image.svg>", "own.ttf"),
            "Inter-Italic"
        ));
    }

    #[test]
    fn renders_without_fonts_of_its_own() {
        // The built-in Inter sets the text, in bold too.
        let output = Engine::new()
            .render(&Document::html("<p>Rechnung <b>RE-1</b></p>").created(CREATED))
            .expect("rendered");
        assert_eq!(output.pages, 1);
        assert!(contains(&output.pdf, "Inter-Regular"));
        assert!(contains(&output.pdf, "Inter-Bold"));
    }

    #[test]
    fn strokes_the_native_frames_of_controls_as_chrome_prints_them() {
        let render = |html: &str| {
            let document = Document::html(format!("<style>body {{ font: 13px Inter }}</style>{html}"))
                .created(CREATED)
                .tagged(false);
            streams(&engine().render(&document).expect("rendered").pdf)
        };
        // A field's frame is a hairline, a button's a stroke of 1px (the default width), a
        // zoomed field's a stroke of whole pixels of the zoom.
        assert!(render("<input value=Text>").contains("0 w"));
        let button = render("<button>Senden</button>");
        assert!(!button.contains("0 w") && button.contains("\nS\n"), "{button}");
        assert!(render("<input value=Text style='zoom: 2'>").contains("2 w"));
    }

    #[test]
    fn draws_inline_svg_as_figures() {
        let page = |svg: &str| {
            Document::html(format!(
                "<!doctype html><html lang=de><title>Rechnung</title><p style=\"color: #2563eb\">Bezahlt {svg}</p>"
            ))
            .created(CREATED)
            .accessible()
        };
        let icon = |inner: &str, attributes: &str| {
            format!(
                "<svg width=16 height=16 viewBox=\"0 0 24 24\"{attributes}>{inner}\
                 <path d=\"M4 12l5 5L20 6\" fill=\"none\" stroke=\"currentColor\"/></svg>"
            )
        };
        // Named by its title or label, or hidden from readers as decoration.
        let output = engine()
            .render(&page(&icon("<title>bezahlt</title>", "")))
            .expect("titled");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        engine()
            .render(&page(&icon("", " aria-label=\"bezahlt\"")))
            .expect("labelled");
        engine()
            .render(&page(&icon("", " aria-hidden=\"true\"")))
            .expect("decoration");
        match engine().render(&page(&icon("", ""))) {
            Err(Error::Pdf(PdfError::Conformance { problems, .. })) => {
                assert!(problems.contains("has no alternative text"));
            }
            other => panic!("not refused: {:?}", other.map(|output| output.pages)),
        }
    }
}
