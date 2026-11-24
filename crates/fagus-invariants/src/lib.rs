//! What holds for the layout of every document, whatever its markup: checked on the pages of a
//! render ([`fagus::Inspection`]) rather than against reference images.
//!
//! [`check`] renders a document and looks at what the engine laid out:
//!
//! | Invariant | What holds |
//! |---|---|
//! | [`Invariant::Internal`] | Rendering does not end in an internal error |
//! | [`Invariant::Finite`] | Every number of the pages, glyphs and reported boxes is finite |
//! | [`Invariant::Sizes`] | No fragment, page or reported box has a negative size; a page is shrunk by a scale in (0, 1]; the clusters of glyphs lie in the text of their run |
//! | [`Invariant::Placeholder`] | No placeholder of a positioned box is left in the pages |
//! | [`Invariant::PageCount`] | A document has 1 to [`fagus_layout::MAX_PAGES`] pages, as many as the output and the reported layout say, with a warning at the limit |
//! | [`Invariant::Deterministic`] | Rendering again gives the same bytes, pages and reported layout, or fails the same way |
//! | [`Invariant::ReportedLayout`] | [`fagus::Layout`] has the elements with an `id` in document order and a box for each box of theirs in the pages, in page order |
//!
//! These hold for arbitrary input, so the fuzz target `invariants` checks them on arbitrary
//! markup. A document that runs out of its memory budget or time limit is not checked: where
//! that happens depends on the machine.

#![forbid(unsafe_code)]

use std::fmt;

use fagus::{Date, Document, Engine, Error, Inspection, Layout, LayoutBox, Output};
use fagus_dom::NodeId;
use fagus_layout::{Fragment, FragmentKind, MAX_PAGES, Page, units::Rect};

/// The creation date of the documents checked, so that rendering twice writes the same bytes.
const CREATED: u64 = 1_793_620_800;

/// A fragment this far from the corner of its page, or this large, in pixels, counts as
/// saturated ([`Stats::saturated`]): lengths end there when they overflow, or come from styles
/// that ask for them (`margin: -1e30px`).
const SATURATED: f32 = 1.0e6;

/// The most violations [`examine`] collects of one document.
const MOST: usize = 32;

/// How long the outline of a page with a violation may be, in bytes.
const OUTLINE: usize = 4096;

/// What a violation breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Invariant {
    /// Rendering ended in an internal error.
    Internal,
    /// A number is not finite.
    Finite,
    /// A size is negative, a scale out of range or a cluster outside its text.
    Sizes,
    /// A placeholder is left in the pages.
    Placeholder,
    /// The number of pages is out of range or differs between what reports it.
    PageCount,
    /// Rendering again gave something else.
    Deterministic,
    /// The reported layout differs from the pages.
    ReportedLayout,
}

impl fmt::Display for Invariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Internal => "internal error",
            Self::Finite => "finite numbers",
            Self::Sizes => "sizes",
            Self::Placeholder => "placeholders",
            Self::PageCount => "page count",
            Self::Deterministic => "determinism",
            Self::ReportedLayout => "reported layout",
        })
    }
}

/// An invariant that does not hold, and where.
#[derive(Clone, Debug)]
pub struct Violation {
    pub invariant: Invariant,
    pub message: String,
    /// The number of the page, from 1.
    pub page: Option<usize>,
    /// The fragment: its place among the fragments of the page, then among the children of each
    /// fragment down to it.
    pub path: Vec<usize>,
    /// The fragment's rectangle, relative to its parent.
    pub rect: Option<Rect>,
    /// An outline of the page ([`fagus_layout::dump`]), cut where it is long.
    pub outline: Option<String>,
}

