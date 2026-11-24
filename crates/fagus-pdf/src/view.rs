//! How a viewer shows the PDF when it opens it: the panel beside the pages, how the pages are
//! laid out, the zoom, the page to open at, and what the window does (ISO 32000-1, §7.7.2 and
//! §12.2). These are entries of the document catalog, which krilla does not write; they are
//! added as the PDF is rewritten (see [`crate::compact`]).

use std::fmt::Write as _;

use crate::{Conformance, parse, update};

/// What a viewer shows beside the pages when it opens the PDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageMode {
    /// Nothing but the pages.
    None,
    /// The document outline (the bookmarks).
    Outlines,
    /// The thumbnails of the pages.
    Thumbnails,
    /// The pages in full-screen mode, without menus or window controls.
    FullScreen,
    /// The attachments; it needs PDF 1.6, so PDF/A-1 has none.
    Attachments,
    /// The layers; it needs PDF 1.5, so PDF/A-1 has none.
    Layers,
}

impl PageMode {
    /// Every page mode.
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Outlines,
        Self::Thumbnails,
        Self::FullScreen,
        Self::Attachments,
        Self::Layers,
    ];

    /// The page mode named like `outlines` or `full-screen`, as the command line and the worker
    /// protocol name them.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.name() == name)
    }

    /// The name of the page mode, like `full-screen`.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Outlines => "outlines",
            Self::Thumbnails => "thumbnails",
            Self::FullScreen => "full-screen",
            Self::Attachments => "attachments",
            Self::Layers => "layers",
        }
    }

    /// The value of `/PageMode`.
    fn pdf(self) -> &'static str {
        match self {
            Self::None => "/UseNone",
            Self::Outlines => "/UseOutlines",
            Self::Thumbnails => "/UseThumbs",
            Self::FullScreen => "/FullScreen",
            Self::Attachments => "/UseAttachments",
            Self::Layers => "/UseOC",
        }
    }

    /// Whether PDF 1.4 has the value.
    fn in_pdf_1_4(self) -> bool {
        !matches!(self, Self::Attachments | Self::Layers)
    }
}

/// How a viewer lays the pages out when it opens the PDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageLayout {
    /// One page at a time.
    SinglePage,
    /// The pages in one column, scrolling.
    OneColumn,
    /// The pages in two columns, scrolling, the odd pages on the left.
    TwoColumnLeft,
    /// The pages in two columns, scrolling, the odd pages on the right (the first page stands
    /// alone, as the cover of a book).
    TwoColumnRight,
    /// Two pages at a time, the odd pages on the left; it needs PDF 1.5, so PDF/A-1 has none.
    TwoPageLeft,
    /// Two pages at a time, the odd pages on the right; it needs PDF 1.5, so PDF/A-1 has none.
    TwoPageRight,
}

impl PageLayout {
    /// Every page layout.
    pub const ALL: [Self; 6] = [
        Self::SinglePage,
        Self::OneColumn,
        Self::TwoColumnLeft,
        Self::TwoColumnRight,
        Self::TwoPageLeft,
        Self::TwoPageRight,
    ];

    /// The page layout named like `one-column`, as the command line and the worker protocol
    /// name them.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|layout| layout.name() == name)
    }

    /// The name of the page layout, like `two-column-left`.
    pub fn name(self) -> &'static str {
        match self {
            Self::SinglePage => "single-page",
            Self::OneColumn => "one-column",
            Self::TwoColumnLeft => "two-column-left",
            Self::TwoColumnRight => "two-column-right",
            Self::TwoPageLeft => "two-page-left",
            Self::TwoPageRight => "two-page-right",
        }
    }

    /// The value of `/PageLayout`.
    fn pdf(self) -> &'static str {
        match self {
            Self::SinglePage => "/SinglePage",
            Self::OneColumn => "/OneColumn",
            Self::TwoColumnLeft => "/TwoColumnLeft",
            Self::TwoColumnRight => "/TwoColumnRight",
            Self::TwoPageLeft => "/TwoPageLeft",
            Self::TwoPageRight => "/TwoPageRight",
        }
    }

    /// Whether PDF 1.4 has the value.
    fn in_pdf_1_4(self) -> bool {
        !matches!(self, Self::TwoPageLeft | Self::TwoPageRight)
    }
}

/// How large a viewer shows the pages when it opens the PDF.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Zoom {
    /// The whole page fits the window.
    FitPage,
    /// The width of the page fits the window.
    FitWidth,
    /// A magnification in percent: 100 for the actual size.
    Percent(f32),
}

/// How a viewer shows the PDF when it opens it. Viewers follow it to different degrees: it is a
/// wish, not a rule.
///
/// ```
/// use fagus_pdf::{InitialView, PageLayout, PageMode, Zoom};
///
/// let view = InitialView::new()
///     .page_mode(PageMode::Outlines)
///     .page_layout(PageLayout::OneColumn)
///     .zoom(Zoom::FitWidth);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InitialView {
    page_mode: Option<PageMode>,
    page_layout: Option<PageLayout>,
    zoom: Option<Zoom>,
    /// From 1.
    page: Option<usize>,
    fit_window: bool,
    center_window: bool,
}

impl InitialView {
    /// A view that leaves everything to the viewer.
    pub fn new() -> Self {
        Self::default()
    }