impl Violation {
    fn new(invariant: Invariant, message: impl Into<String>) -> Self {
        Self {
            invariant,
            message: message.into(),
            page: None,
            path: Vec::new(),
            rect: None,
            outline: None,
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.invariant, self.message)?;
        if let Some(page) = self.page {
            write!(formatter, " (page {page}")?;
            if !self.path.is_empty() {
                let path: Vec<String> = self.path.iter().map(ToString::to_string).collect();
                write!(formatter, ", fragment {}", path.join("/"))?;
            }
            if let Some(rect) = self.rect {
                write!(
                    formatter,
                    ", x={} y={} w={} h={}",
                    rect.x.to_px(),
                    rect.y.to_px(),
                    rect.width.to_px(),
                    rect.height.to_px()
                )?;
            }
            formatter.write_str(")")?;
        }
        if let Some(outline) = &self.outline {
            write!(formatter, "\n{outline}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Violation {}

/// What was checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Whether the document rendered; one that the engine refuses for a reason (a PDF error) has
    /// nothing to check but that it is refused again.
    pub rendered: bool,
    /// Whether a render ran out of its memory budget or time limit, so that nothing, or only
    /// the first render, was checked.
    pub over_budget: bool,
    pub pages: usize,
    pub fragments: usize,
    pub glyphs: usize,
    /// The boxes of the reported layout.
    pub boxes: usize,
    /// The fragments farther than a million pixels from the corner of their page, or larger:
    /// counted, not a violation, as styles may ask for such lengths.
    pub saturated: usize,
}

/// Renders `document` with `engine` and checks the invariants; the first that does not hold is
/// the error. The document is rendered with a fixed creation date; one that is encrypted without
/// a seed writes other bytes each time and fails.
pub fn check(engine: &Engine, document: &Document) -> Result<Stats, Violation> {
    first(examine(engine, None, document))
}

/// As [`check`], and renders the document once more with `fresh`, an engine set up as `engine`
/// that has rendered nothing yet: what `engine` remembers of earlier documents must not change
/// the output.
pub fn check_fresh(engine: &Engine, fresh: &Engine, document: &Document) -> Result<Stats, Violation> {
    first(examine(engine, Some(fresh), document))
}

fn first((stats, violations): (Stats, Vec<Violation>)) -> Result<Stats, Violation> {
    match violations.into_iter().next() {
        Some(violation) => Err(violation),
        None => Ok(stats),
    }
}

/// As [`check_fresh`], with every violation found (up to a few dozen) rather than the first: for
/// measuring what an invariant meets before it is enforced.
pub fn examine(engine: &Engine, fresh: Option<&Engine>, document: &Document) -> (Stats, Vec<Violation>) {
    let document = document
        .clone()
        .created(Date::from_unix(CREATED))
        .report_layout(true);
    let mut stats = Stats::default();
    let mut found = Vec::new();
    let first = engine.render(&document.clone().inspect(true));
    match &first {
        Ok(output) => {
            stats = examine_output(output, &mut found);
            stats.rendered = true;
        }
        Err(Error::Internal(message)) => {
            found.push(Violation::new(Invariant::Internal, message.clone()));
            return (stats, found);
        }
        Err(error) if over_budget(error) => {
            stats.over_budget = true;
            return (stats, found);
        }
        Err(_) => {}
    }
    let again = [(Some(engine), "again"), (fresh, "by a fresh engine")];
    for (engine, how) in again {
        let Some(engine) = engine else { continue };
        let second = engine.render(&document);
        if let Err(error) = &second
            && over_budget(error)
        {
            stats.over_budget = true;
            continue;
        }
        if let Some(difference) = difference(&first, &second) {
            found.push(Violation::new(
                Invariant::Deterministic,
                format!("rendered {how}: {difference}"),
            ));
        }
    }
    (stats, found)
}

fn over_budget(error: &Error) -> bool {
    matches!(error, Error::MemoryBudget { .. } | Error::TimeLimit { .. })
}

/// How two renders of a document differ, if they do.
fn difference(first: &Result<Output, Error>, second: &Result<Output, Error>) -> Option<String> {
    match (first, second) {
        (Ok(first), Ok(second)) => {
            if first.pages != second.pages {
                Some(format!("{} pages, then {}", first.pages, second.pages))
            } else if first.layout != second.layout {
                Some("the reported layout differs".to_owned())
            } else if first.pdf != second.pdf {
                let at = first
                    .pdf
                    .iter()
                    .zip(&second.pdf)
                    .position(|(first, second)| first != second)
                    .unwrap_or_else(|| first.pdf.len().min(second.pdf.len()));
                Some(format!(
                    "the PDF differs from byte {at} ({} bytes, then {})",
                    first.pdf.len(),
                    second.pdf.len()
                ))
            } else {
                None
            }
        }
        (Err(first), Err(second)) => {
            let (first, second) = (first.to_string(), second.to_string());
            (first != second).then(|| format!("failed with `{first}`, then with `{second}`"))
        }
        (Ok(_), Err(error)) => Some(format!("rendered, then failed with `{error}`")),
        (Err(error), Ok(_)) => Some(format!("failed with `{error}`, then rendered")),
    }
}

/// Checks what one render laid out: the pages of its inspection, its page count and its
/// reported layout.
fn examine_output(output: &Output, found: &mut Vec<Violation>) -> Stats {
    let mut stats = Stats {
        pages: output.pages,
        ..Stats::default()
    };
    let Some(inspection) = &output.inspection else {
        found.push(Violation::new(
            Invariant::PageCount,
            "the render kept no pages to inspect",
        ));
        return stats;
    };
    let mut checker = Checker {
        inspection,
        found,
        stats: &mut stats,
        boxes: Vec::new(),
    };
    checker.page_count(output);
    for (index, page) in inspection.pages.iter().enumerate() {
        checker.page(index, page);
    }
    match &output.layout {
        Some(layout) => checker.reported(layout),
        None => checker.add(Violation::new(
            Invariant::ReportedLayout,
            "the render reported no layout",
        )),
    }
    stats
}

struct Checker<'a> {
    inspection: &'a Inspection,
    found: &'a mut Vec<Violation>,
    stats: &'a mut Stats,
    /// The boxes of elements in the pages, as they come: the element and the number of the page.
    boxes: Vec<(NodeId, usize)>,
}

impl Checker<'_> {
    fn add(&mut self, violation: Violation) {
        if self.found.len() < MOST {
            self.found.push(violation);
        }
    }

    /// A violation on the page with the index `page`, at the fragment at `path`.
    fn on_page(&mut self, invariant: Invariant, message: String, page: usize, at: Option<(&[usize], Rect)>) {
        if self.found.len() >= MOST {
            return;
        }
        let mut outline = fagus_layout::dump(&self.inspection.pages[page], &self.inspection.document);
        if outline.len() > OUTLINE {
            let mut end = OUTLINE;
            while !outline.is_char_boundary(end) {
                end -= 1;
            }
            outline.truncate(end);
            outline.push_str("…\n");
        }
        self.found.push(Violation {
            invariant,
            message,
            page: Some(page + 1),
            path: at.map(|(path, _)| path.to_vec()).unwrap_or_default(),
            rect: at.map(|(_, rect)| rect),
            outline: Some(outline),
        });
    }

    fn page_count(&mut self, output: &Output) {
        let pages = self.inspection.pages.len();
        if output.pages != pages {
            self.add(Violation::new(
                Invariant::PageCount,
                format!("the output has {} pages, the layout {pages}", output.pages),
            ));
        }
        if let Some(layout) = &output.layout
            && layout.pages.len() != pages
        {
            self.add(Violation::new(
                Invariant::PageCount,
                format!(
                    "the reported layout has {} pages, the layout {pages}",
                    layout.pages.len()
                ),
            ));
        }
        if pages == 0 || pages > MAX_PAGES {
            self.add(Violation::new(
                Invariant::PageCount,
                format!("{pages} pages, outside 1 to {MAX_PAGES}"),
            ));
        }
        let warned = output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "layout.too-many-pages");
        if (pages >= MAX_PAGES) != warned {
            self.add(Violation::new(
                Invariant::PageCount,
                format!(
                    "{pages} pages {} the warning layout.too-many-pages",
                    if warned { "with" } else { "without" }
                ),
            ));
        }
    }