    /// What the viewer shows beside the pages.
    #[must_use]
    pub fn page_mode(mut self, mode: PageMode) -> Self {
        self.page_mode = Some(mode);
        self
    }

    /// How the viewer lays the pages out.
    #[must_use]
    pub fn page_layout(mut self, layout: PageLayout) -> Self {
        self.page_layout = Some(layout);
        self
    }

    /// How large the viewer shows the pages.
    #[must_use]
    pub fn zoom(mut self, zoom: Zoom) -> Self {
        self.zoom = Some(zoom);
        self
    }

    /// The page the viewer opens at, counted from 1. A page the document does not have is left
    /// out, with a diagnostic.
    #[must_use]
    pub fn page(mut self, page: usize) -> Self {
        self.page = Some(page);
        self
    }

    /// Whether the viewer sizes its window to the first page shown.
    #[must_use]
    pub fn fit_window(mut self, fit: bool) -> Self {
        self.fit_window = fit;
        self
    }

    /// Whether the viewer centers its window on the screen.
    #[must_use]
    pub fn center_window(mut self, center: bool) -> Self {
        self.center_window = center;
        self
    }

    /// The view without what a document of `pages` pages that meets `conformance` cannot have,
    /// and what is wrong with each part left out.
    pub fn checked(&self, conformance: Option<Conformance>, pages: usize) -> (Self, Vec<String>) {
        let mut view = self.clone();
        let mut problems = Vec::new();
        // PDF/A-1 is PDF 1.4.
        if let Some(conformance) = conformance.filter(|conformance| conformance.part() == 1) {
            let standard = conformance.name();
            if let Some(mode) = view.page_mode.filter(|mode| !mode.in_pdf_1_4()) {
                view.page_mode = None;
                problems.push(format!(
                    "{standard} is PDF 1.4, which does not have the page mode `{}`; it is left out",
                    mode.name()
                ));
            }
            if let Some(layout) = view.page_layout.filter(|layout| !layout.in_pdf_1_4()) {
                view.page_layout = None;
                problems.push(format!(
                    "{standard} is PDF 1.4, which does not have the page layout `{}`; it is left out",
                    layout.name()
                ));
            }
        }
        if let Some(Zoom::Percent(percent)) = view.zoom
            && !(percent.is_finite() && percent > 0.0)
        {
            view.zoom = None;
            problems.push(format!(
                "a zoom of {percent} percent is not possible; it is left out"
            ));
        }
        if let Some(page) = view.page.filter(|&page| page == 0 || page > pages) {
            view.page = None;
            problems.push(format!(
                "the page to open at ({page}) is not a page of the document, which has {pages}; it is left out"
            ));
        }
        (view, problems)
    }

    /// The entries of `/ViewerPreferences`, after `/DisplayDocTitle` if the document has a
    /// title.
    pub(crate) fn preferences(view: Option<&Self>, title: bool) -> String {
        let mut entries = String::new();
        if title {
            entries.push_str("/DisplayDocTitle true");
        }
        if view.is_some_and(|view| view.fit_window) {
            entries.push_str("/FitWindow true");
        }
        if view.is_some_and(|view| view.center_window) {
            entries.push_str("/CenterWindow true");
        }
        entries
    }

    /// The entries of the document catalog besides `/ViewerPreferences`. `pdf` is the document
    /// as krilla wrote it, in which the page to open at is looked up.
    pub(crate) fn entries(&self, pdf: &[u8]) -> Vec<(&'static str, String)> {
        let mut entries = Vec::new();
        if let Some(mode) = self.page_mode {
            entries.push(("/PageMode", mode.pdf().to_owned()));
        }
        if let Some(layout) = self.page_layout {
            entries.push(("/PageLayout", layout.pdf().to_owned()));
        }
        if let Some(destination) = self.destination(pdf) {
            entries.push(("/OpenAction", destination));
        }
        entries
    }

    /// The destination a viewer opens at: the top of the page, with the zoom. It is a
    /// destination and no action, which has nothing a standard forbids and no strings to
    /// encrypt. `None` without a page and a zoom, or if the page is not found.
    fn destination(&self, pdf: &[u8]) -> Option<String> {
        if self.zoom.is_none() && self.page.is_none() {
            return None;
        }
        let update = update::Update::new(pdf)?;
        let catalog = update.dictionary(update.root()?)?;
        let tree = update.dictionary(update::reference(parse::entry(&catalog, "/Pages")?)?)?;
        let kids = update::references(parse::entry(&tree, "/Kids")?);
        let page = *kids.get(self.page.unwrap_or(1).checked_sub(1)?)?;
        let dictionary = update.dictionary(page)?;
        // The top of the page: the last number of its media box.
        let media = parse::entry(&dictionary, "/MediaBox")?;
        let top = media
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split_whitespace()
            .nth(3)
            .filter(|top| top.parse::<f32>().is_ok())?;
        let mut destination = format!("[{page} 0 R");
        match self.zoom {
            Some(Zoom::FitPage) => destination.push_str("/Fit"),
            Some(Zoom::FitWidth) => write!(destination, "/FitH {top}").ok()?,
            Some(Zoom::Percent(percent)) => {
                write!(destination, "/XYZ null {top} {}", update::number(percent / 100.0)).ok()?;
            }
            // The zoom stays as the viewer has it.
            None => write!(destination, "/XYZ null {top} null").ok()?,
        }
        destination.push(']');
        Some(destination)
    }
}