    fn page(&mut self, index: usize, page: &Page) {
        for (name, value) in [
            ("bleed", page.bleed),
            ("scale", page.scale),
            ("shift", page.shift),
        ] {
            if !value.is_finite() {
                self.on_page(
                    Invariant::Finite,
                    format!("the {name} of the page is {value}"),
                    index,
                    None,
                );
            }
        }
        if page.width.0 <= 0 || page.height.0 <= 0 {
            self.on_page(
                Invariant::Sizes,
                format!(
                    "the page is {} by {} pixels",
                    page.width.to_px(),
                    page.height.to_px()
                ),
                index,
                None,
            );
        }
        // Not finite is reported above.
        if page.scale <= 0.0 || page.scale > 1.0 {
            self.on_page(
                Invariant::Sizes,
                format!("the page is shrunk by {}, outside (0, 1]", page.scale),
                index,
                None,
            );
        }
        // Without recursion: fragments nest as deep as elements do, hundreds of levels.
        let mut path: Vec<usize> = Vec::new();
        let mut stack: Vec<(&Fragment, usize, usize, (f32, f32))> = page
            .fragments
            .iter()
            .enumerate()
            .rev()
            .map(|(position, fragment)| (fragment, 0, position, (0.0, 0.0)))
            .collect();
        while let Some((fragment, depth, position, origin)) = stack.pop() {
            path.truncate(depth);
            path.push(position);
            let x = origin.0 + fragment.rect.x.to_px();
            let y = origin.1 + fragment.rect.y.to_px();
            self.fragment(index, &path, fragment, (x, y));
            stack.extend(
                fragment
                    .children
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(position, child)| (child, depth + 1, position, (x, y))),
            );
        }
    }

    /// Checks one fragment, whose corner is at `corner` on the page.
    fn fragment(&mut self, page: usize, path: &[usize], fragment: &Fragment, corner: (f32, f32)) {
        self.stats.fragments += 1;
        let rect = fragment.rect;
        let at = Some((path, rect));
        if rect.width.0 < 0 || rect.height.0 < 0 {
            self.on_page(
                Invariant::Sizes,
                "a fragment has a negative size".to_owned(),
                page,
                at,
            );
        }
        if [corner.0, corner.1, rect.width.to_px(), rect.height.to_px()]
            .iter()
            .any(|length| length.abs() >= SATURATED)
        {
            self.stats.saturated += 1;
        }
        match &fragment.kind {
            FragmentKind::Box { node: Some(node), .. } => self.boxes.push((*node, page + 1)),
            FragmentKind::Placeholder(_) => {
                self.on_page(
                    Invariant::Placeholder,
                    "a placeholder is left in the page".to_owned(),
                    page,
                    at,
                );
            }
            FragmentKind::Text(run) => {
                self.stats.glyphs += run.glyphs.len();
                let mut numbers = vec![("font size", run.size)];
                if let Some(symbol) = run.symbol {
                    numbers.extend([
                        ("marker symbol's x", symbol.x),
                        ("marker symbol's y", symbol.y),
                        ("marker symbol's size", symbol.size),
                    ]);
                }
                for (name, value) in numbers {
                    if !value.is_finite() {
                        self.on_page(
                            Invariant::Finite,
                            format!("the {name} of the text {:?} is {value}", run.text),
                            page,
                            at,
                        );
                    }
                }
                let unfinite = run.glyphs.iter().position(|glyph| {
                    !(glyph.advance.is_finite() && glyph.x_offset.is_finite() && glyph.y_offset.is_finite())
                });
                if let Some(glyph) = unfinite {
                    self.on_page(
                        Invariant::Finite,
                        format!(
                            "glyph {glyph} of the text {:?} is {:?}",
                            run.text, run.glyphs[glyph]
                        ),
                        page,
                        at,
                    );
                }
                let outside = run.glyphs.iter().position(|glyph| {
                    let (start, end) = (glyph.cluster.0 as usize, glyph.cluster.1 as usize);
                    start > end || !run.text.is_char_boundary(start) || !run.text.is_char_boundary(end)
                });
                if let Some(glyph) = outside {
                    self.on_page(
                        Invariant::Sizes,
                        format!(
                            "the cluster {:?} of glyph {glyph} is not in the text {:?}",
                            run.glyphs[glyph].cluster, run.text
                        ),
                        page,
                        at,
                    );
                }
            }
            FragmentKind::Box { node: None, .. }
            | FragmentKind::Line { .. }
            | FragmentKind::Anonymous
            | FragmentKind::Image(_) => {}
        }
    }

    /// Checks the reported layout against the document and the boxes found in the pages.
    fn reported(&mut self, layout: &Layout) {
        let document = &self.inspection.document;
        let pages = self.inspection.pages.len();
        for (index, page) in layout.pages.iter().enumerate() {
            if !(page.width.is_finite() && page.height.is_finite()) {
                self.add(Violation::new(
                    Invariant::Finite,
                    format!("page {} is reported as {page:?}", index + 1),
                ));
            }
        }
        let elements: Vec<(NodeId, &str)> = document
            .document_element()
            .into_iter()
            .flat_map(|root| document.descendants(root))
            .filter_map(|node| {
                let id = document.element(node)?.id.as_deref()?;
                (!id.is_empty()).then_some((node, id))
            })
            .collect();
        let ids = || elements.iter().map(|&(_, id)| id);
        if !layout
            .elements
            .iter()
            .map(|element| element.id.as_str())
            .eq(ids())
        {
            self.add(Violation::new(
                Invariant::ReportedLayout,
                format!(
                    "{} elements are reported, the document has {} with an id, or in another order",
                    layout.elements.len(),
                    elements.len()
                ),
            ));
            return;
        }
        for (element, &(node, id)) in layout.elements.iter().zip(&elements) {
            let rows = element.rows.iter().flat_map(|row| &row.boxes);
            for (index, placed) in element.boxes.iter().chain(rows).enumerate() {
                self.stats.boxes += 1;
                self.reported_box(id, index, placed, pages);
            }
            if !element.boxes.is_sorted_by_key(|placed| placed.page) {
                self.add(Violation::new(
                    Invariant::ReportedLayout,
                    format!("the boxes of #{id} are not in page order"),
                ));
            }
            // The boxes of a row are those of its cells joined, one for each page.
            if element.tag == "tr" {
                continue;
            }
            let reported: Vec<usize> = element.boxes.iter().map(|placed| placed.page).collect();
            let laid_out: Vec<usize> = self
                .boxes
                .iter()
                .filter(|&&(of, _)| of == node)
                .map(|&(_, page)| page)
                .collect();
            if reported != laid_out {
                self.add(Violation::new(
                    Invariant::ReportedLayout,
                    format!(
                        "#{id} is reported with boxes on the pages {reported:?}, its boxes are on {laid_out:?}"
                    ),
                ));
            }
        }
    }

    fn reported_box(&mut self, id: &str, index: usize, placed: &LayoutBox, pages: usize) {
        let numbers = [placed.x, placed.y, placed.width, placed.height];
        if numbers.iter().any(|number| !number.is_finite()) {
            self.add(Violation::new(
                Invariant::Finite,
                format!("box {index} of #{id} is reported as {placed:?}"),
            ));
        } else if placed.width < 0.0 || placed.height < 0.0 {
            self.add(Violation::new(
                Invariant::Sizes,
                format!("box {index} of #{id} is reported with a negative size: {placed:?}"),
            ));
        }
        if placed.page == 0 || placed.page > pages {
            self.add(Violation::new(
                Invariant::ReportedLayout,
                format!(
                    "box {index} of #{id} is reported on page {} of {pages}",
                    placed.page
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use fagus_layout::units::Au;

    use super::*;

    const HTML: &str = "<!DOCTYPE html><style>@page { size: 400px 300px; margin: 20px } \
                        body { margin: 0; font: 14px/20px Inter }</style>\
                        <p id=first style='margin: 0'>One</p>\
                        <div id=tall style='height: 400px; position: relative'>\
                        <b id=corner style='position: absolute; right: 0'>Two</b></div>\
                        <span id=hidden style='display: none'>Three</span>";

    fn rendered() -> Output {
        let document = Document::html(HTML).report_layout(true).inspect(true);
        Engine::new().render(&document).expect("rendered")
    }

    fn violations(output: &Output) -> Vec<Invariant> {
        let mut found = Vec::new();
        examine_output(output, &mut found);
        found.iter().map(|violation| violation.invariant).collect()
    }

    /// The first fragment of the first page for which `wanted` holds.
    fn fragment(output: &mut Output, wanted: impl Fn(&Fragment) -> bool) -> &mut Fragment {
        fn find<'a>(
            fragments: &'a mut [Fragment],
            wanted: &impl Fn(&Fragment) -> bool,
        ) -> Option<&'a mut Fragment> {
            for fragment in fragments {
                if wanted(fragment) {
                    return Some(fragment);
                }
                if let Some(found) = find(&mut fragment.children, wanted) {
                    return Some(found);
                }
            }
            None
        }
        let pages = &mut output.inspection.as_mut().expect("inspection").pages;
        find(&mut pages[0].fragments, &wanted).expect("a fragment")
    }

    fn is_text(fragment: &Fragment) -> bool {
        matches!(fragment.kind, FragmentKind::Text(_))
    }

    #[test]
    fn a_document_holds_the_invariants() {
        let stats = check_fresh(&Engine::new(), &Engine::new(), &Document::html(HTML)).expect("holds");
        assert!(stats.rendered && !stats.over_budget);
        assert_eq!(stats.pages, 2);
        // #first, #tall on two pages and #corner; #hidden has none.
        assert_eq!(stats.boxes, 4);
        assert!(stats.fragments > 8 && stats.glyphs == 6);
        assert_eq!(stats.saturated, 0);
    }

    #[test]
    fn an_empty_document_has_a_page() {
        for html in ["", "<!DOCTYPE html>", "<html style='display: none'>x"] {
            let stats = check(&Engine::new(), &Document::html(html)).expect("holds");
            assert_eq!(stats.pages, 1, "{html}");
        }
    }

    #[test]
    fn finds_numbers_that_are_not_finite() {
        let mut output = rendered();
        let text = fragment(&mut output, is_text);
        let FragmentKind::Text(run) = &mut text.kind else {
            unreachable!()
        };
        run.glyphs[1].advance = f32::NAN;
        assert_eq!(violations(&output), [Invariant::Finite]);

        let mut output = rendered();
        output.inspection.as_mut().expect("inspection").pages[1].shift = f32::INFINITY;
        assert_eq!(violations(&output), [Invariant::Finite]);

        let mut output = rendered();
        output.layout.as_mut().expect("layout").elements[0].boxes[0].x = f32::NAN;
        assert_eq!(violations(&output), [Invariant::Finite]);
    }

    #[test]
    fn finds_negative_sizes_scales_and_clusters_outside_the_text() {
        let mut output = rendered();
        fragment(&mut output, is_text).rect.width = Au(-1);
        let mut found = Vec::new();
        examine_output(&output, &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].invariant, Invariant::Sizes);
        assert_eq!(found[0].page, Some(1));
        assert!(found[0].path.len() > 2);
        assert!(found[0].to_string().contains("text \"One\""), "{}", found[0]);

        let mut output = rendered();
        output.inspection.as_mut().expect("inspection").pages[0].scale = 1.5;
        assert_eq!(violations(&output), [Invariant::Sizes]);

        let mut output = rendered();
        let text = fragment(&mut output, is_text);
        let FragmentKind::Text(run) = &mut text.kind else {
            unreachable!()
        };
        run.glyphs[2].cluster = (2, 4);
        assert_eq!(violations(&output), [Invariant::Sizes]);
    }

    #[test]
    fn finds_a_placeholder() {
        let mut output = rendered();
        fragment(&mut output, is_text).kind = FragmentKind::Placeholder(fagus_layout::BoxId(0));
        assert_eq!(violations(&output), [Invariant::Placeholder]);
    }

    #[test]
    fn finds_page_counts_that_differ() {
        let mut output = rendered();
        output.pages = 3;
        assert_eq!(violations(&output), [Invariant::PageCount]);

        let mut output = rendered();
        output.layout.as_mut().expect("layout").pages.pop();
        assert_eq!(violations(&output), [Invariant::PageCount]);
    }

    #[test]
    fn finds_a_reported_layout_that_differs_from_the_pages() {
        let mut output = rendered();
        output.layout.as_mut().expect("layout").elements[1].boxes.pop();
        assert_eq!(violations(&output), [Invariant::ReportedLayout]);

        let mut output = rendered();
        output.layout.as_mut().expect("layout").elements[1].boxes[1].page = 3;
        assert_eq!(violations(&output), [Invariant::ReportedLayout; 2]);

        let mut output = rendered();
        output.layout.as_mut().expect("layout").elements.swap(0, 1);
        assert_eq!(violations(&output), [Invariant::ReportedLayout]);
    }

    #[test]
    fn finds_renders_that_differ() {
        let render =
            |html: &str| Engine::new().render(&Document::html(html).created(Date::from_unix(CREATED)));
        let (one, other) = (
            render("<p>One"),
            render("<p>Other<p style='break-before: page'>x"),
        );
        assert_eq!(difference(&one, &render("<p>One")), None);
        assert_eq!(difference(&one, &other).as_deref(), Some("1 pages, then 2"));
        let different = difference(&one, &render("<p>Two")).expect("differs");
        assert!(different.starts_with("the PDF differs from byte "), "{different}");
        let failed = Err(Error::Internal("x".to_owned()));
        assert!(difference(&one, &failed).is_some());
    }
}
