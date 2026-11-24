//! Layout and fragmentation for Fagus.
//!
//! [`layout`] builds the box tree from a styled document and paginates it. Fragmentation is part
//! of every layout algorithm (ADR 2): a box that does not fit on a page returns a break token and
//! continues on the next page.

#![forbid(unsafe_code)]

mod anchor;
mod annotation;
pub mod avif;
mod bidi;
mod block;
mod block_justify;
pub mod bmp;
mod breaks;
mod cjk;
mod clamp;
mod colspan;
mod column_positioned;
mod columns;
mod contained_fixed;
mod counter_style;
mod counters;
mod emphasis;
mod empty_piece;
mod fieldset;
mod flex_balance;
mod flex_column;
mod flex_sizing;
mod float;
mod footnote;
mod foreign;
mod fragment;
mod frame;
mod grid;
mod grid_contribution;
mod grid_positioned;
mod image;
mod inline;
mod inline_containing;
mod letter_spacing;
mod list_marker;
mod math;
mod memo;
mod monolithic;
mod page;
mod parallel;
pub mod path_data;
mod positioned;
mod quotes;
mod ratio;
pub mod responsive;
mod ruby;
pub mod scrollbars;
mod shape;
mod shape_gradient;
mod sizing;
mod stacking_owner;
mod sticky;
mod subgrid;
mod table;
mod text_box;
mod text_case;
mod tree;
mod unfragmented;
pub mod units;
mod writing;

pub use counter_style::CounterStyles;
pub use counters::counter_text;
pub use emphasis::{Marks, marks as emphasis_marks};
pub use foreign::{ForeignObject, ForeignObjects, SvgFit};
pub use fragment::{
    Carried, Children, Fragment, FragmentKind, Glyph, GlyphOrientation, MarkerSymbol, Page, PageMarks,
    PageOrientation, SymbolShape, TextRun, VisibleSides,
};
pub use frame::{FrameDocument, content_height, is_responsive, layout_viewport};
pub use image::{
    Image, ImageError, ImageFormat, Images, MAX_PIXELS, PathSegment, PixelFilter, SvgClip, SvgDocument,
    SvgReference,
};
pub use inline::{
    COMBINED_ROOM, StyleFonts, face_for, font_variations, hangable_punctuation, run_variations,
    text_font_style, used_font_size,
};
pub use math::alternative_text as formula_alternative_text;
pub use page::{MAX_PAGES, PageBox, PageGroup, held_by, page_box};
pub use shape::AlphaMask;
pub use tree::forms::control_mark;
pub use tree::transformed_text;
pub use tree::{BoxId, BoxTree, Targets, embedded_image_url};

use fagus_css::RuleSet;
use fagus_dom::Document;
use fagus_style::{ComputedStyle, StyledDocument};
use fagus_text::FontDb;

/// The name of the first page of a styled document (the `page` its first in-flow content
/// starts), if it names one, and the page groups it is in. Chrome takes the viewport, which
/// viewport units are relative to, from the page area of the first page.
pub fn first_page(
    document: &Document,
    styled: &StyledDocument,
    rules: &RuleSet,
    fonts: &FontDb,
) -> (Option<std::sync::Arc<str>>, Vec<PageGroup>) {
    let tree = BoxTree::build_with(
        document,
        styled,
        &Images::default(),
        Targets::default(),
        CounterStyles::new(rules),
        Some(fonts),
        None,
    );
    let Some(root) = tree.root else {
        return (None, Vec::new());
    };
    let layouter = block::Layouter::new(&tree, fonts);
    (
        layouter.start_page(root).cloned(),
        page::first_page_groups(&layouter, rules, root),
    )
}

/// Lays out a styled document into pages.
pub fn layout(
    document: &Document,
    styled: &StyledDocument,
    rules: &RuleSet,
    fonts: &FontDb,
    images: &Images,
) -> Vec<Page> {
    layout_with_targets(document, styled, rules, fonts, images, Targets::default(), None).0
}

/// Lays out a document with what its `target-*()` functions refer to, as a first layout found
/// it, and finds that again: the pages elements with an id start on and their counters. Where
/// the functions were asked, a second layout with what this one found gives their values.
///
/// Form controls write the texts they show of their own (a submit button's label, the
/// placeholders and formats of dates) in the language of their `lang`, else in that of `locale`
/// (a language tag such as `de-DE`), else in American English, as Chrome does in the language of
/// its user interface.
pub fn layout_with_targets(
    document: &Document,
    styled: &StyledDocument,
    rules: &RuleSet,
    fonts: &FontDb,
    images: &Images,
    targets: Targets,
    locale: Option<&str>,
) -> (Vec<Page>, Targets) {
    layout_shrunk(document, styled, rules, fonts, images, targets, locale, 1.0)
}

/// Like [`layout_with_targets`], with the content shrunk by `shrink` (at most 1) and laid out on
/// page areas as much larger, as Chrome prints content wider than its pages (see
/// [`Page::scale`]).
#[allow(clippy::too_many_arguments)]
pub fn layout_shrunk(
    document: &Document,
    styled: &StyledDocument,
    rules: &RuleSet,
    fonts: &FontDb,
    images: &Images,
    targets: Targets,
    locale: Option<&str>,
    shrink: f32,
) -> (Vec<Page>, Targets) {
    let mut tree = BoxTree::build_with(
        document,
        styled,
        images,
        targets,
        CounterStyles::new(rules),
        Some(fonts),
        locale,
    );
    let root_style = document
        .document_element()
        .and_then(|root| styled.style(root))
        .map_or_else(ComputedStyle::default, |style| (**style).clone());
    page::add_margin_images(&mut tree, rules, images);
    scrollbars::add_scrolling(&mut tree);
    let (mut pages, mut numbers) = page::paginate(&tree, fonts, rules, &root_style, shrink);
    // Scroll containers whose content overflows get their scroll bars, and the document is laid
    // out again with them. The pages before go first: `paginate` charges the budget of the
    // render only for the pages it lays out, the caller for those it gets.
    for _ in 0..scrollbars::MAX_PASSES {
        if !scrollbars::add_overflowing(&mut tree, &pages) {
            break;
        }
        drop(pages);
        (pages, numbers) = page::paginate(&tree, fonts, rules, &root_style, shrink);
    }
    let mut found = tree.found_targets().clone();
    if found.asked {
        fn walk(
            fragment: &Fragment,
            document: &Document,
            page: i32,
            pages: &mut rustc_hash::FxHashMap<String, i32>,
        ) {
            if let FragmentKind::Box { node: Some(node), .. } = &fragment.kind
                && let Some(id) = document.element(*node).and_then(|data| data.id.as_deref())
            {
                pages.entry(id.to_owned()).or_insert(page);
            }
            for child in &fragment.children {
                walk(child, document, page, pages);
            }
        }
        for (index, page) in pages.iter().enumerate() {
            let number = numbers
                .get(index)
                .copied()
                .unwrap_or_else(|| i32::try_from(index + 1).unwrap_or(i32::MAX));
            for fragment in &page.fragments {
                walk(fragment, document, number, &mut found.pages);
            }
        }
    }
    (pages, found)
}

/// A readable outline of the fragments of a page, for debugging.
pub fn dump(page: &Page, document: &Document) -> String {
    fn walk(fragment: &Fragment, document: &Document, depth: usize, out: &mut String) {
        use std::fmt::Write;
        let rect = fragment.rect;
        let label = match &fragment.kind {
            FragmentKind::Box { node: Some(node), .. } => {
                let element = document.element(*node);
                let name = element.map_or("?".to_owned(), |data| data.local_name().to_string());
                let class = element
                    .map(|data| data.classes.join("."))
                    .filter(|class| !class.is_empty());
                class.map_or(name.clone(), |class| format!("{name}.{class}"))
            }
            FragmentKind::Box { node: None, .. } => "box".to_owned(),
            FragmentKind::Line { .. } => "line".to_owned(),
            FragmentKind::Text(run) => format!("text {:?}", run.text),
            FragmentKind::Anonymous => "anonymous".to_owned(),
            FragmentKind::Placeholder(id) => format!("placeholder {}", id.0),
            FragmentKind::Image(image) => format!("image {}x{}", image.width, image.height),
        };
        let _ = writeln!(
            out,
            "{:indent$}{label} x={:.1} y={:.1} w={:.1} h={:.1}",
            "",
            rect.x.to_px(),
            rect.y.to_px(),
            rect.width.to_px(),
            rect.height.to_px(),
            indent = depth * 2
        );
        for child in &fragment.children {
            walk(child, document, depth + 1, out);
        }
    }
    let mut out = String::new();
    for fragment in &page.fragments {
        walk(fragment, document, 0, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fagus_css::{MediaEnvironment, Origin, Stylesheet};
    use std::path::Path;
    use std::sync::Arc;

    fn render(html: &str) -> Vec<Page> {
        render_document(html).1
    }

    fn render_document(html: &str) -> (Document, Vec<Page>) {
        render_document_with(html, |_| Images::default())
    }

    /// Like `render_document`, with the images that `images` gives for the parsed document.
    fn render_document_with(html: &str, images: impl FnOnce(&Document) -> Images) -> (Document, Vec<Page>) {
        render_document_in(html, &test_fonts(), images)
    }

    fn test_fonts() -> FontDb {
        let mut fonts = FontDb::new();
        fonts
            .add_font_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts"))
            .expect("test fonts");
        fonts
    }

    /// Like `render_document_with`, with `fonts`.
    fn render_document_in(
        html: &str,
        fonts: &FontDb,
        images: impl FnOnce(&Document) -> Images,
    ) -> (Document, Vec<Page>) {
        let document = Document::parse_html(html);
        let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let css: String = document
            .descendants(fagus_dom::NodeId::DOCUMENT)
            .filter(|&node| document.element(node).is_some_and(|data| data.is("style")))
            .map(|node| document.text_content(node))
            .collect();
        let author = Stylesheet::parse(&css, Origin::Author, "style");
        let environment = MediaEnvironment::default();
        // In quirks mode the user agent stylesheet has the rules of quirks mode after it, as the
        // engine adds them.
        let quirks = Stylesheet::parse(fagus_style::QUIRKS_CSS, Origin::UserAgent, "quirks.css");
        let rules = if document.quirks_mode() {
            RuleSet::new(&[&ua, &quirks, &author], &environment)
        } else {
            RuleSet::new(&[&ua, &author], &environment)
        };
        let styled = fagus_style::compute_styles(&document, &rules, (environment.width, environment.height));
        let images = images(&document);
        let pages = layout(&document, &styled, &rules, fonts, &images);
        (document, pages)
    }

    /// The fragment of the element with class `class` and its border box position on the page.
    fn find<'p>(page: &'p Page, document: &Document, class: &str) -> Option<(f32, f32, &'p Fragment)> {
        fn walk<'p>(
            fragment: &'p Fragment,
            document: &Document,
            class: &str,
            origin: (f32, f32),
        ) -> Option<(f32, f32, &'p Fragment)> {
            let x = origin.0 + fragment.rect.x.to_px();
            let y = origin.1 + fragment.rect.y.to_px();
            if let FragmentKind::Box { node: Some(node), .. } = &fragment.kind
                && document
                    .element(*node)
                    .is_some_and(|data| data.classes.iter().any(|name| &**name == class))
            {
                return Some((x, y, fragment));
            }
            fragment
                .children
                .iter()
                .find_map(|child| walk(child, document, class, (x, y)))
        }
        page.fragments
            .iter()
            .find_map(|fragment| walk(fragment, document, class, (0.0, 0.0)))
    }

    fn contains_text(fragment: &Fragment, needle: &str) -> bool {
        matches!(&fragment.kind, FragmentKind::Text(run) if run.text.contains(needle))
            || fragment.children.iter().any(|child| contains_text(child, needle))
    }

    fn lines(fragment: &Fragment) -> usize {
        usize::from(matches!(fragment.kind, FragmentKind::Line { .. }))
            + fragment.children.iter().map(lines).sum::<usize>()
    }

    /// The text in a page's margin boxes, each run followed by `|`.
    fn margin_text(page: &Page) -> String {
        fn collect(fragment: &Fragment, out: &mut String) {
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push_str(run.text.trim());
                out.push('|');
            }
            for child in &fragment.children {
                collect(child, out);
            }
        }
        let mut text = String::new();
        for fragment in &page.fragments[1..] {
            collect(fragment, &mut text);
        }
        text
    }

    #[test]
    fn shows_running_elements_and_named_strings_in_the_margins() {
        let pages = render(
            "<style>
               @page { margin: 30mm 20mm;
                 @top-left { content: string(chapter); }
                 @top-right { content: string(chapter, last); }
                 @bottom-center { content: element(footer); } }
               @page :first { @top-left { content: string(chapter, first-except); } }
               body { font-family: Inter }
               .footer { position: running(footer); }
               .footer span::after { content: counter(page) '/' counter(pages); }
               h2 { string-set: chapter content(); }
               .break { break-before: page; }
             </style>
             <div class=footer>Fuß <span></span></div>
             <h2>Eins</h2><p>a</p><h2>Zwei</h2>
             <h2 class=break>Drei</h2>
             <p class=break>Ende</p>",
        );
        assert_eq!(pages.len(), 3);
        assert!(!contains_text(&pages[0].fragments[0], "Fuß"), "out of the flow");
        // `first-except` shows nothing on a page where the string is set; `last` the last value.
        assert_eq!(margin_text(&pages[0]), "Zwei|Fuß|1/3|");
        assert_eq!(margin_text(&pages[1]), "Drei|Drei|Fuß|2/3|");
        // Without an assignment, a page shows the value it starts with.
        assert_eq!(margin_text(&pages[2]), "Drei|Drei|Fuß|3/3|");
    }

    #[test]
    fn ignores_content_that_combines_a_running_element_with_other_values() {
        // `element()` cannot be combined with other values (CSS Generated Content for Paged
        // Media 3, §1.2.1): such a declaration is invalid and the one before it holds; after
        // `var()`, the box has no content.
        let pages = render(
            "<style>
               @page {
                 @top-left { content: 'before'; }
                 @top-left { content: element(a) element(b); }
                 @top-center { content: 'before'; }
                 @top-center { content: element(a) ' – ' counter(page); }
                 @top-right { --more: ' – '; content: element(b) var(--more); }
                 @bottom-center { content: element(b); }
               }
               body { font-family: Inter }
               .a { position: running(a); } .b { position: running(b); }
             </style>
             <div class=a>A</div><div class=b>B</div><p>Text</p>",
        );
        assert_eq!(margin_text(&pages[0]), "before|before|B|");
    }

    #[test]
    fn assigns_the_named_strings_of_row_groups_at_their_first_cell() {
        // The first row of the second group is anonymous; the group's string still counts, and
        // before the strings of its rows.
        let pages = render(
            "<style>
               @page { @top-left { content: string(group, first); }
                       @top-right { content: string(group, last); } }
               body { font-family: Inter }
               .table { display: table } .group { display: table-row-group }
               .row { display: table-row } .cell { display: table-cell }
             </style>
             <div class=table>
               <div class=group style='string-set: group \"Eins\"'>
                 <div class=row style='string-set: group \"Zeile\"'><div class=cell>a</div></div>
               </div>
               <div class=group style='string-set: group \"Zwei\"'><div class=cell>b</div></div>
             </div>",
        );
        assert_eq!(margin_text(&pages[0]), "Eins|Zwei|");
    }

    #[test]
    fn counts_page_counters_in_named_strings_on_the_page_of_the_element() {
        // The page counters take the values of the page the heading is on, also on the pages
        // after it that show the string it set; the counter style applies.
        let pages = render(
            "<style>
               @page { @top-left { content: string(chapter); } }
               @page :nth(3) { counter-reset: page 10; }
               body { font-family: Inter }
               h2 { string-set: chapter content() ', ' counter(page) ' of ' counter(pages, upper-roman); }
               .break { break-before: page; }
             </style>
             <h2>Eins</h2>
             <p class=break>a</p>
             <h2 class=break>Zwei</h2>",
        );
        assert_eq!(pages.len(), 3);
        assert_eq!(margin_text(&pages[0]), "Eins, 1 of III|");
        assert_eq!(margin_text(&pages[1]), "Eins, 1 of III|");
        assert_eq!(margin_text(&pages[2]), "Zwei, 10 of III|");
    }

    /// The lines of the content of each page.
    fn content_lines(pages: &[Page]) -> Vec<Vec<String>> {
        pages
            .iter()
            .map(|page| {
                let mut out = Vec::new();
                line_texts(&page.fragments[0], &mut out);
                out
            })
            .collect()
    }

    #[test]
    fn counts_page_counters_in_named_strings_in_the_content_on_the_page_of_the_element() {
        // `string()` in the content shows the page counters as the margin boxes do: with the
        // values of the page the heading is on, also two pages after it and where a page resets
        // the counter. (The page count is not known while the content is laid out.)
        let pages = render(
            "<style>
               @page { size: 400px 300px; margin: 0; @top-left { content: string(chapter); } }
               @page :nth(3) { counter-reset: page 10; }
               body { margin: 0; font: 10px/20px Inter }
               h2 { margin: 0; font: inherit; break-before: page;
                    string-set: chapter content() ', ' counter(page, upper-roman) ' after '
                                counter(-fagus-previous-page) ' of ' counter(pages); }
               p { margin: 0 } p::before { content: string(chapter) }
               .break { break-before: page; }
             </style>
             <p></p>
             <h2>Eins</h2><p></p>
             <h2>Zwei</h2><p></p>
             <p class=break></p>
             <p class=break></p>",
        );
        assert_eq!(
            content_lines(&pages),
            [
                vec![""],
                vec!["Eins", "Eins, II after 1 of 0"],
                vec!["Zwei", "Zwei, X after 2 of 0"],
                vec!["Zwei, X after 2 of 0"],
                vec!["Zwei, X after 2 of 0"],
            ]
        );
        // The margin boxes show the same, with the page count.
        assert_eq!(margin_text(&pages[4]), "Zwei, X after 2 of 5|");
    }

    #[test]
    fn counts_page_counters_in_named_strings_set_on_the_page_that_shows_them() {
        // The element that sets the string is on the page that shows it: the counters are those
        // of that page, for the string of a box that starts there and for the one of a box that
        // goes on from the page before.
        let pages = render(
            "<style>
               @page { size: 400px 300px; margin: 0 }
               body { margin: 0; font: 10px/20px Inter }
               div { string-set: part 'Part ' counter(page) }
               h2 { margin: 0; font: inherit;
                    string-set: chapter content() ' ' counter(page) ' before ' counter(-fagus-next-page); }
               p { margin: 0 } p::before { content: string(part) ', ' string(chapter) }
               .break { break-before: page; }
             </style>
             <div><h2>Eins</h2><p></p>
             <h2 class=break>Zwei</h2><p></p></div>",
        );
        assert_eq!(
            content_lines(&pages),
            [
                vec!["Eins", "Part 1, Eins 1 before 2"],
                vec!["Zwei", "Part 1, Zwei 2 before 3"],
            ]
        );
    }

    #[test]
    fn counts_page_counters_in_named_strings_in_repeated_table_rows_on_the_page_of_the_element() {
        use std::fmt::Write;
        let mut rows = String::new();
        for n in 1..=30 {
            write!(rows, "<tr data-n=\"{n}\"><td>Position {n}</td></tr>").expect("written");
        }
        // The header shows the chapter with the page it starts on, on every page of the table;
        // the row carried over shows the last row of the page before with that page, the footer
        // the last row above it with this page.
        let pages = render(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 10px/20px Inter }}
               table {{ border-spacing: 0 }} td, th {{ padding: 0; font-weight: normal; text-align: left }}
               h2 {{ margin: 0; font: inherit; string-set: chapter content() ' from ' counter(page) }}
               tbody tr {{ string-set: row attr(data-n) ' on ' counter(page) }}
               th::before {{ content: string(chapter) }}
               .carry {{ -fagus-repeat: at-break }}
               .carry td::before, tfoot td::before {{ content: string(row) }}</style>
             <h2>Eins</h2>
             <table><thead><tr><th></th></tr><tr class=carry><td></td></tr></thead>
             <tfoot><tr><td></td></tr></tfoot>
             <tbody>{rows}</tbody></table>"
        ));
        let lines = content_lines(&pages);
        assert_eq!(pages.len(), 3);
        assert_eq!(lines[0][..3], ["Eins", "Eins from 1", "Position 1"]);
        assert_eq!(lines[0][lines[0].len() - 2..], ["Position 12", "12 on 1"]);
        assert_eq!(lines[1][..3], ["Eins from 1", "12 on 1", "Position 13"]);
        assert_eq!(lines[1][lines[1].len() - 2..], ["Position 24", "24 on 2"]);
        assert_eq!(lines[2][..3], ["Eins from 1", "24 on 2", "Position 25"]);
        assert_eq!(lines[2][lines[2].len() - 2..], ["Position 30", "30 on 3"]);
    }

    #[test]
    fn assigns_the_named_strings_of_elements_without_a_box() {
        // An element with `display: none` sets its strings where its box would be: where the
        // next box starts, or, after the last, where the content ends. Its counters do not
        // change.
        let pages = render(
            "<style>
               @page { @top-left { content: string(title, first); }
                       @top-right { content: string(title, last); } }
               body { font-family: Inter; counter-reset: n }
               h1 { display: none; counter-increment: n; string-set: title content() ' ' counter(n); }
               .break { break-before: page; }
             </style>
             <h1>Eins</h1><p>a</p>
             <div class=break><h1>Zwei</h1><p>b</p><h1>Drei</h1></div>",
        );
        assert_eq!(pages.len(), 2);
        assert_eq!(margin_text(&pages[0]), "Eins 0|Eins 0|");
        assert_eq!(margin_text(&pages[1]), "Zwei 0|Drei 0|");
    }

    #[test]
    fn assigns_the_named_strings_of_elements_without_a_box_of_their_own_at_the_next_box() {
        for (html, expected) in [
            (
                "<div style='display: contents; string-set: t \"C\"'><p>x</p></div>",
                "C|",
            ),
            ("<p>a<br style='string-set: t \"B\"'><span>x</span></p>", "B|"),
            (
                "<p>a <b style='display: none; string-set: t \"I\"'></b><span>x</span></p>",
                "I|",
            ),
            (
                "<table><tr style='display: none; string-set: t \"R\"'><td>a</td></tr><tr><td>b</td></tr></table>",
                "R|",
            ),
            (
                "<div style='display: flex'><b style='display: none; string-set: t \"F\"'></b><i>x</i></div>",
                "F|",
            ),
        ] {
            let pages = render(&format!(
                "<style>@page {{ @top-left {{ content: string(t); }} }} body {{ font-family: Inter }}</style>{html}"
            ));
            assert_eq!(margin_text(&pages[0]), expected, "{html}");
        }
    }

    #[test]
    fn breaks_and_sizes_named_pages() {
        let pages = render(
            "<style>
               @page { size: A5; }
               @page wide { size: A5 landscape; @top-center { content: 'Breit'; } }
               body { font-family: Inter }
               .wide { page: wide; }
             </style>
             <p>Hoch</p><div class=wide><p>Quer 1</p></div><div class=wide><p>Quer 2</p></div>
             <p>Wieder hoch</p>",
        );
        assert_eq!(pages.len(), 3);
        assert!(pages[0].width < pages[0].height);
        assert!(pages[1].width > pages[1].height, "the named page");
        assert!(
            contains_text(&pages[1].fragments[0], "Quer 1")
                && contains_text(&pages[1].fragments[0], "Quer 2"),
            "boxes with the same page name share a page"
        );
        assert_eq!(margin_text(&pages[1]), "Breit|");
        assert!(pages[2].width < pages[2].height);
        assert!(contains_text(&pages[2].fragments[0], "Wieder hoch"));
    }

    /// The page positions of the line boxes below `fragment`: (x, y, width).
    fn line_boxes(fragment: &Fragment, origin: (f32, f32), out: &mut Vec<(f32, f32, f32)>) {
        let x = origin.0 + fragment.rect.x.to_px();
        let y = origin.1 + fragment.rect.y.to_px();
        if matches!(fragment.kind, FragmentKind::Line { .. }) && fragment.rect.height > units::Au::ZERO {
            out.push((x, y, fragment.rect.width.to_px()));
        }
        for child in &fragment.children {
            line_boxes(child, (x, y), out);
        }
    }

    /// The text runs below `fragment`, in order.
    fn runs<'f>(fragment: &'f Fragment, out: &mut Vec<&'f TextRun>) {
        if let FragmentKind::Text(run) = &fragment.kind {
            out.push(run);
        }
        for child in &fragment.children {
            runs(child, out);
        }
    }

    #[test]
    fn makes_up_small_capitals_as_chrome_does() {
        let render = |css: &str| {
            let pages = render(&format!(
                "<style>body {{ font: 16px Inter; margin: 0 }} p {{ {css} }}</style><p>Rechnung über 12 €</p>"
            ));
            let mut found = Vec::new();
            runs(&pages[0].fragments[0], &mut found);
            found
                .iter()
                .map(|run| (run.text.clone(), run.size))
                .collect::<Vec<_>>()
        };
        let expect = |expected: &[(&str, f32)]| -> Vec<(String, f32)> {
            expected
                .iter()
                .map(|(text, size)| ((*text).to_owned(), *size))
                .collect()
        };
        // Inter has no small capitals: lowercase letters become capitals at 70 % of the size,
        // rounded; with `all-small-caps` everything does, with unicase all but them.
        assert_eq!(
            render("font-variant-caps: small-caps"),
            expect(&[
                ("R", 16.0),
                ("echnung", 11.0),
                (" ", 16.0),
                ("über", 11.0),
                (" 12 €", 16.0)
            ])
        );
        assert_eq!(
            render("font-variant: all-small-caps"),
            expect(&[
                ("R", 11.0),
                ("echnung", 11.0),
                (" ", 11.0),
                ("über", 11.0),
                (" 12 €", 11.0)
            ])
        );
        assert_eq!(
            render("font-variant-caps: unicase"),
            expect(&[
                ("R", 11.0),
                ("echnung", 16.0),
                (" ", 11.0),
                ("über", 16.0),
                (" 12 €", 11.0)
            ])
        );
        assert_eq!(
            render("font-variant-caps: small-caps; font-synthesis: none"),
            expect(&[("Rechnung über 12 €", 16.0)])
        );
        assert_eq!(
            render("font-variant-caps: titling-caps"),
            expect(&[("Rechnung über 12 €", 16.0)])
        );
        // The capitals are glyphs of capitals, mapped to the letters of the text.
        let pages =
            render_document("<p style='font: small-caps 16px Inter'>e</p><p style='font: 16px Inter'>E</p>")
                .1;
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], &mut found);
        assert_eq!(found[0].glyphs[0].id, found[1].glyphs[0].id);
        assert_eq!(
            (found[0].text.as_str(), found[0].glyphs[0].cluster),
            ("e", (0, 1))
        );
    }

    #[test]
    fn makes_small_capitals_of_letters_that_change_when_uppercased() {
        // `ß` becomes two small capitals S, as in Chrome, like the letters around it.
        let pages = render_document(
            "<p style='font: small-caps 16px Inter'>Fuß</p><p style='font: 11px Inter'>USS</p>",
        )
        .1;
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], &mut found);
        let small: Vec<(&str, f32, usize)> = found
            .iter()
            .map(|run| (run.text.as_str(), run.size, run.glyphs.len()))
            .collect();
        assert_eq!(small[..2], [("F", 16.0, 1), ("uß", 11.0, 3)]);
        let glyphs = |index: usize| {
            found[index]
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(glyphs(1), glyphs(2));
    }

    #[test]
    fn refers_to_targets_and_fills_leaders() {
        let html = "<style>@page { margin: 0; size: 300px 200px } body { font: 16px/20px Inter; margin: 0;
               counter-reset: chapter } h2 { break-before: page; counter-increment: chapter; margin: 0 }
               .toc a { display: block } .toc a::after { content: leader('.') target-counter(attr(href), page) }
               .toc a::before { content: target-counter(attr(href), chapter) '. ' }
               .ref::after { content: ' (' target-text(attr(href)) ')' }</style>
             <div class=toc><a href='#a'>Eins</a><a href='#b'>Zwei</a></div><a class=ref href='#b'>Verweis</a>
             <h2 id=a>Anfang</h2><h2 id=b>Ende</h2>";
        let document = Document::parse_html(html);
        let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let css: String = document
            .descendants(fagus_dom::NodeId::DOCUMENT)
            .filter(|&node| document.element(node).is_some_and(|data| data.is("style")))
            .map(|node| document.text_content(node))
            .collect();
        let author = Stylesheet::parse(&css, Origin::Author, "style");
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = fagus_style::compute_styles(&document, &rules, (environment.width, environment.height));
        let fonts = test_fonts();
        let images = Images::default();
        let (_, targets) = layout_with_targets(
            &document,
            &styled,
            &rules,
            &fonts,
            &images,
            Targets::default(),
            None,
        );
        assert!(targets.asked);
        assert_eq!(
            (targets.pages.get("a"), targets.pages.get("b")),
            (Some(&2), Some(&3))
        );
        let (pages, _) = layout_with_targets(&document, &styled, &rules, &fonts, &images, targets, None);
        let mut found = Vec::new();
        line_texts(&pages[0].fragments[0], &mut found);
        // The number of the page after a leader of dots, and the counter of the target before.
        assert!(
            found[0].starts_with("1. Eins...") && found[0].ends_with(".2"),
            "{found:?}"
        );
        assert!(
            found[1].starts_with("2. Zwei") && found[1].ends_with(".3"),
            "{found:?}"
        );
        assert_eq!(found[2], "Verweis (Ende)");
    }

    #[test]
    fn lays_out_form_controls_as_chrome_does() {
        let (document, pages) = render_document(
            "<!DOCTYPE html><style>@page { margin: 0 } body { margin: 0; font: 16px/20px Inter }
               input, textarea, select { font-family: Inter }</style>
             <p><input class=text value=Wert><input class=small size=1><input class=check type=checkbox checked>
               <select class=select><option>kurz</option><option selected>eine lange Wahl</option></select></p>
             <p><textarea class=area rows=3 cols=10>a</textarea>
               <input class=styled style='border: 3px solid red; width: 50px'></p>
             <fieldset class=set><legend class=legend>Gruppe</legend>Inhalt</fieldset>",
        );
        let rect = |class: &str| {
            let (x, y, fragment) = find(&pages[0], &document, class).expect(class);
            (x, y, fragment.rect.width.to_px(), fragment.rect.height.to_px())
        };
        // Inter at 13.33px on Linux: 8.54px a character (its OS/2 table), 9 rounded up, and the
        // box around its glyphs 44px wide; 20 characters, the box less a character, a 2px
        // padding and a 2px border on each side.
        assert_eq!(rect("text").2, 9.0 * 20.0 + 44.0 - 9.0 + 8.0);
        assert_eq!(rect("small").2, 44.0 + 8.0);
        // One line of text, 16px high, in the frame.
        assert_eq!(rect("text").3, 22.0);
        // The checkbox is 13px square, its bottom on the baseline of the text field.
        let (_, check_y, check_width, check_height) = rect("check");
        assert_eq!((check_width, check_height), (13.0, 13.0));
        let text = rect("text");
        let (_, _, field) = find(&pages[0], &document, "text").expect("text");
        let baseline = text.1
            + block::first_baseline(field, fagus_style::values::WritingMode::HorizontalTb)
                .expect("a line")
                .to_px();
        assert!((check_y + 13.0 - baseline).abs() < 0.5, "{check_y} {baseline}");
        // A select is as wide as its longest option, with room for the arrow.
        assert!(rect("select").2 > 100.0);
        // Ten characters wide, three lines high, in a 2px padding and a 1px border: its text
        // fits, so it has no scroll bar (Chrome 157 on Linux).
        let area = rect("area");
        assert!((area.2 - (90.0 + 6.0)).abs() < 0.5, "{area:?}");
        assert_eq!(area.3, 3.0 * 16.0 + 6.0);
        // Author borders replace the native frame.
        assert_eq!(rect("styled").2, 50.0 + 4.0 + 6.0);
        // The legend sits on the top border of its fieldset.
        let (set, legend) = (rect("set"), rect("legend"));
        assert_eq!(legend.1, set.1);
        assert_eq!(legend.0, set.0 + 2.0 + 12.0);
    }

    #[test]
    fn writes_form_controls_in_the_language_of_their_lang() {
        let html =
            "<html lang=de><style>@page { margin: 0 } body { margin: 0; font: 16px/20px Inter }</style>
             <p><input type=submit> <input type=date value=2026-11-04></p>
             <p lang=fr><input type=reset> <input type=number value=1.5></p>
             <p lang=ja><input type=file></p>
             <p lang=''><input type=time value=14:05></p>";
        let texts = |html: &str, locale: Option<&str>| {
            let document = Document::parse_html(html);
            let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
            let rules = RuleSet::new(&[&ua], &MediaEnvironment::default());
            let styled = fagus_style::compute_styles(&document, &rules, (800.0, 600.0));
            let fonts = test_fonts();
            let (pages, _) = layout_with_targets(
                &document,
                &styled,
                &rules,
                &fonts,
                &Images::default(),
                Targets::default(),
                locale,
            );
            let mut found = Vec::new();
            for fragment in &pages[0].fragments {
                line_texts(fragment, &mut found);
            }
            found.join("|")
        };
        // `lang` is inherited; languages Fagus has no texts for, and an empty `lang`, take the
        // engine's language, else American English.
        assert_eq!(
            texts(html, None),
            "Senden 04.11.2026|Réinitialiser 1,5|Choose FileNo file chosen|02:05 PM"
        );
        assert_eq!(
            texts(html, Some("pl-PL")),
            "Senden 04.11.2026|Réinitialiser 1,5|Wybierz plikNie wybrano pliku|14:05"
        );
        let plain = "<p><input type=submit> <input type=file multiple>";
        assert_eq!(texts(plain, None), "Submit Choose FilesNo file chosen");
        assert_eq!(texts(plain, Some("en-GB")), "Submit Choose filesNo file chosen");
        assert_eq!(
            texts(plain, Some("nl")),
            "Verzenden Bestanden kiezenGeen bestand gekozen"
        );
    }

    #[test]
    fn shows_details_as_chrome_does() {
        let pages = render(
            "<style>body { font: 16px/20px Inter }</style>
             <details><summary>Eins</summary><p>verborgen</p></details>
             <details open><summary>Zwei</summary><p>offen</p></details>
             <details lang=fr><p>verborgen</p></details>
             <details open lang=de>drei</details>",
        );
        let mut found = Vec::new();
        line_texts(&pages[0].fragments[0], &mut found);
        // Closed, only the summary shows, after a triangle; without a summary, Chrome's own in
        // the language of the element.
        assert_eq!(found, ["▸Eins", "▾Zwei", "offen", "▸Détails", "▾Details", "drei"]);
    }

    #[test]
    fn stretches_boxes_to_fill_their_containing_block() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 } .wrap { width: 400px; height: 100px; position: relative }
               .s { width: stretch; margin: 0 10px; padding: 5px; border: 2px solid }
               .float { float: left } .block { display: inline-block }
               .abs { position: absolute; left: 20px; top: 0; width: -webkit-fill-available }
               .tall, .framed { height: stretch; margin: 10px 0; width: 10px }
               .frame { padding: 1px 0 }</style>
             <div class=wrap><div class='s float'>a</div></div>
             <div class=wrap><span class='s block'>b</span></div>
             <div class=wrap><div class='s abs'>c</div></div>
             <div class=wrap><div class=tall></div></div>
             <div class='wrap frame'><div class=framed></div></div>",
        );
        let width = |class: &str| {
            let (x, _, fragment) = find(&pages[0], &document, class).expect(class);
            (x, fragment.rect.width.to_px(), fragment.rect.height.to_px())
        };
        // Floats and inline blocks, which `auto` shrinks, fill the 400px less their margins.
        assert_eq!(width("float").1, 380.0);
        assert_eq!(width("block").1, 380.0);
        // From the inset on.
        assert_eq!(width("abs").1, 360.0);
        // A definite containing block height, less the margins that cannot collapse through its
        // edges, as in Chrome.
        assert_eq!(width("tall").2, 100.0);
        assert_eq!(width("framed").2, 80.0);
    }

    #[test]
    fn styles_the_first_letter_and_the_first_line() {
        // The text runs of a fragment with their positions and sizes.
        fn runs(fragment: &Fragment, origin: (f32, f32), out: &mut Vec<(f32, f32, f32, String)>) {
            let (x, y) = (
                origin.0 + fragment.rect.x.to_px(),
                origin.1 + fragment.rect.y.to_px(),
            );
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push((x, y + run.baseline.to_px(), run.size, run.text.clone()));
            }
            for child in &fragment.children {
                runs(child, (x, y), out);
            }
        }
        let (_, pages) = render_document(
            "<style>@page { margin: 0 } body { font: 16px/24px Inter; margin: 0 } p { width: 300px; margin: 0 0 24px }
               .a::first-letter { font-size: 32px } .b::first-line { font-size: 24px }
               .c::first-letter { initial-letter: 3 }</style>
             <p class=a>„Lorem ipsum</p>
             <p class=b>Ein langer Absatz, der auf mehr als eine Zeile umbricht und noch weiter geht.</p>
             <p class=c>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt
               ut labore et dolore magna aliqua.</p>",
        );
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], (0.0, 0.0), &mut found);
        // The first letter with the quotation mark before it.
        assert_eq!((found[0].2, found[0].3.as_str()), (32.0, "„L"));
        assert_eq!((found[1].2, found[1].3.as_str()), (16.0, "orem ipsum"));
        // The first line in its own size, the others in theirs.
        let second: Vec<_> = found.iter().filter(|run| run.1 > 30.0 && run.1 < 120.0).collect();
        assert!(
            second.first().is_some_and(|run| (run.2 - 24.0).abs() < 0.01),
            "{second:?}"
        );
        assert!(
            second.last().is_some_and(|run| (run.2 - 16.0).abs() < 0.01),
            "{second:?}"
        );
        // An initial letter as tall as three lines, the lines beside it starting where its ink
        // ends and the fourth at the start.
        let letter = found
            .iter()
            .find(|run| run.3 == "L" && run.2 > 32.0)
            .expect("an initial letter");
        assert!((letter.2 - 82.0).abs() < 1.0, "{letter:?}");
        let lines: Vec<_> = found
            .iter()
            .filter(|run| run.1 > letter.1 - 60.0 && run.3 != "L")
            .collect();
        assert!(lines[..3].iter().all(|run| run.0 > 30.0), "{lines:?}");
        assert!(lines.iter().any(|run| run.0.abs() < 0.01), "{lines:?}");
    }

    #[test]
    fn clones_decorations_of_broken_boxes() {
        // All fragments of the element with class `class`, with their positions on the page.
        fn all<'p>(
            fragment: &'p Fragment,
            document: &Document,
            class: &str,
            origin: (f32, f32),
            out: &mut Vec<(f32, f32, &'p Fragment)>,
        ) {
            let x = origin.0 + fragment.rect.x.to_px();
            let y = origin.1 + fragment.rect.y.to_px();
            if let FragmentKind::Box { node: Some(node), .. } = &fragment.kind
                && document
                    .element(*node)
                    .is_some_and(|data| data.classes.iter().any(|name| &**name == class))
            {
                out.push((x, y, fragment));
            }
            for child in &fragment.children {
                all(child, document, class, (x, y), out);
            }
        }
        let sides = |fragment: &Fragment| match &fragment.kind {
            FragmentKind::Box { sides, .. } => *sides,
            _ => panic!("a box"),
        };
        let (document, pages) = render_document(
            "<style>@page { margin: 0; size: 300px 120px } body { font: 16px/20px Inter; margin: 0 }
               p { width: 200px; margin: 0 } span { padding: 0 10px; border: 0 solid; border-width: 0 2px }
               .clone { box-decoration-break: clone }
               div { border: 4px solid; padding: 6px; box-decoration-break: clone }</style>
             <p><span class=slice>ein Text, der über drei Zeilen umbricht und weiterläuft</span></p>
             <p><span class=clone>ein Text, der über drei Zeilen umbricht und weiterläuft</span></p>
             <div class=block>1<br>2<br>3<br>4<br>5<br>6</div>",
        );
        let mut slice = Vec::new();
        let mut clone = Vec::new();
        for fragment in &pages[0].fragments {
            all(fragment, &document, "slice", (0.0, 0.0), &mut slice);
            all(fragment, &document, "clone", (0.0, 0.0), &mut clone);
        }
        assert!(slice.len() > 1 && slice.len() == clone.len());
        // Sliced, the fragments on later lines start at the line's start, without the box's
        // start; cloned, each has its border and padding at both ends.
        assert!(!sides(slice[1].2).left && !sides(slice[0].2).right);
        assert!(clone.iter().all(|(_, _, fragment)| sides(fragment).all()));
        assert_eq!(clone[1].0, 0.0);
        let first_text = |fragment: &Fragment| {
            fragment
                .children
                .iter()
                .find(|child| matches!(child.kind, FragmentKind::Text(_)))
                .map(|child| child.rect.x.to_px())
        };
        assert_eq!(
            first_text(clone[1].2),
            Some(12.0),
            "after the cloned border and padding"
        );
        // A block on two pages has its border and padding on both.
        let mut blocks = Vec::new();
        for page in &pages {
            for fragment in &page.fragments {
                all(fragment, &document, "block", (0.0, 0.0), &mut blocks);
            }
        }
        assert!(blocks.len() >= 2, "{} fragments", blocks.len());
        assert!(blocks.iter().all(|(_, _, fragment)| sides(fragment).all()));
        // On the next page its content starts below the cloned border and padding.
        let line = blocks[1]
            .2
            .children
            .iter()
            .find(|child| matches!(child.kind, FragmentKind::Line { .. }))
            .expect("a line");
        assert_eq!(line.rect.y.to_px(), 10.0);
    }

    #[test]
    fn clamps_lines_with_an_ellipsis() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { font: 16px/20px Inter; margin: 0 }
               .clamp { width: 250px; overflow: hidden; display: -webkit-box; -webkit-box-orient: vertical;
                        -webkit-line-clamp: 2 } p { margin: 0 } .after { height: 5px }</style>
             <div class=clamp>Sehr geehrte Damen und Herren, wir erlauben uns, Ihnen die folgenden Leistungen
               in Rechnung zu stellen.</div><div class=after></div>
             <div class=clamp><p>Eins.</p><p>Zwei mit mehr Text, der umbricht und weitergeht.</p><p>Drei.</p></div>
             <div class=clamp style='display: block'>Ein Absatz ohne Clamp, der auf mehr als zwei Zeilen umbricht
               und weitergeht.</div>",
        );
        let mut found = Vec::new();
        line_texts(&pages[0].fragments[0], &mut found);
        let found: Vec<String> = found.iter().map(|line| line.trim().to_owned()).collect();
        // Two lines, the second cut to end with an ellipsis, as in Chrome; across paragraphs too;
        // a block that is no vertical -webkit-box is not clamped. The lines after them are laid
        // out as overflow, which `overflow: hidden` clips.
        assert_eq!(
            found[..2],
            ["Sehr geehrte Damen und", "Herren, wir erlauben uns, Ihne…"]
        );
        let second = found.iter().position(|line| line == "Eins.").expect("second box");
        assert_eq!(
            found[second..second + 2],
            ["Eins.", "Zwei mit mehr Text, der umbric…"]
        );
        assert!(found.len() > 6, "{found:?}");
        // The box ends with the last line it shows.
        let (_, y, _) = find(&pages[0], &document, "after").expect("after");
        assert_eq!(y, 40.0);
    }

    #[test]
    fn clamped_lines_overflow_a_box_that_does_not_clip() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { font: 16px/20px Inter; margin: 0 }
               .clamp { width: 250px; display: -webkit-box; -webkit-box-orient: vertical;
                        -webkit-line-clamp: 2 } p { margin: 0 } .after { height: 5px }</style>
             <div class=clamp><p>Eins zwei drei.</p><p>Vier fünf sechs sieben acht neun zehn elf zwölf
               dreizehn vierzehn.</p></div><div class=after></div>",
        );
        let mut found = Vec::new();
        line_texts(&pages[0].fragments[0], &mut found);
        let found: Vec<String> = found.iter().map(|line| line.trim().to_owned()).collect();
        // The second line ends with an ellipsis as more follows, and the lines after it are laid
        // out still, below it, as Chrome paints them.
        assert!(found[1].ends_with('…'), "{found:?}");
        assert!(found.len() > 2, "{found:?}");
        assert!(found.iter().any(|line| line.contains("vierzehn")), "{found:?}");
        // The box ends with its second line all the same.
        let (_, y, _) = find(&pages[0], &document, "after").expect("after");
        assert_eq!(y, 40.0);
    }

    #[test]
    fn places_boxes_against_their_anchors() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0; size: 400px 300px } body { margin: 0 }
               .box { position: relative; width: 400px; height: 200px }
               .anchor { anchor-name: --a; position: absolute; left: 100px; top: 40px; width: 80px; height: 30px }
               .below { position: absolute; position-anchor: --a; top: anchor(bottom); left: anchor(left);
                        width: 20px; height: 10px }
               .sized { position: absolute; left: calc(anchor(--a right) + 5px); top: anchor(--a center);
                        width: anchor-size(--a width); height: 10px }
               .area { position: absolute; position-anchor: --a; position-area: top center; width: 40px; height: 10px }
               .flip { position: absolute; position-anchor: --a; position-area: top; height: 50px; width: 10px;
                       position-try-fallbacks: flip-block }
               .missing { position: absolute; left: anchor(--b left, 7px); top: 0; width: 5px; height: 5px }</style>
             <div class=box><div class=anchor></div><div class=below></div><div class=sized></div>
               <div class=area></div><div class=flip></div><div class=missing></div></div>",
        );
        let at = |class: &str| {
            let (x, y, fragment) = find(&pages[0], &document, class).expect(class);
            (x, y, fragment.rect.width.to_px())
        };
        assert_eq!(at("below"), (100.0, 70.0, 20.0));
        assert_eq!(at("sized"), (185.0, 55.0, 80.0));
        // Above the anchor, centered on it.
        assert_eq!(at("area"), (120.0, 30.0, 40.0));
        // 50px do not fit above (40px): flipped below, centered on the anchor.
        assert_eq!(at("flip"), (135.0, 70.0, 10.0));
        // Without the anchor, the fallback.
        assert_eq!(at("missing").0, 7.0);
    }

    #[test]
    fn tries_position_rules_in_order_and_hides_as_asked() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0; size: 400px 300px } body { margin: 0 }
               @position-try --below { position-area: bottom; width: 30px }
               .box { position: relative; width: 400px; height: 140px }
               .anchor { anchor-name: --a; position: absolute; left: 100px; top: 30px; width: 60px; height: 20px }
               .t { position: absolute; position-anchor: --a; position-area: top; width: 10px; height: 14px }
               .named { position-try-fallbacks: --below; height: 40px }
               .most { position-try-fallbacks: flip-block; position-try-order: most-height }
               .overflow { position-area: right; width: 300px; position-visibility: no-overflow }
               .clip { overflow: hidden; width: 20px; height: 20px }
               .hidden-anchor { anchor-name: --h; margin-left: 50px; width: 10px; height: 10px }
               .follows { position: absolute; position-anchor: --h; top: anchor(bottom); left: anchor(left);
                          width: 10px; height: 10px }
               .always { position-visibility: always }</style>
             <div class=box><div class=anchor></div><div class='t named'></div><div class='t most'></div>
               <div class='t overflow'></div><div class=clip><div class=hidden-anchor></div></div>
               <div class=follows></div><div class='follows always'></div></div>",
        );
        let at = |class: &str| {
            let (x, y, fragment) = find(&pages[0], &document, class).expect(class);
            (x, y, fragment.rect.width.to_px())
        };
        // 40px do not fit above: the rule puts the box below, 30px wide, centered.
        assert_eq!(at("named"), (115.0, 50.0, 30.0));
        // It fits above, but there is more room below.
        assert_eq!(at("most"), (125.0, 50.0, 10.0));
        let visible = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect(class);
            match &fragment.kind {
                FragmentKind::Box { style, .. } => {
                    style.visibility == fagus_style::values::Visibility::Visible
                }
                _ => true,
            }
        };
        // Too wide right of the anchor, and no option fits: hidden.
        assert!(!visible("overflow"));
        // The anchor is clipped away: the box that follows it is hidden, unless always shown.
        assert!(!visible("follows"));
        assert!(visible("always"));
    }

    #[test]
    fn finds_anchors_on_earlier_pages() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0; size: 400px 200px } body { margin: 0 }
               .anchor { anchor-name: --p; margin: 50px 0 0 100px; width: 60px; height: 20px }
               .filler { height: 300px }
               .t { position: absolute; position-anchor: --p; width: 30px; height: 10px }
               .beside { top: anchor(top); left: anchor(right) }
               .below { position-area: bottom }</style>
             <div class=anchor></div><div class=filler></div><div class='t beside'></div><div class='t below'></div>",
        );
        let first = |class: &str| {
            let (x, y, _) = find(&pages[0], &document, class).expect(class);
            (x, y)
        };
        // On the anchor's page, against it, though their placeholders are on the next.
        assert_eq!(first("beside"), (160.0, 50.0));
        assert_eq!(first("below"), (115.0, 70.0));
    }

    #[test]
    fn sizes_positioned_and_grid_boxes_and_heights_with_calc_size() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 16px/20px Inter }
               .rel { position: relative; width: 400px; height: 100px }
               .abs { position: absolute; left: 0; right: 100px; top: 0; width: calc-size(auto, size / 2); height: 5px }
               .grid { display: grid; grid-template-columns: 200px; width: 200px }
               .item { width: calc-size(auto, size * 0.25); height: 5px }
               .min { min-height: calc-size(auto, size + 30px) }
               .max { height: 100px; max-height: calc-size(fit-content, size + 10px) }
               .double { height: 10px; min-height: calc-size(min-content, size * 2) }</style>
             <div class=rel><div class=abs></div></div><div class=grid><div class=item></div></div>
             <div class=min></div><div class=max>Line</div><div class=double>Line</div>",
        );
        let size = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect(class);
            (fragment.rect.width.to_px(), fragment.rect.height.to_px())
        };
        // Half of the 300px between the insets, a quarter of the grid area.
        assert_eq!(size("abs").0, 150.0);
        assert_eq!(size("item").0, 50.0);
        // An `auto` minimum is nothing; content-based limits are the content's height.
        assert_eq!(size("min").1, 30.0);
        assert_eq!(size("max").1, 30.0);
        assert_eq!(size("double").1, 40.0);
    }

    #[test]
    fn sizes_boxes_with_calc_size() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 16px/20px Inter }
               .wrap { width: 400px } .half { width: calc-size(auto, size * 0.5); height: 5px }
               .fit { display: inline-block; width: calc-size(fit-content, size + 20px) }
               .tall { height: calc-size(auto, size * 2) }</style>
             <div class=wrap><div class=half></div><span class=fit>Text</span><div class=tall>Line</div></div>",
        );
        let width = |class: &str| {
            find(&pages[0], &document, class)
                .expect(class)
                .2
                .rect
                .width
                .to_px()
        };
        assert_eq!(width("half"), 200.0);
        let (_, _, fit) = find(&pages[0], &document, "fit").expect("fit");
        assert!(fit.rect.width.to_px() > 50.0 && fit.rect.width.to_px() < 80.0);
        let (_, _, tall) = find(&pages[0], &document, "tall").expect("tall");
        assert_eq!(tall.rect.height.to_px(), 40.0);
    }

    #[test]
    fn places_grid_items_in_named_areas() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 }
               .grid { display: grid; width: 300px; grid-template-columns: 100px [main-start] 1fr [main-end];
                       grid-template-rows: 20px 30px; grid-template-areas: 'logo head' 'side body' 'foot foot' }
               .head { grid-area: head } .side { grid-area: side } .foot { grid-area: foot; height: 10px }
               .main { grid-column: main; grid-row: 4 }</style>
             <div class=grid><div class=foot></div><div class=main></div><div class=side></div><div class=head></div></div>",
        );
        let position = |class: &str| {
            let (x, y, fragment) = find(&pages[0], &document, class).expect(class);
            (x, y, fragment.rect.width.to_px())
        };
        assert_eq!(position("head"), (100.0, 0.0, 200.0));
        assert_eq!(position("side"), (0.0, 20.0, 100.0));
        // The areas make a third row, explicit but sized as an implicit one.
        assert_eq!(position("foot"), (0.0, 50.0, 300.0));
        assert_eq!(position("main"), (100.0, 60.0, 200.0));
    }

    #[test]
    fn passes_tracks_down_to_subgrids() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 }
               .grid { display: grid; width: 300px; grid-template-columns: [a] auto [b] auto [c] 1fr;
                       column-gap: 10px; grid-template-rows: auto auto; row-gap: 4px }
               .sub { grid-column: 1 / -1; grid-row: 1 / 3; display: grid; grid-template-columns: subgrid;
                      grid-template-rows: subgrid; padding: 0 5px }
               .wide { width: 80px; height: 20px } .tall { height: 30px } .named { grid-column: b; height: 10px }</style>
             <div class=grid><div class=sub><div class=wide></div><div class=tall></div><div class=named></div></div></div>",
        );
        let position = |class: &str| {
            let (x, y, fragment) = find(&pages[0], &document, class).expect(class);
            (x, y, fragment.rect.width.to_px())
        };
        // The subgrid's items size the parent's columns: the first takes 80px and the subgrid's
        // padding; the second, with nothing wide in it, starts after the gap.
        assert_eq!(position("wide"), (5.0, 0.0, 80.0));
        let (x, _, _) = position("tall");
        assert_eq!(x, 95.0);
        // A parent's line name places an item of the subgrid; it goes into the second row.
        let (x, y, _) = position("named");
        assert_eq!((x, y), (95.0, 34.0));
    }

    #[test]
    fn wraps_lines_around_shapes_of_floats() {
        let (document, pages) = render_document(
            "<style>@page { size: 800px 600px; margin: 0 } body { font: 16px/20px Inter; margin: 0 }
               .text { width: 400px }
               .float { float: left; width: 100px; height: 100px; shape-outside: polygon(0 0, 100% 100%, 0 100%) }
             </style>
             <div class=text><div class=float></div>Eins zwei drei vier fünf sechs sieben acht neun zehn elf zwölf
             dreizehn vierzehn fünfzehn sechzehn siebzehn achtzehn neunzehn zwanzig</div>",
        );
        let (_, _, text) = find(&pages[0], &document, "text").expect("text");
        let mut lines = Vec::new();
        line_boxes(text, (0.0, 0.0), &mut lines);
        // Each line starts where the triangle's edge is at the bottom of the line.
        for &(x, y, _) in lines.iter().take(5) {
            assert_eq!(x, y + 20.0, "{lines:?}");
        }
    }

    #[test]
    fn balances_and_prettifies_lines_as_chrome_does() {
        let lines = |css: &str, text: &str| {
            let pages = render(&format!(
                "<style>@page {{ margin: 0 }} body {{ font: 16px/20px Inter; margin: 0 }}
                   p {{ width: 300px; margin: 0; {css} }}</style><p>{text}</p>"
            ));
            let mut found = Vec::new();
            line_texts(&pages[0].fragments[0], &mut found);
            found
                .iter()
                .map(|line| line.trim().to_owned())
                .collect::<Vec<_>>()
        };
        let short = "Sehr geehrte Damen und Herren wir erlauben";
        assert_eq!(
            lines("", short),
            ["Sehr geehrte Damen und Herren wir", "erlauben"]
        );
        // Pretty: no single word on a short last line.
        assert_eq!(
            lines("text-wrap: pretty", short),
            ["Sehr geehrte Damen und Herren", "wir erlauben"]
        );
        // Balanced: lines of about the same length.
        assert_eq!(
            lines(
                "text-wrap: balance",
                "Sehr geehrte Damen und Herren wir erlauben Ende"
            ),
            ["Sehr geehrte Damen und", "Herren wir erlauben Ende"]
        );
        assert_eq!(
            lines(
                "text-wrap: balance",
                "Sehr geehrte Damen und Herren wir erlauben uns Ihnen die folgenden Leistungen in Rechnung zu stellen die wir Ende"
            ),
            [
                "Sehr geehrte Damen und",
                "Herren wir erlauben uns Ihnen",
                "die folgenden Leistungen in",
                "Rechnung zu stellen die wir Ende"
            ]
        );
        // No wrapping at all.
        assert_eq!(lines("text-wrap: nowrap", short).len(), 1);
    }

    #[test]
    fn adjusts_font_sizes_to_aspect_values() {
        // Inter's x-height is 1118/2048 of its size: text keeping an x-height of 0.6 em is set
        // at 17.59px, and its line (`normal`) grows with it, one with a line height from a
        // number does not.
        let pages = render(
            "<style>@page { margin: 0 } body { font: 16px Inter; margin: 0 } p { margin: 0; font-size-adjust: 0.6 }</style>
             <p>Rechnung</p><p style='line-height: 1.5'>Rechnung</p><p style='font-size-adjust: none'>Rechnung</p>",
        );
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], &mut found);
        let expected = 16.0 * 0.6 / (1118.0 / 2048.0);
        assert!((found[0].size - expected).abs() < 0.01, "{}", found[0].size);
        assert_eq!(found[2].size, 16.0);
        let mut lines = Vec::new();
        line_boxes(&pages[0].fragments[0], (0.0, 0.0), &mut lines);
        let tops: Vec<f32> = lines.iter().map(|line| line.1).collect();
        assert_eq!(tops, [0.0, 21.0, 45.0]);
    }

    #[test]
    fn turns_ligatures_off_with_letter_spacing() {
        // Inter raises a hyphen between capitals (`calt`); letter spacing turns contextual (and
        // common) ligatures off and with them that, as in Chrome, unless `font-feature-settings`
        // turns them on again.
        let hyphen = |css: &str| {
            let pages = render(&format!("<p style='font: 16px Inter; {css}'>A-B</p>"));
            let mut found = Vec::new();
            runs(&pages[0].fragments[0], &mut found);
            found[0].glyphs[1].id
        };
        let raised = hyphen("");
        assert_ne!(raised, hyphen("letter-spacing: 1px"));
        assert_eq!(
            hyphen("letter-spacing: 1px"),
            hyphen("font-variant-ligatures: no-contextual")
        );
        assert_eq!(
            raised,
            hyphen("letter-spacing: 1px; font-feature-settings: \"calt\"")
        );
    }

    #[test]
    fn makes_room_for_fallback_faces_with_line_height_normal() {
        // A family whose digits come from Geist, which reaches higher and lower than Inter: at
        // 20px Inter's line is 19 + 5 pixels, Geist's 20 + 6.
        let mut fonts = test_fonts();
        for (file, unicode_range) in [
            ("Inter-Regular.ttf", vec![]),
            ("Geist-Regular.ttf", vec![(0x30, 0x39)]),
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
            let path = if file.starts_with("Geist") {
                path.join("corpus/quittio/fonts").join(file)
            } else {
                path.join("fonts").join(file)
            };
            let rule = fagus_text::Descriptors {
                family: "Mixed".to_owned(),
                weight: None,
                style: fagus_text::FontStyle::Normal,
                stretch: None,
                unicode_range,
                ..fagus_text::Descriptors::default()
            };
            let data = Arc::new(std::fs::read(path).expect("font"));
            fonts.add_document_face(Arc::new(
                fagus_text::Face::from_rule(data, 0, &rule).expect("face"),
            ));
        }
        let (_, pages) = render_document_in(
            "<!DOCTYPE html><style>
               @page { size: 400px 400px; margin: 0; }
               body { font: 20px Mixed; margin: 0; }
               p { margin: 0; }
             </style>
             <p>Rechnung</p><p>Rechnung 2026</p><p style='line-height: 30px'>Rechnung 2026</p>
             <p>Rechnung <span>2026</span></p>",
            &fonts,
            |_| Images::default(),
        );
        let mut lines = Vec::new();
        line_boxes(&pages[0].fragments[0], (0.0, 0.0), &mut lines);
        let tops: Vec<f32> = lines.iter().map(|line| line.1).collect();
        // The digits make their line two pixels taller, only with `line-height: normal`, also
        // inside an inline box.
        assert_eq!(tops, [0.0, 24.0, 50.0, 80.0]);
        assert_eq!(pages[0].fragments[0].rect.height.to_px(), 106.0);
    }

    #[test]
    fn flows_lines_around_floats() {
        let (document, pages) = render_document(
            "<style>
               @page { size: 800px 600px; margin: 0; }
               body { font: 16px/20px Inter; margin: 0; }
               .float { float: left; width: 100px; height: 50px; }
               .text { width: 400px; }
               .cleared { clear: left; }
               .context { overflow: hidden; height: 10px; }
             </style>
             <div class=text><div class=float></div>Eins zwei drei vier fünf sechs sieben acht neun zehn elf zwölf
             dreizehn vierzehn fünfzehn sechzehn siebzehn achtzehn neunzehn zwanzig</div>
             <div class=float></div><div class=context></div><p class=cleared>Unten</p>",
        );
        let page = &pages[0];
        let (_, _, text) = find(page, &document, "text").expect("text");
        let mut lines = Vec::new();
        line_boxes(text, (0.0, 0.0), &mut lines);
        // Lines beside the 50px float (two and a half lines) start after it, the rest at 0.
        assert!(lines.len() >= 4, "{lines:?}");
        for &(x, y, width) in &lines {
            let beside = y < 50.0;
            assert_eq!(x, if beside { 100.0 } else { 0.0 }, "{lines:?}");
            assert_eq!(width, if beside { 300.0 } else { 400.0 }, "{lines:?}");
        }
        // A new formatting context goes beside the second float; `clear` moves the paragraph
        // below it.
        let (x, _, context) = find(page, &document, "context").expect("context");
        assert_eq!((x, context.rect.width.to_px()), (100.0, 700.0));
        let (_, y, _) = find(page, &document, "cleared").expect("cleared");
        let (_, float_y, _) = pages[0]
            .fragments
            .iter()
            .find_map(|fragment| find_floats(fragment, (0.0, 0.0)).into_iter().nth(1))
            .expect("second float");
        assert_eq!(y, float_y + 50.0);
    }

    /// The page positions of the floats below `fragment`.
    fn find_floats(fragment: &Fragment, origin: (f32, f32)) -> Vec<(f32, f32, &Fragment)> {
        let x = origin.0 + fragment.rect.x.to_px();
        let y = origin.1 + fragment.rect.y.to_px();
        let mut found = Vec::new();
        if let FragmentKind::Box { style, .. } = &fragment.kind
            && style.is_floating()
        {
            found.push((x, y, fragment));
        }
        for child in &fragment.children {
            found.extend(find_floats(child, (x, y)));
        }
        found
    }

    #[test]
    fn sizes_shrink_to_fit_boxes_around_floats() {
        let (document, pages) = render_document(
            "<style>
               body { font: 16px/20px Inter; margin: 0; }
               .cell { display: inline-block; }
               .float { float: left; width: 120px; height: 20px; }
             </style>
             <div class=cell><div class=float></div><div class=float></div></div>",
        );
        let (_, _, cell) = find(&pages[0], &document, "cell").expect("cell");
        assert_eq!(cell.rect.width.to_px(), 240.0, "two floats side by side");
        let floats = find_floats(cell, (0.0, 0.0));
        assert_eq!(
            floats.iter().map(|float| float.0).collect::<Vec<_>>(),
            [0.0, 120.0]
        );
    }

    #[test]
    fn contains_floats_in_formatting_contexts() {
        let (document, pages) = render_document(
            "<style>
               body { font: 16px/20px Inter; margin: 0; }
               .float { float: right; width: 50px; height: 80px; }
             </style>
             <div class=plain><div class=float></div>a</div>
             <div class=root style='display: flow-root'><div class=float></div>b</div>",
        );
        let (_, _, plain) = find(&pages[0], &document, "plain").expect("plain");
        let (_, _, root) = find(&pages[0], &document, "root").expect("root");
        assert_eq!(
            plain.rect.height.to_px(),
            20.0,
            "floats overflow a box in the same context"
        );
        assert_eq!(
            root.rect.height.to_px(),
            80.0,
            "a new formatting context contains them"
        );
    }

    #[test]
    fn empty_clearing_boxes_keep_the_margins_above_them() {
        // An empty `clear: both` box after a block whose bottom margin already passes the float
        // still has clearance in Chrome: the margin above it no longer collapses with the
        // heading's below it. Its own margins collapse with those below, starting from its top
        // margin's edge. A float that ends within the margins above it from the context's top
        // leaves the margins collapsing through. The numbers are Chrome's.
        let (document, pages) = render_document(
            "<style>
               @page { size: 600px 1200px; margin: 0 }
               body { font: 16px/20px Inter; margin: 0; }
               .case { display: flow-root; height: 200px; }
               .float { float: left; width: 100px; }
               .p { height: 60px; margin: 0 0 16px; }
               .clear { clear: both; }
               h2 { font-size: 16px; margin: 12px 0 0; }
             </style>
             <div class=case><div class=float style='height: 50px'></div><div class=p></div>
               <div class=clear></div><h2 class=a>a</h2></div>
             <div class=case><div class=float style='height: 77px'></div><div class=p></div>
               <div class=clear></div><h2 class=b>b</h2></div>
             <div class=case><div class=float style='height: 50px'></div><div class=p></div>
               <div class=clear style='margin: 2px 0 10px'></div><h2 class=c>c</h2></div>
             <div class=case><div class=float style='height: 50px'></div><div class=p></div>
               <div class=clear style='margin: 0 0 20px'></div><h2 class=d>d</h2></div>
             <div class=case><div class=float style='height: 10px'></div><div class=p></div>
               <div class=clear></div><h2 class=e>e</h2></div>",
        );
        let y = |class: &str, case: f32| find(&pages[0], &document, class).expect(class).1 - case * 200.0;
        assert_eq!(y("a", 0.0), 88.0);
        assert_eq!(y("b", 1.0), 89.0);
        assert_eq!(y("c", 2.0), 86.0);
        assert_eq!(y("d", 3.0), 96.0);
        assert_eq!(y("e", 4.0), 76.0);
    }

    #[test]
    fn moves_floats_that_cannot_break_and_goes_on_beside_them() {
        let pages = render(
            "<style>
               @page { size: 400px 300px; margin: 0; }
               body { font: 16px/20px Inter; margin: 0; }
               .spacer { height: 240px; }
               .float { float: left; width: 100px; height: 100px; break-inside: avoid; }
               .clear { clear: left; height: 10px; }
             </style>
             <div class=spacer></div><p style='margin: 0'><span class=float></span>Wort Wort</p>
             <div class=clear></div>",
        );
        assert_eq!(pages.len(), 2);
        // The paragraph stays on the first page, at the full width; the float starts the next,
        // and the box that clears it goes below it there.
        assert!(find_floats(&pages[0].fragments[0], (0.0, 0.0)).is_empty());
        let mut lines = Vec::new();
        line_boxes(&pages[0].fragments[0], (0.0, 0.0), &mut lines);
        assert_eq!((lines[0].0, lines[0].1), (0.0, 240.0));
        let floats = find_floats(&pages[1].fragments[0], (0.0, 0.0));
        assert_eq!((floats[0].0, floats[0].1), (0.0, 0.0));
        let mut boxes = Vec::new();
        collect_boxes(&pages[1].fragments[0], (0.0, 0.0), &mut boxes);
        assert!(boxes.contains(&(100.0, 10.0)), "{boxes:?}");
    }

    /// The bottom edge and height of the boxes of 10px height below `fragment`.
    fn collect_boxes(fragment: &Fragment, origin: (f32, f32), out: &mut Vec<(f32, f32)>) {
        let y = origin.1 + fragment.rect.y.to_px();
        if matches!(fragment.kind, FragmentKind::Box { .. })
            && fragment.rect.height == units::Au::from_px(10.0)
        {
            out.push((y, 10.0));
        }
        for child in &fragment.children {
            collect_boxes(child, (origin.0 + fragment.rect.x.to_px(), y), out);
        }
    }

    #[test]
    fn breaks_floats_and_continues_paragraphs_on_the_next_page() {
        let words = "Wort ".repeat(200);
        let pages = render(&format!(
            "<style>
               @page {{ size: 400px 300px; margin: 0; }}
               body {{ font: 16px/20px Inter; margin: 0; }}
               .spacer {{ height: 240px; }}
               .float {{ float: left; width: 100px; height: 100px; background: red; }}
             </style>
             <div class=spacer></div><p><span class=float></span>{words}</p>"
        ));
        assert!(pages.len() >= 3);
        // The float does not fit below the spacer: it breaks, and its rest starts the second
        // page, beside the paragraph's lines there.
        let floats = find_floats(&pages[0].fragments[0], (0.0, 0.0));
        assert_eq!(floats.len(), 1);
        assert_eq!((floats[0].1, floats[0].2.rect.height.to_px()), (256.0, 44.0));
        let floats = find_floats(&pages[1].fragments[0], (0.0, 0.0));
        assert_eq!(floats.len(), 1);
        assert_eq!((floats[0].0, floats[0].1), (0.0, 0.0));
        assert_eq!(floats[0].2.rect.height.to_px(), 56.0);
        let mut lines = Vec::new();
        line_boxes(&pages[1].fragments[0], (0.0, 0.0), &mut lines);
        assert_eq!((lines[0].0, lines[0].1), (100.0, 0.0), "{lines:?}");
        // Lines on the next page are broken again for the full width.
        let mut lines = Vec::new();
        line_boxes(&pages[2].fragments[0], (0.0, 0.0), &mut lines);
        assert!(
            lines
                .iter()
                .all(|&(x, _, width)| x.abs() < 0.01 && (width - 400.0).abs() < 0.01),
            "{lines:?}"
        );
    }

    #[test]
    fn wraps_flex_items_into_lines() {
        let (document, pages) = render_document(
            "<style>
               @page { size: 400px 600px; margin: 0; }
               body { margin: 0; font: 16px/20px Inter; }
               .row { display: flex; flex-wrap: wrap; gap: 10px 20px; }
               .row div { width: 100px; height: 30px; }
               .reverse { display: flex; flex-direction: row-reverse; }
               .reverse div { width: 50px; height: 10px; }
             </style>
             <div class=row><div class=a></div><div></div><div></div><div class=d></div><div></div></div>
             <div class=reverse><div class=first></div><div></div></div>",
        );
        let page = &pages[0];
        let (x, y, _) = find(page, &document, "a").expect("a");
        assert_eq!((x, y), (0.0, 0.0));
        // Three items of 100px with gaps of 20px fit in 400px; the fourth starts the second line.
        let (x, y, _) = find(page, &document, "d").expect("d");
        assert_eq!((x, y), (0.0, 40.0));
        let (_, _, row) = find(page, &document, "row").expect("row");
        assert_eq!(row.rect.height.to_px(), 70.0);
        // row-reverse starts at the right.
        let (x, _, _) = find(page, &document, "first").expect("first");
        assert_eq!(x, 350.0);
    }

    #[test]
    fn grows_column_items_into_the_minimum_height() {
        let (document, pages) = render_document(
            "<style>
               body { margin: 0; font: 16px/20px Inter; }
               .card { display: flex; flex-direction: column; min-height: 200px; }
               main { flex: 1; }
             </style>
             <div class=card><main>Inhalt</main><footer class=footer>Fuß</footer></div>",
        );
        let (_, card_y, card) = find(&pages[0], &document, "card").expect("card");
        let (_, footer_y, footer) = find(&pages[0], &document, "footer").expect("footer");
        assert_eq!(card.rect.height.to_px(), 200.0);
        assert_eq!(
            footer_y + footer.rect.height.to_px(),
            card_y + 200.0,
            "the footer sits at the bottom"
        );
    }

    #[test]
    fn breaks_flex_containers_across_pages() {
        let cards = "<div class=card></div>".repeat(9);
        let sections = "<section>Abschnitt</section>".repeat(8);
        let (document, pages) = render_document(&format!(
            "<style>
               @page {{ size: 300px 400px; margin: 0; }}
               body {{ margin: 0; font: 16px/20px Inter; }}
               .page {{ display: flex; flex-direction: column; gap: 10px; }}
               .grid {{ display: flex; flex-wrap: wrap; }}
               .card {{ width: 100px; height: 100px; }}
               section {{ height: 60px; }}
             </style>
             <div class=page><div class=grid>{cards}</div>{sections}</div>"
        ));
        // Three lines of cards (300px), then sections of 60px with 10px gaps: 5 + 5 + 1 sections
        // on three pages, a section broken like in browsers where a page ends inside it.
        assert_eq!(pages.len(), 3);
        let (_, _, grid) = find(&pages[0], &document, "grid").expect("grid");
        assert_eq!(grid.rect.height.to_px(), 300.0);
        let mut heights = Vec::new();
        for page in &pages {
            let mut boxes = Vec::new();
            for fragment in &page.fragments {
                collect_sections(fragment, &document, (0.0, 0.0), &mut boxes);
            }
            for &(y, height) in &boxes {
                assert!(y + height <= 400.0, "inside the page: {boxes:?}");
            }
            heights.push(boxes.iter().map(|(_, height)| height).sum::<f32>());
        }
        assert_eq!(heights.iter().sum::<f32>(), 8.0 * 60.0, "{heights:?}");
    }

    /// `count` lines of text named `name` and numbered, one per line.
    fn numbered(name: &str, count: usize) -> String {
        use std::fmt::Write;
        (1..=count).fold(String::new(), |mut out, n| {
            write!(out, "{name}{n}<br>").expect("written");
            out
        })
    }

    /// The text of the lines in the fragment of the element with class `class` on `page`.
    fn texts_of(page: &Page, document: &Document, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((_, _, fragment)) = find(page, document, class) {
            line_texts(fragment, &mut out);
        }
        out
    }

    #[test]
    fn breaks_flex_items_across_pages() {
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
               .row {{ display: flex }} .row > div {{ flex: 1 }}</style>
             <div style='height: 150px'></div>
             <div class=row><div class=a>{}</div><div class=b>{}</div></div>",
            numbered("a", 12),
            numbered("b", 3)
        ));
        // The line does not fit: its items break inside, as in Chrome, rather than moving on.
        assert_eq!(pages.len(), 2);
        assert_eq!(texts_of(&pages[0], &document, "a").len(), 7);
        let (_, _, row) = find(&pages[0], &document, "row").expect("row");
        assert_eq!(row.rect.height.to_px(), 150.0);
        // The short item is stretched to the line, to the end of the page and on.
        let (_, _, b) = find(&pages[0], &document, "b").expect("b");
        assert_eq!(b.rect.height.to_px(), 150.0);
        assert_eq!(
            texts_of(&pages[1], &document, "a"),
            ["a8", "a9", "a10", "a11", "a12"]
        );
        let (_, by, b) = find(&pages[1], &document, "b").expect("rest of b");
        assert_eq!((by, b.rect.height.to_px()), (0.0, 100.0));
        assert_eq!(texts_of(&pages[1], &document, "b").len(), 0);
    }

    #[test]
    fn breaks_grid_rows_and_moves_rows_that_cannot_break() {
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
               .grid {{ display: grid; grid-template-columns: 1fr 1fr }}</style>
             <div style='height: 200px'></div>
             <div class=grid><div>x</div><div>y</div><div class=c>{}</div><div>z</div>
             <div class=d style='break-inside: avoid'>{}</div><div>w</div></div>",
            numbered("c", 6),
            numbered("d", 3)
        ));
        // The second row breaks inside its item; the third avoids a break and moves on.
        assert_eq!(texts_of(&pages[0], &document, "c"), ["c1", "c2", "c3", "c4"]);
        assert_eq!(texts_of(&pages[1], &document, "c"), ["c5", "c6"]);
        let (_, y, _) = find(&pages[1], &document, "d").expect("d");
        assert_eq!(y, 40.0);
        let (document, pages) = render_document(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0; font: 16px/20px Inter }
               .grid { display: grid; grid-template-columns: 1fr 1fr }</style>
             <div style='height: 250px'></div>
             <div class=grid><div>x</div><div>y</div><div class=e style='break-before: page'>e</div></div>",
        );
        assert_eq!(pages.len(), 2);
        assert!(find(&pages[1], &document, "e").is_some());
    }

    #[test]
    fn breaks_table_rows_inside_their_cells() {
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
               table {{ border-spacing: 0 }} td {{ padding: 0; vertical-align: top }}</style>
             <div style='height: 200px'></div>
             <table><tr><td>x</td></tr><tr><td class=c>{}</td><td class=d>d1<br>d2</td></tr>
             <tr><td rowspan=2 class=m>{}</td><td class=n>n</td></tr><tr><td class=p>p</td></tr></table>",
            numbered("c", 6),
            numbered("m", 14)
        ));
        // A row breaks where its cells break, even one that would fit on the next page.
        assert_eq!(texts_of(&pages[0], &document, "c"), ["c1", "c2", "c3", "c4"]);
        assert_eq!(texts_of(&pages[0], &document, "d"), ["d1", "d2"]);
        assert_eq!(texts_of(&pages[1], &document, "c"), ["c5", "c6"]);
        // A cell spanning rows breaks too, and the rows it spans grow alike, as in Chrome.
        assert_eq!(texts_of(&pages[1], &document, "m").len(), 12);
        assert_eq!(texts_of(&pages[2], &document, "m"), ["m13", "m14"]);
        let (_, _, n) = find(&pages[1], &document, "n").expect("n");
        assert_eq!(n.rect.height.to_px(), 140.0);
    }

    #[test]
    fn breaks_floats_that_are_containers() {
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
               .f {{ float: left; width: 150px }} table {{ border-spacing: 0 }} td {{ padding: 0 }}</style>
             <div style='height: 200px'></div>
             <div class='f flex' style='display: flex'><div>{}</div></div>
             <table class='f table'><tr><td>{}</td></tr></table>",
            numbered("a", 8),
            numbered("b", 3)
        ));
        let (_, _, flex) = find(&pages[0], &document, "flex").expect("flex");
        assert_eq!(flex.rect.height.to_px(), 100.0);
        assert_eq!(texts_of(&pages[1], &document, "flex"), ["a6", "a7", "a8"]);
        let (_, _, table) = find(&pages[0], &document, "table").expect("table");
        assert_eq!(table.rect.height.to_px(), 60.0);
    }

    #[test]
    fn breaks_column_flex_containers_with_a_height() {
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}</style>
             <div style='height: 200px'></div>
             <div class=col style='display: flex; flex-direction: column; height: 300px'>
             <div class=a>{}</div><div class=b>{}</div><div class=c style='margin-top: 40px'>c1</div></div>",
            numbered("a", 4),
            numbered("b", 3)
        ));
        // The column breaks inside: its second item does not fit and starts the next page, and
        // the container's height goes on there, as in Chrome.
        assert_eq!(pages.len(), 2);
        let (_, _, column) = find(&pages[0], &document, "col").expect("column");
        assert_eq!(column.rect.height.to_px(), 100.0);
        assert!(find(&pages[0], &document, "b").is_none());
        let (_, y, _) = find(&pages[1], &document, "b").expect("b");
        assert_eq!(y, 0.0);
        assert_eq!(texts_of(&pages[1], &document, "b").len(), 3);
        // The items below move along, their margins kept.
        let (_, y, _) = find(&pages[1], &document, "c").expect("c");
        assert_eq!(y, 100.0);
        let (_, _, column) = find(&pages[1], &document, "col").expect("rest of the column");
        assert_eq!(column.rect.height.to_px(), 200.0);
    }

    #[test]
    fn justifies_flex_items_left_and_right() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 16px/20px Inter }
               .f { display: flex; width: 300px; height: 100px }
               .f > div { width: 50px; height: 20px }</style>
             <div class=f dir=rtl style='justify-content: left'><div class=a></div></div>
             <div class=f style='flex-direction: row-reverse; justify-content: right'><div class=b></div></div>
             <div class=f style='flex-direction: column; justify-content: right'><div class=c></div></div>",
        );
        // By the sides of rows, whatever their direction; at the start of a column.
        let x = |class: &str| find(&pages[0], &document, class).expect("item").0;
        assert_eq!((x("a"), x("b")), (0.0, 250.0));
        let (_, y, _) = find(&pages[0], &document, "c").expect("c");
        assert_eq!(y, 200.0);
    }

    #[test]
    fn places_fixed_boxes_at_their_static_positions() {
        let (document, pages) = render_document(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0; font: 16px/20px Inter }
               .fixed { position: fixed }</style>
             <div style='height: 400px'></div>
             <div style='margin-left: 50px'><div class='fixed static'>s</div></div>
             <div class='fixed top' style='top: 10px'>t</div>
             <div style='height: 400px'></div>",
        );
        assert_eq!(pages.len(), 3);
        // From the page its placeholder is on, where the placeholder is, as in Chrome.
        assert!(find(&pages[0], &document, "static").is_none());
        for page in &pages[1..] {
            let (x, y, _) = find(page, &document, "static").expect("static");
            assert_eq!((x, y), (50.0, 100.0));
        }
        // On every page with a vertical inset.
        for page in &pages {
            let (x, y, _) = find(page, &document, "top").expect("top");
            assert_eq!((x, y), (0.0, 10.0));
        }
    }

    #[test]
    fn grows_rows_after_cells_broken_in_their_padding() {
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
               table {{ border-spacing: 0 }} td {{ padding: 0 0 4px; vertical-align: top }}</style>
             <table><tr><td>{}</td><td>b</td></tr><tr><td class=next>c</td></tr></table>",
            numbered("a", 30)
        ));
        // The text ends with the second page; the padding of both cells goes on, one below the
        // other, as in Chrome.
        let (_, y, _) = find(&pages[2], &document, "next").expect("next row");
        assert_eq!(y, 8.0);
    }

    #[test]
    fn leaves_the_first_part_of_a_grid_empty_before_a_forced_break() {
        let (document, pages) = render_document(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0; font: 16px/20px Inter }</style>
             <div class=grid style='display: grid; padding-top: 4px'><div class=a style='break-before: page'>a</div></div>",
        );
        // The padding separates the row from the top of the grid: the break falls inside the
        // grid, whose first part is empty, as in Chrome.
        assert_eq!(pages.len(), 2);
        let (_, _, grid) = find(&pages[0], &document, "grid").expect("grid");
        assert_eq!(grid.rect.height.to_px(), 300.0);
        let (_, y, _) = find(&pages[1], &document, "a").expect("a");
        assert_eq!(y, 0.0);
    }

    #[test]
    fn breaks_lines_anywhere() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 16px/20px Inter }</style>
             <div class=d style='width: 30px; line-break: anywhere'>abcdefgh</div>",
        );
        assert!(texts_of(&pages[0], &document, "d").len() > 2);
    }

    #[test]
    fn shapes_each_part_of_a_broken_float() {
        let text = "lorem ipsum dolor sit amet ".repeat(30);
        let pages = render(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}</style>
             <div style='height: 200px'></div>
             <div><div style='float: left; width: 150px; height: 300px; shape-outside: polygon(0 0, 100% 0, 0 100%)'></div>{text}</div>"
        ));
        // The part on the next page is a triangle of its own: its second line starts left of
        // the float's full width.
        let mut starts = Vec::new();
        line_starts(&pages[1].fragments[0], 0.0, &mut starts);
        assert!(starts.len() > 2);
        assert!(starts[1] < 150.0, "{starts:?}");
    }

    /// The left edges of the lines in a fragment.
    fn line_starts(fragment: &Fragment, origin: f32, out: &mut Vec<f32>) {
        let x = origin + fragment.rect.x.to_px();
        if matches!(fragment.kind, FragmentKind::Line { .. }) {
            let first = fragment
                .children
                .iter()
                .map(|child| child.rect.x.to_px())
                .fold(f32::INFINITY, f32::min);
            out.push(x + first);
            return;
        }
        for child in &fragment.children {
            line_starts(child, x, out);
        }
    }

    #[test]
    fn places_absolute_boxes_by_their_containing_block() {
        let (document, pages) = render_document(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0; font: 16px/20px Inter }
               div { position: relative } .a { position: absolute }</style>
             <div>one two <div class='a block'>x</div><span class='a inline'>y</span> three</div>
             <div dir=rtl style='width: 300px; height: 40px'><div class='a ltr' dir=ltr style='width: 50px'>z</div></div>
             <div dir=rtl style='width: 300px'><div class=float dir=ltr style='float: inline-start; width: 40px; height: 10px'></div></div>",
        );
        // A box that was block-level starts on the next line, one that was inline stays on it.
        let (bx, by, _) = find(&pages[0], &document, "block").expect("block");
        assert_eq!((bx, by), (0.0, 20.0));
        let (ix, iy, _) = find(&pages[0], &document, "inline").expect("inline");
        assert!(ix > 0.0 && iy == 0.0);
        // In a right-to-left block, boxes start at its right edge, whatever their own direction.
        let (lx, _, ltr) = find(&pages[0], &document, "ltr").expect("ltr");
        assert_eq!(lx + ltr.rect.width.to_px(), 300.0);
        let (fx, _, _) = find(&pages[0], &document, "float").expect("float");
        assert_eq!(fx, 260.0);
    }

    #[test]
    fn justifies_reversed_rows_by_the_inline_axis() {
        let (document, pages) = render_document(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0; font: 16px/20px Inter }
               .row { display: flex; flex-direction: row-reverse; width: 400px } .row > div { width: 50px }</style>
             <div class=row style='justify-content: start'><div class=s></div></div>
             <div class=row style='justify-content: flex-start'><div class=fs></div></div>",
        );
        // `start` is the inline start, on the left; `flex-start` the main start, on the right.
        let (sx, _, _) = find(&pages[0], &document, "s").expect("start");
        let (fx, _, _) = find(&pages[0], &document, "fs").expect("flex-start");
        assert_eq!((sx, fx), (0.0, 350.0));
    }

    fn collect_sections(
        fragment: &Fragment,
        document: &Document,
        origin: (f32, f32),
        out: &mut Vec<(f32, f32)>,
    ) {
        let x = origin.0 + fragment.rect.x.to_px();
        let y = origin.1 + fragment.rect.y.to_px();
        if let FragmentKind::Box { node: Some(node), .. } = &fragment.kind
            && document.element(*node).is_some_and(|data| data.is("section"))
        {
            out.push((y, fragment.rect.height.to_px()));
        }
        for child in &fragment.children {
            collect_sections(child, document, (x, y), out);
        }
    }

    #[test]
    fn places_grid_items_and_breaks_between_rows() {
        let rows = "<div></div>".repeat(12);
        let (document, pages) = render_document(&format!(
            "<style>
               @page {{ size: 300px 200px; margin: 0; }}
               body {{ margin: 0; font: 16px/20px Inter; }}
               .grid {{ display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 0 30px; }}
               .grid div {{ height: 40px; }}
               .wide {{ grid-column: 2 / -1; }}
               .tall {{ grid-row: span 2; }}
             </style>
             <div class=grid><div class=tall></div><div class=wide></div><div class=a></div>{rows}</div>"
        ));
        let page = &pages[0];
        // Columns of (300 - 60) / 3 = 80px; the wide item takes columns 2 and 3.
        let (x, y, wide) = find(page, &document, "wide").expect("wide");
        assert_eq!((x, y, wide.rect.width.to_px()), (110.0, 0.0, 190.0));
        // The next item goes beside the tall one in the second row.
        let (x, y, _) = find(page, &document, "a").expect("a");
        assert_eq!((x, y), (110.0, 40.0));
        // Six rows of 40px: five fit on the first page (the first two stay together, as the tall
        // item spans them), the last moves on.
        assert_eq!(pages.len(), 2);
        let (_, _, grid) = find(page, &document, "grid").expect("grid");
        assert_eq!(grid.rect.height.to_px(), 200.0);
        let (_, _, rest) = find(&pages[1], &document, "grid").expect("rest");
        assert_eq!(rest.rect.height.to_px(), 40.0);
    }

    #[test]
    fn distributes_free_space_between_grid_tracks() {
        // `auto` tracks stretch into the free space only when `justify-content` and
        // `align-content` do not distribute it themselves (CSS Grid 2, §11.8).
        let cases = [
            ("normal", "normal", [0.0, 100.0, 200.0], 0.0),
            ("space-between", "end", [0.0, 125.0, 250.0], 80.0),
            ("center", "center", [75.0, 125.0, 175.0], 40.0),
            ("space-evenly", "space-around", [37.5, 125.0, 212.5], 40.0),
        ];
        for (justify, align, xs, row_y) in cases {
            let (document, pages) = render_document(&format!(
                "<style>@page {{ size: 300px 200px; margin: 0 }} body {{ margin: 0 }}
                   .grid {{ display: grid; grid-template-columns: repeat(3, auto); height: 100px;
                            justify-content: {justify}; align-content: {align} }}
                   .grid div {{ width: 50px; height: 20px }}</style>
                 <div class=grid><div class=a></div><div class=b></div><div class=c></div></div>"
            ));
            for (class, x) in ["a", "b", "c"].into_iter().zip(xs) {
                let (found_x, y, _) = find(&pages[0], &document, class).expect("item");
                assert_eq!((found_x, y), (x, row_y), "{justify} {align}: {class}");
            }
        }
    }

    /// The x of the glyph for `letter` below `fragment`, relative to it.
    fn glyph_x(fragment: &Fragment, letter: char) -> Option<f32> {
        fragment.children.iter().find_map(|child| match &child.kind {
            FragmentKind::Text(run) => {
                let at = run.text.find(letter)? as u32;
                let before: f32 = run
                    .glyphs
                    .iter()
                    .take_while(|glyph| glyph.cluster.0 < at)
                    .map(|glyph| glyph.advance)
                    .sum();
                Some(child.rect.x.to_px() + before)
            }
            _ => glyph_x(child, letter).map(|x| x + child.rect.x.to_px()),
        })
    }

    /// The x of the first text run below `fragment`, relative to it.
    fn first_text_x(fragment: &Fragment) -> Option<f32> {
        fragment.children.iter().find_map(|child| match &child.kind {
            FragmentKind::Text(_) => Some(child.rect.x.to_px()),
            _ => first_text_x(child).map(|x| x + child.rect.x.to_px()),
        })
    }

    /// The text of each line box below `fragment`, in order.
    fn line_texts(fragment: &Fragment, out: &mut Vec<String>) {
        if matches!(fragment.kind, FragmentKind::Line { .. }) {
            fn text(fragment: &Fragment, out: &mut String) {
                if let FragmentKind::Text(run) = &fragment.kind {
                    out.push_str(&run.text);
                }
                for child in &fragment.children {
                    text(child, out);
                }
            }
            let mut line = String::new();
            text(fragment, &mut line);
            out.push(line.trim().to_owned());
            return;
        }
        for child in &fragment.children {
            line_texts(child, out);
        }
    }

    #[test]
    fn hyphenates_words() {
        let pages = render(
            "<html lang=de><style>body { font: 16px/20px Inter; margin: 0 } p { width: 110px }
               .auto { hyphens: auto } .none { hyphens: none }</style>
             <p class=auto>Rechnungsnummer</p>
             <p>Umsatz&shy;steuer&shy;nummer</p>
             <p class=none>Umsatz&shy;steuer&shy;nummer</p></html>",
        );
        let mut lines = Vec::new();
        line_texts(&pages[0].fragments[0], &mut lines);
        // "steuernummer" shaped as part of the whole word, kerned across the soft hyphen as in
        // Chrome, is a quarter pixel too wide for the line.
        assert_eq!(
            lines,
            [
                "Rechnungs\u{AD}",
                "nummer",
                "Umsatz\u{AD}",
                "steuer\u{AD}",
                "nummer",
                "Umsatzsteuernummer"
            ]
        );
    }

    #[test]
    fn hyphenates_the_last_word_only_where_it_starts_a_line() {
        // As in Chrome (Blink's `ShouldHyphenate`): "Rech-" would fit after "der", but the last
        // word of a paragraph breaks only where it starts its line.
        let pages = render(
            "<html lang=de><style>body { font: 16px/20px Inter; margin: 0 } p { width: 110px; hyphens: auto }</style>
             <p>Das Datum der Rechnungsstellung.</p><p>Die Rechnungsnummer ist hier</p></html>",
        );
        let mut lines = Vec::new();
        line_texts(&pages[0].fragments[0], &mut lines);
        assert_eq!(
            lines,
            [
                "Das Datum",
                "der",
                "Rechnungs\u{AD}",
                "stellung.",
                "Die Rech\u{AD}",
                "nungsnummer",
                "ist hier"
            ]
        );
    }

    #[test]
    fn shows_the_hyphenate_character_at_hyphenation_points() {
        fn hyphens(fragment: &Fragment, out: &mut Vec<usize>) {
            if let FragmentKind::Text(run) = &fragment.kind
                && let Some(word) = run.text.strip_suffix('\u{AD}')
            {
                out.push(run.glyphs.len() - word.chars().count());
            }
            fragment.children.iter().for_each(|child| hyphens(child, out));
        }
        let pages = render(
            "<html lang=de><style>body { font: 16px/20px Inter; margin: 0 } p { width: 110px; hyphens: auto }
               .two { hyphenate-character: '--' } .auto { hyphenate-character: auto }</style>
             <div class=two><p>Rechnungsnummer</p><p class=auto>Rechnungsnummer</p></div></html>",
        );
        // It stands for a soft hyphen in the text, and shows as the characters it says.
        let mut found = Vec::new();
        hyphens(&pages[0].fragments[0], &mut found);
        assert_eq!(found, [2, 1]);
    }

    #[test]
    fn hyphenates_with_the_hyphen_minus_where_the_font_has_no_hyphen() {
        // Geist has no U+2010: Chrome then draws U+002D, which is narrower than a missing glyph.
        let mut fonts = test_fonts();
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/quittio/fonts/Geist-Regular.ttf");
        let rule = fagus_text::Descriptors {
            family: "Geist".to_owned(),
            weight: None,
            style: fagus_text::FontStyle::Normal,
            stretch: None,
            unicode_range: vec![],
            ..fagus_text::Descriptors::default()
        };
        let data = Arc::new(std::fs::read(path).expect("font"));
        fonts.add_document_face(Arc::new(
            fagus_text::Face::from_rule(data, 0, &rule).expect("face"),
        ));
        let (_, pages) = render_document_in(
            "<html lang=de><style>body { font: 16px/20px Geist; margin: 0 } p { width: 110px; hyphens: auto }</style>
             <p>Rechnungsnummer</p></html>",
            &fonts,
            |_| Images::default(),
        );
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], &mut found);
        let hyphenated = found
            .iter()
            .find(|run| run.text.ends_with('\u{AD}'))
            .expect("a hyphen");
        let face = fonts.face(hyphenated.face);
        assert_eq!(hyphenated.glyphs.last().map(|glyph| glyph.id), face.glyph('-'));
    }

    #[test]
    fn sets_hyphenated_words_whole_and_shapes_them_again_where_they_break() {
        /// The text of each line and the width of its glyphs, without a hyphen at its end.
        fn lines(fragment: &Fragment, out: &mut Vec<(String, f32)>) {
            if matches!(fragment.kind, FragmentKind::Line { .. }) {
                let mut found = Vec::new();
                runs(fragment, &mut found);
                let text: String = found.iter().map(|run| run.text.as_str()).collect();
                let mut glyphs: Vec<&Glyph> = found.iter().flat_map(|run| &run.glyphs).collect();
                if text.ends_with('\u{AD}') {
                    glyphs.pop();
                }
                let width = glyphs.iter().map(|glyph| glyph.advance).sum();
                out.push((text.trim_end_matches('\u{AD}').to_owned(), width));
                return;
            }
            for child in &fragment.children {
                lines(child, out);
            }
        }
        // Geist joins "tt" and "ff" in ligatures, and kerns "e" before "fi".
        let mut fonts = test_fonts();
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/quittio/fonts/Geist-Regular.ttf");
        let rule = fagus_text::Descriptors {
            family: "Geist".to_owned(),
            weight: None,
            style: fagus_text::FontStyle::Normal,
            stretch: None,
            unicode_range: vec![],
            ..fagus_text::Descriptors::default()
        };
        let data = Arc::new(std::fs::read(path).expect("font"));
        let face = fonts.add_document_face(Arc::new(
            fagus_text::Face::from_rule(data, 0, &rule).expect("face"),
        ));
        let (_, pages) = render_document_in(
            "<html lang=de><style>body { font: 20px/20px Geist; margin: 0 } p { hyphens: auto; margin: 0 }
               .narrow { width: 1px }</style>
             <p>bitte</p><p class=narrow>Kaffeefilter</p></html>",
            &fonts,
            |_| Images::default(),
        );
        let mut found = Vec::new();
        lines(&pages[0].fragments[0], &mut found);
        let shaped = |text: &str| fonts.shape(face, text).advance * fonts.face(face).scale(20.0);
        let texts: Vec<&str> = found.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(texts, ["bitte", "Kaf", "fee", "fil", "ter"]);
        // A word that does not break is as wide as shaped whole, with its ligatures (as
        // Chrome sets it); where it breaks, each line has its part shaped on its own.
        for (text, width) in &found {
            assert!(
                (width - shaped(text)).abs() < 0.01,
                "{text}: {width} {}",
                shaped(text)
            );
        }
        assert!(shaped("bitte") < shaped("bit") + shaped("te") - 0.5);
    }

    /// The lines below `fragment` with the text of their runs: the top on the page and the text.
    fn line_tops(fragment: &Fragment, y: f32, out: &mut Vec<(f32, String)>) {
        let top = y + fragment.rect.y.to_px();
        if matches!(fragment.kind, FragmentKind::Line { .. }) {
            let mut found = Vec::new();
            runs(fragment, &mut found);
            out.push((top, found.iter().map(|run| run.text.as_str()).collect()));
            return;
        }
        for child in &fragment.children {
            line_tops(child, top, out);
        }
    }

    #[test]
    fn draws_emphasis_marks_and_makes_room_for_them_as_chrome_does() {
        let pages = render(
            "<style>@page { margin: 0 } body { font: 20px/20px Inter; margin: 0 } p { margin: 0; width: 200px }
               .em { text-emphasis: filled circle; text-emphasis-position: under }</style>
             <p class=em>ab, c de fg hi jk lm no pq rs</p><p>next</p>",
        );
        let fonts = test_fonts();
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], &mut found);
        let marks = emphasis::marks(&fonts, found[0], None).expect("marks");
        // One for each letter, none for spaces and the comma, at half the font size, just under
        // the em box and centered on the letters.
        assert_eq!(marks.glyphs.len(), 13);
        assert_eq!((marks.size, marks.offset), (10.0, 12.0));
        assert!((marks.x - 1.04).abs() < 0.02, "{}", marks.x);
        // The marks push the next line down where they reach past the line box.
        let mut lines = Vec::new();
        line_tops(&pages[0].fragments[0], 0.0, &mut lines);
        let tops: Vec<f32> = lines.iter().map(|(top, _)| *top).collect();
        assert_eq!(tops, [0.0, 31.0, 62.0]);
    }

    #[test]
    fn lays_out_ruby_as_chrome_does() {
        let pages = render(
            "<style>@page { margin: 0 } body { font: 20px/20px Inter; margin: 0 } p { margin: 0; width: 200px }</style>
             <p><ruby>Fagus<rt>beech</rt></ruby> and <ruby>a<rt>wide annotation</rt><rp>)</rp></ruby> b</p>",
        );
        let mut lines = Vec::new();
        line_tops(&pages[0].fragments[0], 0.0, &mut lines);
        // The annotations, a line of their own over the em box of their base, push the line down.
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, 10.0);
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], &mut found);
        let texts: Vec<&str> = found.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(texts, ["beech", "Fagus", " and ", "wide annotation", "a", " b"]);
        assert_eq!(found[0].size, 10.0);
        let positions = |fragment: &Fragment| {
            fn walk(fragment: &Fragment, origin: (f32, f32), out: &mut Vec<(f32, f32)>) {
                let (x, y) = (
                    origin.0 + fragment.rect.x.to_px(),
                    origin.1 + fragment.rect.y.to_px(),
                );
                if let FragmentKind::Text(run) = &fragment.kind {
                    out.push((x, y + run.baseline.to_px()));
                }
                for child in &fragment.children {
                    walk(child, (x, y), out);
                }
            }
            let mut out = Vec::new();
            walk(fragment, (0.0, 0.0), &mut out);
            out
        };
        let at = positions(&pages[0].fragments[0]);
        // Baselines and starts as in Chrome: the base on 27, its annotation on 8, the wide
        // annotation reaching over the text beside its base by half its font size.
        assert_eq!((at[0].1, at[1].1), (8.0, 27.0));
        assert!((at[0].0 - 13.89).abs() < 0.05, "{:?}", at[0]);
        assert!((at[3].0 - 98.75).abs() < 0.05, "{:?}", at[3]);
        assert!((at[4].0 - 130.80).abs() < 0.05, "{:?}", at[4]);
        assert!((at[5].0 - 169.08).abs() < 0.05, "{:?}", at[5]);
    }

    #[test]
    fn trims_the_first_and_last_lines_to_text_box_edges() {
        let pages = render(
            "<style>@page { margin: 0 } body { font: 20px/32px Inter; margin: 0 } p { margin: 0 }
               .trim { text-box: trim-both cap alphabetic }</style>
             <p class=trim>Trim</p><p>after</p>",
        );
        let mut lines = Vec::new();
        line_tops(&pages[0].fragments[0], 0.0, &mut lines);
        // The first baseline lies the cap height below the top, the block ends at the last one.
        let baseline = |index: usize| {
            let (top, _) = lines[index];
            top + 23.0
        };
        assert!((baseline(0) - 14.55).abs() < 0.01, "{lines:?}");
        assert!((baseline(1) - (14.55 + 23.0)).abs() < 0.01, "{lines:?}");
    }

    #[test]
    fn breaks_words_that_overflow_like_browsers() {
        let pages = render(
            "<style>body { font: 16px/20px Inter; margin: 0 } p { width: 120px; margin: 0 }
               .anywhere { overflow-wrap: anywhere } .all { word-break: break-all }</style>
             <p>IBAN DE89370400440532013000</p>
             <p class=anywhere>IBAN DE89370400440532013000</p>
             <p class=all>IBAN DE89370400440532013000</p>
             <p class=anywhere>Siehe https://example.com/rechnungen/RE-0042</p>",
        );
        let mut lines = Vec::new();
        line_texts(&pages[0].fragments[0], &mut lines);
        assert_eq!(
            lines,
            [
                // Without either property, the number runs past the edge.
                "IBAN",
                "DE89370400440532013000",
                // `overflow-wrap` moves it to a line of its own, then breaks it where that is full.
                "IBAN",
                "DE893704004",
                "40532013000",
                // `word-break: break-all` breaks it where the first line is full.
                "IBAN DE89370",
                "400440532013",
                "000",
                // As in Chrome, a URL breaks neither after its slashes nor after its dots.
                "Siehe",
                "https://example",
                ".com/rechnung",
                "en/RE-0042",
            ]
        );
    }

    #[test]
    fn counts_breaks_inside_words_for_the_minimum_width() {
        let (document, pages) = render_document(
            "<style>body { font: 16px/20px Inter; margin: 0 } div { width: 0 }
               span { display: inline-block } .anywhere { overflow-wrap: anywhere }
               .word { overflow-wrap: break-word } .all { word-break: break-all }</style>
             <div><span class=anywhere>DE8937</span></div>
             <div><span class=word>DE8937</span></div>
             <div><span class=all>DE8937</span></div>",
        );
        let width = |class| {
            find(&pages[0], &document, class)
                .expect(class)
                .2
                .rect
                .width
                .to_px()
        };
        // The breaks of `overflow-wrap: break-word` do not count, those of `anywhere` and
        // `word-break: break-all` do: the box shrinks to the widest character.
        assert!(width("word") > 50.0);
        assert!(width("anywhere") < 15.0);
        assert!(width("all") < 15.0);
    }

    #[test]
    fn continues_a_broken_word_on_the_next_page() {
        let pages = render(
            "<style>@page { size: 200px 100px; margin: 0 } body { font: 16px/20px Inter; margin: 0 }
               p { width: 60px; margin: 0; overflow-wrap: anywhere }</style>
             <p>DE89370400440532013000DE89370400440532013000</p>",
        );
        assert_eq!(pages.len(), 2);
        let mut lines = Vec::new();
        for page in &pages {
            line_texts(&page.fragments[0], &mut lines);
        }
        assert_eq!(lines.concat(), "DE89370400440532013000DE89370400440532013000");
    }

    #[test]
    fn shapes_words_across_their_break_opportunities() {
        fn width(fragment: &Fragment) -> f32 {
            let own = match &fragment.kind {
                FragmentKind::Text(run) => run.glyphs.iter().map(|glyph| glyph.advance).sum(),
                _ => 0.0,
            };
            own + fragment.children.iter().map(width).sum::<f32>()
        }
        let pages = render("<style>body { font: 16px Inter; margin: 0 }</style><p>RE-2026-0042</p>");
        let mut fonts = FontDb::new();
        fonts
            .add_font_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts"))
            .expect("test fonts");
        let face = fonts
            .query(&["Inter"], 400.0, fagus_text::FontStyle::Normal)
            .expect("Inter");
        let shaped = |text: &str| -> f32 {
            fonts
                .shape(face, text)
                .glyphs
                .iter()
                .map(|glyph| glyph.x_advance)
                .sum::<f32>()
                * fonts.face(face).scale(16.0)
        };
        // The line may break after each hyphen, but the kerning across them stays, as in browsers.
        let whole = shaped("RE-2026-0042");
        assert!((shaped("RE-") + shaped("2026-") + shaped("0042") - whole).abs() > 0.05);
        assert!((width(&pages[0].fragments[0]) - whole).abs() < 0.01);
    }

    #[test]
    fn sizes_images_without_a_natural_size_like_browsers() {
        let (document, pages) = render_document_with(
            "<style>body { margin: 0; font: 16px/20px Inter } div { width: 500px }</style>
             <div><svg class=fill viewBox='0 0 40 10'></svg></div>
             <div style='display: flex; align-items: flex-start'><svg class=flex viewBox='0 0 40 10'></svg></div>
             <div style='display: flex; flex-direction: column; align-items: start'>
               <svg class=column viewBox='0 0 40 10'></svg></div>
             <div><span style='display: inline-block'><svg class=shrink viewBox='0 0 40 10'></svg></span></div>
             <div><svg class=margins viewBox='0 0 40 10' style='margin: 0 20px; padding: 5px'></svg></div>
             <div><svg class=wide width=80></svg></div>
             <div><svg class=none></svg></div>",
            |document| {
                // Inline SVGs without `width` and `height`: a ratio from the `viewBox` at most.
                let mut images = Images::default();
                for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
                    if let Some(data) = document.element(node).filter(|data| data.is_svg("svg")) {
                        let ratio = data.attribute("viewBox").map(|_| 4.0);
                        images.insert_inline(
                            node,
                            Arc::new(Image {
                                data: Arc::new(Vec::new()),
                                format: ImageFormat::Svg,
                                width: 40.0,
                                height: 10.0,
                                natural: (false, false),
                                ratio,
                                inline: true,
                                orientation: 1,
                                filters: Arc::from([]),
                                pad: 0,
                            }),
                        );
                    }
                }
                images
            },
        );
        let size = |class| {
            let rect = find(&pages[0], &document, class).expect(class).2.rect;
            (rect.width.to_px(), rect.height.to_px())
        };
        // With a ratio alone, as wide as the containing block allows, in flex containers too.
        assert_eq!(size("fill"), (500.0, 125.0));
        assert_eq!(size("flex"), (500.0, 125.0));
        assert_eq!(size("column"), (500.0, 125.0));
        assert_eq!(size("margins"), (460.0, 122.5));
        // Shrinking to fit, it counts for nothing.
        assert_eq!(size("shrink"), (0.0, 0.0));
        // Without a ratio, the default size of 300 × 150 fills in what is missing.
        assert_eq!(size("wide"), (80.0, 150.0));
        assert_eq!(size("none"), (300.0, 150.0));
    }

    #[test]
    fn sizes_images_by_their_size_attributes_below_styles() {
        let (document, pages) = render_document_with(
            "<style>body { margin: 0 } img { display: block }</style>
             <img class=both src=square.png width=100 height=50>
             <img class=auto-height src=square.png width=100 height=50 style='height: auto'>
             <img class=auto-width src=square.png width=100 height=50 style='width: auto'>
             <img class=styled src=square.png width=100 height=50 style='width: 60px'>",
            |_| {
                // A square image, 200 pixels wide.
                let mut images = Images::default();
                images.insert(
                    "square.png",
                    Arc::new(Image {
                        data: Arc::new(Vec::new()),
                        format: ImageFormat::Png,
                        width: 200.0,
                        height: 200.0,
                        natural: (true, true),
                        ratio: Some(1.0),
                        inline: false,
                        orientation: 1,
                        filters: Arc::from([]),
                        pad: 0,
                    }),
                );
                images
            },
        );
        let size = |class| {
            let rect = find(&pages[0], &document, class).expect(class).2.rect;
            (rect.width.to_px(), rect.height.to_px())
        };
        // `width` and `height` are presentational hints: a style's `auto` overrides them, and
        // the image's own ratio sizes the other side, as in Chrome.
        assert_eq!(size("both"), (100.0, 50.0));
        assert_eq!(size("auto-height"), (100.0, 100.0));
        assert_eq!(size("auto-width"), (50.0, 50.0));
        assert_eq!(size("styled"), (60.0, 50.0));
    }

    #[test]
    fn zooms_the_natural_and_default_sizes_of_images() {
        let (document, pages) = render_document_with(
            "<style>body { margin: 0 } img, svg { display: block }</style>
             <img class=half src=square.png style='zoom: 50%'>
             <div style='zoom: 2'><img class=double src=square.png><svg class=none></svg></div>",
            |document| {
                let image = |width: f32, natural: bool, inline: bool| {
                    Arc::new(Image {
                        data: Arc::new(Vec::new()),
                        format: if inline {
                            ImageFormat::Svg
                        } else {
                            ImageFormat::Png
                        },
                        width,
                        height: width,
                        natural: (natural, natural),
                        ratio: natural.then_some(1.0),
                        inline,
                        orientation: 1,
                        filters: Arc::from([]),
                        pad: 0,
                    })
                };
                let mut images = Images::default();
                images.insert("square.png", image(200.0, true, false));
                for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
                    if document.element(node).is_some_and(|data| data.is_svg("svg")) {
                        images.insert_inline(node, image(40.0, false, true));
                    }
                }
                images
            },
        );
        let size = |class| {
            let rect = find(&pages[0], &document, class).expect(class).2.rect;
            (rect.width.to_px(), rect.height.to_px())
        };
        // An image pixel is as many CSS pixels as the zoom, as is the default size of 300 × 150.
        assert_eq!(size("half"), (100.0, 100.0));
        assert_eq!(size("double"), (400.0, 400.0));
        assert_eq!(size("none"), (600.0, 300.0));
    }

    #[test]
    fn breaks_tall_table_rows_across_pages() {
        let text = "Wort ".repeat(120);
        let (document, pages) = render_document(&format!(
            "<style>
               @page {{ size: 300px 300px; margin: 0; }}
               body {{ margin: 0; font: 16px/20px Inter; }}
               table {{ width: 100%; border-collapse: collapse; }}
               td {{ padding: 0; vertical-align: top; }}
               .spacer {{ height: 200px; }}
             </style>
             <div class=spacer></div>
             <table><thead><tr><th class=head>Kopf</th><th>B</th></tr></thead>
             <tr><td class=short>kurz</td><td>{text}</td></tr></table>"
        ));
        assert!(pages.len() >= 3, "{}", pages.len());
        // The row starts on the first page, after the header, and goes on below the header on
        // the next ones; the short cell is empty there, as wide as its column.
        let mut texts = Vec::new();
        line_texts(&pages[0].fragments[0], &mut texts);
        assert_eq!(texts.first().map(String::as_str), Some("Kopf"));
        assert!(texts.iter().any(|text| text.starts_with("Wort")), "{texts:?}");
        let (_, _, first) = find(&pages[0], &document, "short").expect("short cell");
        for page in &pages[1..] {
            assert!(find(page, &document, "head").is_some(), "the header repeats");
            let (_, _, cell) = find(page, &document, "short").expect("short cell");
            assert!(cell.children.is_empty());
            assert_eq!(cell.rect.width, first.rect.width);
        }
        // Every word is on some page.
        let words: usize = pages
            .iter()
            .map(|page| {
                let mut lines = Vec::new();
                line_texts(&page.fragments[0], &mut lines);
                lines
                    .iter()
                    .map(|line| line.matches("Wort").count())
                    .sum::<usize>()
            })
            .sum();
        assert_eq!(words, 120);
    }

    #[test]
    fn sizes_tables_in_flex_items_to_the_item() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 16px Inter } table { width: 100%; border-collapse: collapse }
               .head { display: flex; width: 600px } .space { width: 300px; flex: none } .meta { flex: 1 }</style>
             <div class=head><div class=space></div><table class=meta><tr><td>Nummer</td><td class=value>RE-1</td></tr></table></div>",
        );
        let (x, _, table) = find(&pages[0], &document, "meta").expect("table");
        assert_eq!((x, table.rect.width.to_px()), (300.0, 300.0));
        let (value_x, _, value) = find(&pages[0], &document, "value").expect("cell");
        assert!(value_x + value.rect.width.to_px() <= 600.0);
    }

    #[test]
    fn lays_out_paragraphs_on_one_page() {
        let pages =
            render("<style>body { font-family: Inter }</style><p>Hallo Welt</p><p>Zweiter Absatz</p>");
        assert_eq!(pages.len(), 1);
        assert_eq!(lines(&pages[0].fragments[0]), 2);
        // A4, rounded to layout units.
        assert_eq!(
            pages[0].width,
            units::Au::from_px((210.0_f32 * 96.0 / 25.4 * 64.0).round() / 64.0)
        );
    }

    #[test]
    fn breaks_long_documents_into_pages() {
        let paragraphs =
            "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.</p>"
                .repeat(120);
        let pages = render(&format!(
            "<style>body {{ font-family: Inter }}</style>{paragraphs}"
        ));
        assert!(pages.len() > 3, "{} pages", pages.len());
        let total: usize = pages.iter().map(|page| lines(&page.fragments[0])).sum();
        assert!(total >= 120, "every paragraph is laid out: {total} lines");
    }

    #[test]
    fn places_the_page_area_like_browsers() {
        let (document, pages) = render_document(
            "<style>@page { size: A4; margin: 20mm 18mm 25mm 20mm } body { margin: 0 }</style>
             <div class=wide style='height: 10px'></div>",
        );
        // As measured in Chrome: the area starts at the margins rounded to whole pixels (75.59
        // to 76) and is as wide as the rest of the page rounded up (650.08 to 651).
        let (x, y, wide) = find(&pages[0], &document, "wide").expect("box");
        assert_eq!((x, y, wide.rect.width.to_px()), (76.0, 76.0, 651.0));
        // The page size is rounded and the margins are cut to layout units first: with margins of
        // 12mm, A4 leaves 702.99 pixels exactly, but 703.02 that way, so 704 to lay out in.
        let (document, pages) = render_document(
            "<style>@page { size: A4; margin: 12mm } body { margin: 0 }</style>
             <div class=wide style='height: 10px'></div>",
        );
        let (_, _, wide) = find(&pages[0], &document, "wide").expect("box");
        assert_eq!(wide.rect.width.to_px(), 704.0);
        // A4 with margins of 20mm leaves 971.34 pixels, which Chrome rounds up to 972: a box of
        // 972 pixels fits on one page.
        let pages = render(
            "<style>@page { size: A4; margin: 20mm } body { margin: 0 }</style>
             <div style='height: 972px'></div>",
        );
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn honours_forced_breaks_and_page_size() {
        let pages = render(
            "<style>@page { size: A5 landscape } body { font-family: Inter } h1 { break-before: page }</style><p>a</p><h1>b</h1><h1>c</h1>",
        );
        assert_eq!(pages.len(), 3);
        assert!(pages[0].width > pages[0].height, "landscape");
    }

    #[test]
    fn adds_margin_boxes_with_page_counts() {
        let paragraphs = "<p>Text</p>".repeat(3);
        let pages = render(&format!(
            "<style>@page {{ @bottom-right {{ content: 'Seite ' counter(page) ' von ' counter(pages) }} }}
             body {{ font-family: Inter }} p {{ break-after: page }}</style>{paragraphs}"
        ));
        assert_eq!(pages.len(), 3);
        let text = |page: &Page| -> String {
            fn collect(fragment: &Fragment, out: &mut String) {
                if let FragmentKind::Text(run) = &fragment.kind {
                    out.push_str(&run.text);
                }
                fragment.children.iter().for_each(|child| collect(child, out));
            }
            let mut out = String::new();
            page.fragments
                .iter()
                .skip(1)
                .for_each(|fragment| collect(fragment, &mut out));
            out
        };
        assert_eq!(text(&pages[1]), "Seite 2 von 3");
    }

    #[test]
    fn places_images_in_margin_boxes() {
        // The page offset and height of the first image in `fragment`.
        fn image(fragment: &Fragment, x: f32, y: f32) -> Option<(f32, f32, f32)> {
            let (x, y) = (x + fragment.rect.x.to_px(), y + fragment.rect.y.to_px());
            if matches!(fragment.kind, FragmentKind::Image(_)) {
                return Some((x, y, fragment.rect.height.to_px()));
            }
            fragment.children.iter().find_map(|child| image(child, x, y))
        }
        // Fold marks drawn by an image in the left margin of the first page (Chrome 157: the
        // image's top at the top of the page area, 15mm, its left at the sheet's edge).
        let (_, pages) = render_document_with(
            "<style>@page { size: A4; margin: 40mm 19mm 37mm 20mm }
             @page :first { margin-top: 15mm;
               @left-top { content: url(marks.svg); vertical-align: top; text-align: left; font-size: 0; line-height: 0 } }
             p { break-after: page }</style><p>One</p><p>Two</p>",
            |_| {
                let mut images = Images::default();
                images.insert(
                    "marks.svg",
                    Arc::new(Image {
                        data: Arc::new(Vec::new()),
                        format: ImageFormat::Svg,
                        width: 20.0 * 96.0 / 25.4,
                        height: 200.0 * 96.0 / 25.4,
                        natural: (true, true),
                        ratio: Some(0.1),
                        inline: false,
                        orientation: 1,
                        filters: Arc::from([]),
                        pad: 0,
                    }),
                );
                images
            },
        );
        let found = |page: &Page| {
            page.fragments
                .iter()
                .skip(1)
                .find_map(|fragment| image(fragment, 0.0, 0.0))
        };
        let (x, y, height) = found(&pages[0]).expect("the image of the first page");
        assert!(x.abs() < 0.01, "{x}");
        assert!((y - 15.0 * 96.0 / 25.4).abs() < 0.5, "{y}");
        assert!((height - 200.0 * 96.0 / 25.4).abs() < 0.5, "{height}");
        assert_eq!(found(&pages[1]), None);
    }

    #[test]
    fn places_absolute_boxes_in_their_containing_block() {
        let (document, pages) = render_document(
            "<style>@page { size: 400px 400px; margin: 0 } body { margin: 0; font-family: Inter }
             .cb { position: relative; margin: 50px; width: 200px; height: 100px; border: 10px solid }
             .corner { position: absolute; right: 5px; bottom: 5px; width: 20px; height: 20px }
             .center { position: absolute; inset: 0; margin: auto; width: 40px; height: 40px }
             .static { position: absolute; width: 10px; height: 10px }</style>
             <div class=cb><p style='margin: 0; height: 30px'></p><div class=static></div>
             <div class=corner></div><div class=center></div></div>",
        );
        let page = &pages[0];
        let (x, y, _) = find(page, &document, "corner").expect("corner box");
        assert_eq!((x, y), (235.0, 135.0), "inset from the padding box");
        let (x, y, _) = find(page, &document, "center").expect("centered box");
        assert_eq!((x, y), (140.0, 90.0), "auto margins center between the insets");
        let (x, y, _) = find(page, &document, "static").expect("box at its static position");
        assert_eq!(
            (x, y),
            (60.0, 90.0),
            "below the paragraph, where it would be in flow"
        );
    }

    #[test]
    fn keeps_text_together_around_absolute_boxes() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } .note { position: absolute; top: 0 }</style>
             <p class=text>Hallo <span class=note>Hinweis</span>Welt</p>",
        );
        let (_, _, paragraph) = find(&pages[0], &document, "text").expect("paragraph");
        assert_eq!(lines(paragraph), 1, "the absolute box does not split the line");
        assert!(find(&pages[0], &document, "note").is_some());
    }

    #[test]
    fn repeats_fixed_boxes_on_every_page() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } p { break-after: page }
             .footer { position: fixed; bottom: 0; left: 0 }</style>
             <div class=footer>Fußzeile</div><p>1</p><p>2</p><p>3</p>",
        );
        assert_eq!(pages.len(), 3);
        // Without margins of its own, a page has Chrome's: 1cm in whole points, 28pt. The page
        // area of A4 starts at 37px (the margin rounded); fixed boxes are placed in the viewport,
        // 1047px tall (what the margins leave, cut to whole pixels), as in Chrome.
        let margin = 28.0_f32 * 96.0 / 72.0;
        let area_bottom = margin.round() + (297.0 * 96.0 / 25.4 - 2.0 * margin).floor();
        for page in &pages {
            let (_, y, footer) = find(page, &document, "footer").expect("footer on every page");
            let bottom = y + footer.rect.height.to_px();
            assert!(
                (bottom - area_bottom).abs() < 0.1,
                "at the bottom of the page area: {bottom}"
            );
        }
    }

    #[test]
    fn offsets_relative_boxes() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font-family: Inter } div { height: 10px }
             .moved { position: relative; left: 10px; bottom: 4px }</style>
             <div class=before></div><div class=moved></div><div class=after></div>",
        );
        let (_, before, _) = find(&pages[0], &document, "before").expect("box");
        let (x, y, _) = find(&pages[0], &document, "moved").expect("box");
        let (_, after, _) = find(&pages[0], &document, "after").expect("box");
        // The page area starts at the default margin of 28pt, rounded: 37px.
        assert_eq!(x, 37.0 + 10.0, "moved right");
        assert_eq!(y - before, 6.0);
        assert_eq!(after - before, 20.0, "the following box does not move");
    }

    #[test]
    fn sizes_boxes_by_aspect_ratio() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } .wide { width: 200px; aspect-ratio: 2 }
             .tall { height: 50px; aspect-ratio: 3 } .full { width: 100px; aspect-ratio: 4; font-size: 40px }
             .abs { position: absolute; height: 20px; aspect-ratio: 5 }</style>
             <div class=wide></div><div class=tall></div><div class=full>Text that is taller</div><div class=abs></div>",
        );
        let size = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect("box");
            (fragment.rect.width.to_px(), fragment.rect.height.to_px())
        };
        assert_eq!(size("wide"), (200.0, 100.0));
        assert_eq!(size("tall"), (150.0, 50.0));
        assert!(size("full").1 > 25.0, "never smaller than the content");
        assert_eq!(size("abs"), (100.0, 20.0));
    }

    #[test]
    fn breaks_inline_boxes_across_lines() {
        fn collect<'f>(fragment: &'f Fragment, document: &Document, out: &mut Vec<&'f Fragment>) {
            if let FragmentKind::Box { node: Some(node), .. } = &fragment.kind
                && document.element(*node).is_some_and(|data| data.is("span"))
            {
                out.push(fragment);
            }
            fragment
                .children
                .iter()
                .for_each(|child| collect(child, document, out));
        }
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } p { width: 120px }
             span { border: 2px solid; padding: 0 4px }</style>
             <p>Text <span class=wrap>that runs across the end of the line</span> ends</p>",
        );
        let mut parts = Vec::new();
        collect(&pages[0].fragments[0], &document, &mut parts);
        assert!(parts.len() >= 2, "one fragment per line: {}", parts.len());
        let sides = |fragment: &Fragment| match &fragment.kind {
            FragmentKind::Box { sides, .. } => (sides.left, sides.right),
            _ => unreachable!(),
        };
        assert_eq!(sides(parts[0]), (true, false));
        assert_eq!(sides(parts[parts.len() - 1]), (false, true));
    }

    #[test]
    fn aligns_inline_blocks_on_their_last_baseline() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter; font-size: 16px; line-height: 20px }
             .card { display: inline-block; width: 60px; line-height: 20px }
             .top { display: inline-block; height: 100px; vertical-align: top }</style>
             <p class=line>Text <span class=card>one two three four</span> after</p>
             <p class=tall>Text <span class=top></span></p>",
        );
        let (_, line_y, line) = find(&pages[0], &document, "line").expect("paragraph");
        let (_, card_y, card) = find(&pages[0], &document, "card").expect("card");
        // The card has several lines; its last one shares the baseline with the text.
        assert!(card.rect.height.to_px() >= 40.0);
        let card_bottom = card_y + card.rect.height.to_px();
        let line_bottom = line_y + line.rect.height.to_px();
        assert!(
            (card_bottom - line_bottom).abs() < 1.0,
            "{card_bottom} {line_bottom}"
        );
        let (_, _, tall) = find(&pages[0], &document, "tall").expect("paragraph");
        assert!(
            tall.rect.height.to_px() >= 100.0,
            "the line grows to fit the top-aligned box"
        );
    }

    #[test]
    fn collapses_margins() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 8px; font-family: Inter } div { height: 10px }
             .first { margin-top: 30px } .padded { padding-top: 1px } .padded > div { margin-top: 30px }
             .a { margin-bottom: 10px } .empty { height: auto; margin: 20px 0 } .b { margin-top: 10px }
             .c { margin-bottom: 20px } .d { margin-top: -5px }
             .outer { height: auto } .inner { margin-bottom: 40px } .after { }</style>
             <div class=first></div><div class=padded><div class=nested></div></div>
             <div class=a></div><div class=empty></div><div class=b></div>
             <div class=c></div><div class=d></div>
             <div class=outer><div class=inner></div></div><div class=after></div>",
        );
        let page = &pages[0];
        let top = |class: &str| find(page, &document, class).expect(class).1;
        let bottom = |class: &str| {
            let (_, y, fragment) = find(page, &document, class).expect(class);
            y + fragment.rect.height.to_px()
        };
        assert_eq!(
            top("first"),
            30.0,
            "the child's margin collapses with the body's 8px"
        );
        assert_eq!(
            top("nested") - top("padded"),
            31.0,
            "padding keeps the margin inside"
        );
        assert_eq!(
            top("b") - bottom("a"),
            20.0,
            "an empty box's margins join its neighbors'"
        );
        assert_eq!(top("d") - bottom("c"), 15.0, "negative margins are added");
        assert_eq!(
            top("after") - bottom("inner"),
            40.0,
            "the last child's margin leaves its parent"
        );
        assert_eq!(bottom("outer"), bottom("inner"), "and does not stretch it");
    }

    #[test]
    fn places_right_to_left_content_from_the_right_of_the_page() {
        // A4 with margins of 16mm leaves 672.76 pixels, laid out 673 wide from the left margin
        // rounded to 60 pixels; right to left, Chrome ends them at the right margin rounded,
        // 793.703 - 60, so that they start 0.703 pixels further right.
        let page = |dir: &str| {
            render(&format!(
                "<html dir={dir}><style>@page {{ size: A4; margin: 16mm }} body {{ margin: 0 }}
                   @page {{ @top-center {{ content: 'x' }} }}</style><p>Text</p>"
            ))
            .remove(0)
        };
        let (ltr, rtl) = (page("ltr"), page("rtl"));
        assert_eq!(ltr.shift, 0.0);
        assert!((rtl.shift - 0.703).abs() < 0.01, "{}", rtl.shift);
        // The content is laid out from the same whole pixel.
        assert_eq!(ltr.fragments[0].rect.x, rtl.fragments[0].rect.x);
        // The margin boxes are placed from the right too: the top centre box is as wide, a
        // fraction of a pixel further right.
        let (left, right) = (&ltr.fragments[1], &rtl.fragments[1]);
        assert_eq!(left.rect.width, right.rect.width);
        assert!(right.rect.x > left.rect.x && right.rect.x - left.rect.x < units::Au::from_px(1.0));
    }

    #[test]
    fn starts_right_to_left_documents_with_a_left_page() {
        let margins = |dir: &str| {
            render(&format!(
                "<html dir={dir}><style>@page {{ size: 300px 200px; margin: 20px }}
                   @page :left {{ margin-left: 50px }} body {{ margin: 0 }}</style>
                 <p>One</p><p style='break-before: page'>Two</p>"
            ))
            .iter()
            .map(|page| page.margin[3].to_px())
            .collect::<Vec<_>>()
        };
        assert_eq!(margins("ltr"), [20.0, 50.0]);
        assert_eq!(margins("rtl"), [50.0, 20.0]);
    }

    #[test]
    fn leaves_letter_spacing_out_of_cursive_scripts() {
        // As in Blink: the letters of Arabic (and the digits and punctuation in its runs) take
        // no letter spacing, its spaces and the letters of other scripts do, once a cluster.
        let width = |text: &str, spacing: f32| {
            let pages = render(&format!(
                "<p style='font: 16px Inter; letter-spacing: {spacing}px; white-space: nowrap'>{text}</p>"
            ));
            let mut found = Vec::new();
            runs(&pages[0].fragments[0], &mut found);
            found
                .iter()
                .flat_map(|run| run.glyphs.iter())
                .map(|glyph| glyph.advance)
                .sum::<f32>()
        };
        let added = |text: &str| width(text, 10.0) - width(text, 0.0);
        assert!((added("ببب") - 0.0).abs() < 0.01);
        assert!((added("ببب ab") - 30.0).abs() < 0.01);
        assert!((added("ب 12، ب") - 20.0).abs() < 0.01);
        // Digits before Arabic take its script.
        assert!((added("12 ب") - 10.0).abs() < 0.01);
        assert!((added("אב") - 20.0).abs() < 0.01);
        assert!((added("e\u{301}x") - 20.0).abs() < 0.01);
        // The text around an element decides for the digits in it, as for those without one.
        assert!((added("ب <span>12</span>") - 10.0).abs() < 0.01);
        assert!((added("<b>ب</b> 12") - 10.0).abs() < 0.01);
        assert!((added("<span>12</span> ب") - 10.0).abs() < 0.01);
        assert!((added("ab <span>12</span>") - 50.0).abs() < 0.01);
    }

    #[test]
    fn sets_the_faces_of_a_right_to_left_marker_right_to_left() {
        fn texts(fragment: &Fragment, x: f32, out: &mut Vec<(String, f32)>) {
            let x = x + fragment.rect.x.to_px();
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push((run.text.clone(), x));
            }
            fragment.children.iter().for_each(|child| texts(child, x, out));
        }
        // The full stop of a Hebrew marker comes from another face: the marker "א. " stands
        // with its letter rightmost, the full stop left of it.
        let mut fonts = test_fonts();
        for (file, unicode_range) in [
            ("Inter-Regular.ttf", vec![]),
            ("Geist-Regular.ttf", vec![(0x2E, 0x2E)]),
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
            let path = if file.starts_with("Geist") {
                path.join("corpus/quittio/fonts").join(file)
            } else {
                path.join("fonts").join(file)
            };
            let rule = fagus_text::Descriptors {
                family: "Mixed".to_owned(),
                weight: None,
                style: fagus_text::FontStyle::Normal,
                stretch: None,
                unicode_range,
                ..fagus_text::Descriptors::default()
            };
            let data = Arc::new(std::fs::read(path).expect("font"));
            fonts.add_document_face(Arc::new(
                fagus_text::Face::from_rule(data, 0, &rule).expect("face"),
            ));
        }
        let (_, pages) = render_document_in(
            "<style>body { font: 20px Mixed }</style>
             <ol dir=rtl style='list-style: \"\u{5D0}. \"'><li>x</li></ol>",
            &fonts,
            |_| Images::default(),
        );
        let mut found = Vec::new();
        texts(&pages[0].fragments[0], 0.0, &mut found);
        let at = |text: &str| {
            found
                .iter()
                .find(|(run, _)| run == text)
                .unwrap_or_else(|| panic!("{text} in {found:?}"))
                .1
        };
        assert!(at("\u{5D0}") > at("."), "{found:?}");
        assert!(at(".") > at(" "), "{found:?}");
    }

    #[test]
    fn breaks_lines_after_markers_inside_the_box() {
        // As in Chrome, at the min-content width the content goes below the marker.
        let (document, pages) = render_document(
            "<style>body { font: 16px Inter } ol, ul { list-style-position: inside; padding: 0 }
               div { width: min-content }</style>
             <div><ol><li class=a>word</li></ol></div>
             <div><ul><li class=b>word</li></ul></div>",
        );
        for class in ["a", "b"] {
            let (_, _, item) = find(&pages[0], &document, class).expect(class);
            let mut lines = Vec::new();
            line_boxes(item, (0.0, 0.0), &mut lines);
            assert_eq!(lines.len(), 2, "{class}");
        }
    }

    #[test]
    fn sets_text_areas_on_their_bottom_edge() {
        let (document, pages) = render_document(
            "<style>body { font: 16px Inter; margin: 0 } textarea { font-family: Inter }</style>
             <p class=p>x <textarea class=area rows=3>a</textarea></p>",
        );
        // The bottom of the text area lies on the baseline of the line.
        let (_, area_y, area) = find(&pages[0], &document, "area").expect("area");
        let (_, p_y, p) = find(&pages[0], &document, "p").expect("p");
        let baseline = p_y
            + block::first_baseline(p, fagus_style::values::WritingMode::HorizontalTb)
                .expect("a line")
                .to_px();
        assert!((area_y + area.rect.height.to_px() - baseline).abs() < 0.5);
    }

    #[test]
    fn gives_scroll_containers_the_scroll_bars_of_chrome_on_linux() {
        // `overflow: auto` gets a bar along an axis where the content overflows, `scroll` both
        // always; a bar is 15px thick (10px `thin`, none for `none`) and adds to an auto height
        // (Chrome 157 on Linux, measured).
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 } div div { height: 20px }
               .s { width: 300px }</style>
             <div class='s wide' style='overflow-x: auto'><div style='width: 600px'></div></div>
             <div class='s fits' style='overflow: auto'><div style='width: 300px'></div></div>
             <div class='s scroll' style='overflow: scroll'><div></div></div>
             <div class='s thin' style='overflow-x: auto; scrollbar-width: thin'><div style='width: 600px'></div></div>
             <div class='s none' style='overflow-x: auto; scrollbar-width: none'><div style='width: 600px'></div></div>",
        );
        let bars = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect(class);
            let FragmentKind::Box { style, .. } = &fragment.kind else {
                panic!("{class}: a box");
            };
            (
                style.scrollbars.vertical,
                style.scrollbars.horizontal,
                fragment.rect.height.to_px(),
            )
        };
        assert_eq!(bars("wide"), (false, true, 35.0));
        assert_eq!(bars("fits"), (false, false, 20.0));
        assert_eq!(bars("scroll"), (true, true, 35.0));
        assert_eq!(bars("thin"), (false, true, 30.0));
        assert_eq!(bars("none"), (false, false, 20.0));
        // The vertical bar takes its room from the content: the child of the scrolling box is
        // 285px wide.
        let (_, _, scroll) = find(&pages[0], &document, "scroll").expect("scroll");
        assert_eq!(scroll.children[0].rect.width.to_px(), 285.0);
    }

    #[test]
    fn places_absolute_boxes_inside_the_scroll_bars_and_gutters() {
        // The padding box in which absolutely positioned boxes are placed lies inside the scroll
        // bars and the gutters of a scroll container, between its border and its padding.
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 }
               .c { position: relative; width: 100px; height: 50px; border: 0 solid; border-width: 0 2px }
               .a { position: absolute; left: 0; top: 0; width: 100%; height: 100% }</style>
             <div class=c style='overflow-y: auto; scrollbar-gutter: stable'><div class='a gutter'></div></div>
             <div class=c style='overflow: scroll' dir=rtl><div class='a bars'></div></div>
             <div class=c style='overflow: hidden'><div class='a none'></div></div>",
        );
        let placed = |class: &str| {
            let (x, _, fragment) = find(&pages[0], &document, class).expect(class);
            (x, fragment.rect.width.to_px(), fragment.rect.height.to_px())
        };
        assert_eq!(placed("gutter"), (2.0, 85.0, 50.0));
        assert_eq!(placed("bars"), (17.0, 85.0, 35.0));
        assert_eq!(placed("none"), (2.0, 100.0, 50.0));
    }

    #[test]
    fn leaves_hanging_spaces_out_of_the_scrollable_overflow() {
        // Spaces that hang at the end of a line add nothing that scrolls, as in Chrome: a line
        // that fills its box but for them (two zeros are 25.23 pixels wide) gives no scroll bar.
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 20px Inter }
               div { width: 25.25px; height: 50px; overflow-x: auto; overflow-y: hidden }</style>
             <div class=wrap style='white-space: pre-wrap'>00 00</div>
             <div class=normal>00 00</div>
             <div class=rtl style='white-space: pre-wrap' dir=rtl>00 00</div>
             <div class=pre style='white-space: pre'>00 00</div>",
        );
        let horizontal = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect(class);
            let FragmentKind::Box { style, .. } = &fragment.kind else {
                panic!("{class}: a box");
            };
            style.scrollbars.horizontal
        };
        assert!(!horizontal("wrap"));
        assert!(!horizontal("normal"));
        assert!(!horizontal("rtl"));
        assert!(horizontal("pre"));
    }

    #[test]
    fn gives_scroll_bars_where_chrome_on_linux_prints_them() {
        // A table cell scrolls; an image does not (`overflow` only clips it). A bar takes its
        // room from a minimum and maximum width too. Content that a box clips along one axis
        // overflows along the other; what a box with layout containment holds does not overflow.
        let (document, pages) = render_document_with(
            "<style>@page { margin: 0 } body { margin: 0 } .big { width: 300px; height: 300px }</style>
             <div style='display: table-cell; max-width: 100px; overflow-x: auto' class=cell><div class=big></div></div>
             <svg class=img style='overflow: scroll; width: 50px; height: 50px; display: block'></svg>
             <div class=max style='overflow-y: scroll; max-width: 60px'>x</div>
             <div class=clip style='overflow: auto; width: 100px; height: 100px'>
               <div style='overflow-x: clip; height: 50px'><div class=big></div></div></div>
             <div class=contained style='overflow: auto; width: 100px; height: 100px'>
               <div style='contain: layout; height: 50px'><div class=big></div></div></div>
             <div class=moved style='overflow: auto; width: 60px; height: 60px; padding: 10px'>
               <div style='position: relative; left: 10px; top: 10px; width: 60px; height: 60px'></div></div>",
            |document| {
                let mut images = Images::default();
                for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
                    if document.element(node).is_some_and(|data| data.is_svg("svg")) {
                        images.insert_inline(
                            node,
                            Arc::new(Image {
                                data: Arc::new(Vec::new()),
                                format: ImageFormat::Svg,
                                width: 300.0,
                                height: 300.0,
                                natural: (true, true),
                                ratio: Some(1.0),
                                inline: true,
                                orientation: 1,
                                filters: Arc::from([]),
                                pad: 0,
                            }),
                        );
                    }
                }
                images
            },
        );
        let bars = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect(class);
            let FragmentKind::Box { style, .. } = &fragment.kind else {
                panic!("{class}: a box");
            };
            (
                style.scrollbars.vertical,
                style.scrollbars.horizontal,
                fragment.rect.width.to_px(),
            )
        };
        assert_eq!(bars("cell"), (false, true, 100.0));
        assert_eq!(bars("img"), (false, false, 50.0));
        assert_eq!(bars("max"), (true, false, 60.0));
        assert_eq!(bars("clip"), (true, false, 100.0));
        assert_eq!(bars("contained"), (false, false, 100.0));
        // A relatively positioned child adds the end padding where flow put it, not where it
        // was moved to (which stays inside the padding box here).
        assert_eq!(bars("moved"), (false, false, 80.0));
    }

    #[test]
    fn keeps_the_gutters_of_scrollbar_gutter() {
        // `stable` keeps the room of a vertical bar on the right of a scroll container (on the
        // left right to left) and `both-edges` on both sides; a bar lies in the gutter on its
        // side. Boxes that do not scroll keep none (Chrome on Linux: Blink's
        // `HasScrollbarGutters`).
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 } div > div { height: 20px }
               .s { width: 300px; overflow: hidden; scrollbar-gutter: stable }</style>
             <div class='s stable'><div></div></div>
             <div class='s both' style='scrollbar-gutter: stable both-edges'><div></div></div>
             <div class='s rtl' dir=rtl><div></div></div>
             <div class='s bar' style='overflow: auto; height: 10px'><div></div></div>
             <div class='s bars' style='overflow: scroll; scrollbar-gutter: both-edges stable'><div></div></div>
             <div class='s visible' style='overflow: visible'><div></div></div>
             <div class='s none' style='scrollbar-width: none'><div></div></div>
             <div class='s border' style='box-sizing: border-box; padding: 0 5px'><div></div></div>",
        );
        let child = |class: &str| {
            let (x, _, fragment) = find(&pages[0], &document, class).expect(class);
            let FragmentKind::Box { style, .. } = &fragment.kind else {
                panic!("{class}: a box");
            };
            let inner = &fragment.children[0];
            (
                inner.rect.x.to_px() + fragment.rect.x.to_px() - x,
                inner.rect.width.to_px(),
                fragment.rect.width.to_px(),
                style.scrollbars.vertical,
            )
        };
        assert_eq!(child("stable"), (0.0, 285.0, 300.0, false));
        assert_eq!(child("both"), (15.0, 270.0, 300.0, false));
        assert_eq!(child("rtl"), (15.0, 285.0, 300.0, false));
        assert_eq!(child("bar"), (0.0, 285.0, 300.0, true));
        assert_eq!(child("bars"), (15.0, 270.0, 300.0, true));
        assert_eq!(child("visible"), (0.0, 300.0, 300.0, false));
        assert_eq!(child("none"), (0.0, 300.0, 300.0, false));
        assert_eq!(child("border"), (5.0, 275.0, 300.0, false));
    }

    #[test]
    fn prints_overflowing_text_areas_with_a_scroll_bar() {
        // Chrome on Linux prints a scroll bar beside the text of a text area that overflows it,
        // on its right, or on its left right to left, which takes 15px of its width; the lines
        // take the rest.
        fn extent(fragment: &Fragment, x: f32, out: &mut (f32, f32)) {
            let x = x + fragment.rect.x.to_px();
            if let FragmentKind::Text(_) = &fragment.kind {
                out.0 = out.0.min(x);
                out.1 = out.1.max(x + fragment.rect.width.to_px());
            }
            fragment.children.iter().for_each(|child| extent(child, x, out));
        }
        let long = "a".repeat(80);
        let (document, pages) = render_document(&format!(
            "<style>body {{ font: 16px Inter; margin: 0 }}
               textarea {{ font-family: Inter; width: 100px; height: 40px; padding: 0; border: 0 }}</style>
             <textarea class=ltr>{long}</textarea><textarea class=rtl dir=rtl>{long}</textarea>"
        ));
        for (class, start) in [("ltr", 0.0), ("rtl", 15.0)] {
            let (x, _, area) = find(&pages[0], &document, class).expect(class);
            let mut found = (f32::MAX, f32::MIN);
            extent(area, x - area.rect.x.to_px(), &mut found);
            assert!(found.1 - found.0 > 75.0, "{class}: {found:?}");
            assert!(
                found.0 >= x + start - 0.5 && found.1 <= x + start + 85.5,
                "{class}: {found:?}"
            );
            let FragmentKind::Box { style, .. } = &area.kind else {
                panic!("{class}: a box");
            };
            assert!(
                style.scrollbars.vertical && !style.scrollbars.horizontal,
                "{class}"
            );
        }
    }

    #[test]
    fn lays_out_right_to_left_text_and_boxes() {
        fn texts(fragment: &Fragment, x: f32, out: &mut Vec<(String, f32, f32)>) {
            let x = x + fragment.rect.x.to_px();
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push((run.text.clone(), x, x + fragment.rect.width.to_px()));
            }
            fragment.children.iter().for_each(|child| texts(child, x, out));
        }
        let (document, pages) = render_document(
            "<style>body { font-family: Inter; width: 400px } .b { border-left: 2px solid; padding: 0 3px }
             .c { display: flex } .t td { padding: 0 }</style>
             <p dir=rtl>one</p>
             <p>ab <bdo dir=rtl>xy</bdo> cd</p>
             <ul dir=rtl><li>item</li></ul>
             <div class=c dir=rtl><span>first</span><span>second</span></div>
             <table class=t dir=rtl><tr><td>left</td><td>right</td></tr></table>
             <p dir=rtl style='unicode-bidi: bidi-override'><span class=b>box</span> end</p>",
        );
        let mut found = Vec::new();
        texts(&pages[0].fragments[0], 0.0, &mut found);
        let run = |text: &str| {
            let (_, left, right) = found
                .iter()
                .find(|(run, ..)| run == text)
                .unwrap_or_else(|| panic!("{text} in {found:?}"));
            (*left, *right)
        };
        let body = find(&pages[0], &document, "c").expect("flex box");
        let right_edge = body.0 + body.2.rect.width.to_px();
        // Right to left, text starts on the right.
        assert!((run("one").1 - right_edge).abs() < 0.5, "{found:?}");
        // Overridden to right to left, the glyphs come last first; the text stays as it is.
        let ab = run("ab ");
        assert!(run("xy").0 >= ab.1, "{found:?}");
        // The marker is on the right of the item.
        assert!(
            found
                .iter()
                .any(|(text, left, _)| text.contains('\u{2022}') && *left > run("item").1)
        );
        // Flex items and table columns start on the right.
        assert!(run("first").0 >= run("second").1);
        assert!(run("left").0 > run("right").1);
        // Right-to-left text after a box stands left of it.
        let (x, _, fragment) = find(&pages[0], &document, "b").expect("box");
        assert!(x >= run("end ").1 - 0.01, "{found:?}");
        assert!(matches!(fragment.kind, FragmentKind::Box { sides, .. } if sides.left && sides.right));
    }

    #[test]
    fn numbers_list_items_and_places_markers_outside() {
        fn texts(fragment: &Fragment, x: f32, out: &mut Vec<(String, f32)>) {
            let x = x + fragment.rect.x.to_px();
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push((run.text.clone(), x));
            }
            fragment.children.iter().for_each(|child| texts(child, x, out));
        }
        let pages = render(
            "<style>body { font-family: Inter } h2::before { counter-increment: h; content: counter(h, upper-roman) \". \" }</style>
             <ol start=4><li>vier</li><li value=9>neun</li><li><p>zehn</p></li></ol><h2>a</h2><h2>b</h2>",
        );
        let mut found = Vec::new();
        texts(&pages[0].fragments[0], 0.0, &mut found);
        let position = |text: &str| found.iter().find(|(run, _)| run == text).map(|(_, x)| *x);
        let marker = position("4. ").expect("marker of the first item");
        assert!(
            marker < position("vier").expect("item text"),
            "outside, before the text"
        );
        assert!(position("9. ").is_some(), "value attribute");
        assert!(position("10. ").is_some(), "marker in the paragraph's first line");
        // Each heading's `::before` makes a counter of its own, which no reset made, as in
        // Chrome: both show I.
        assert_eq!(
            found.iter().filter(|(run, _)| run == "I. ").count(),
            2,
            "counter() in generated content: {found:?}"
        );
    }

    #[test]
    fn writes_counters_in_the_counter_styles_of_the_document() {
        fn texts(fragment: &Fragment, out: &mut Vec<String>) {
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push(run.text.clone());
            }
            fragment.children.iter().for_each(|child| texts(child, out));
        }
        let pages = render(
            "<style>body { font-family: Inter }
               @counter-style pfeil { system: cyclic; symbols: '→' '⇒'; suffix: ' ' }
               @counter-style abschnitt { system: extends upper-roman; prefix: '§ '; suffix: ': ' }
               h2::before { counter-increment: h; content: counter(h, lower-greek) ') ' }</style>
             <ol style='list-style-type: pfeil'><li>a</li><li>b</li></ol>
             <ol style='list-style-type: abschnitt' start=4><li>c</li></ol>
             <ol style='list-style-type: lower-greek' start=25><li>d</li></ol><h2>e</h2>",
        );
        let mut found = Vec::new();
        texts(&pages[0].fragments[0], &mut found);
        for marker in ["→ ", "⇒ ", "§ IV: ", "αα. ", "α) "] {
            assert!(found.iter().any(|text| text == marker), "{marker:?} in {found:?}");
        }
    }

    #[test]
    fn quotes_in_the_language_around_them() {
        fn texts(fragment: &Fragment, out: &mut String) {
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push_str(&run.text);
            }
            fragment.children.iter().for_each(|child| texts(child, out));
        }
        let pages = render(
            "<style>body { font-family: Inter }</style>
             <p lang=de><q lang=fr>a <q>b <q>c</q></q></q></p><p><q>d</q></p>
             <p style='quotes: \"<\" \">\"'><q>e</q></p><p style='quotes: none'><q>f</q></p>",
        );
        let mut text = String::new();
        texts(&pages[0].fragments[0], &mut text);
        // The language of the parent, as in Chrome, the depth counted through the document.
        assert!(text.contains("„a «b «c»»“"), "{text}");
        assert!(text.contains("“d”") && text.contains("<e>"), "{text}");
        assert!(text.contains('f') && !text.contains("“f"), "{text}");
    }

    #[test]
    fn lays_out_pseudo_elements_as_boxes_of_their_own() {
        // The border boxes of the pseudo-elements on a page: boxes without an element, with a
        // border.
        fn walk(fragment: &Fragment, origin: (f32, f32), found: &mut Vec<[f32; 4]>) {
            let x = origin.0 + fragment.rect.x.to_px();
            let y = origin.1 + fragment.rect.y.to_px();
            if let FragmentKind::Box {
                node: None, style, ..
            } = &fragment.kind
                && style.border_width.left + style.border_width.bottom > 0.0
            {
                found.push([x, y, fragment.rect.width.to_px(), fragment.rect.height.to_px()]);
            }
            fragment
                .children
                .iter()
                .for_each(|child| walk(child, (x, y), found));
        }
        // An empty `::before` positioned absolutely or placed as an inline-block, as drawn check
        // marks are, is a box with its own size, borders and position, unlike an anonymous box.
        // In a flex container, it is an item like the children.
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/20px Inter }
               p { position: relative; margin: 0 }
               .a::before { content: ''; position: absolute; left: 2px; top: 5px; width: 9px; height: 4px;
                            border-left: 2px solid; border-bottom: 2px solid }
               .b::before { content: ''; display: inline-block; width: 9px; height: 4px; border-bottom: 2px solid }
               .c { display: flex; gap: 5px } .c::before { content: 'i'; width: 14px; height: 10px; border-left: 1px solid }</style>
             <p class=a>a</p><p class=b>b</p><div class=c><span class=text>c</span></div>",
        );
        let mut found = Vec::new();
        walk(&pages[0].fragments[0], (0.0, 0.0), &mut found);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0], [2.0, 5.0, 11.0, 6.0]);
        // The inline-block starts the line of the second paragraph.
        let [x, y, width, height] = found[1];
        assert_eq!((x, width, height), (0.0, 9.0, 6.0));
        assert!((20.0..40.0).contains(&y), "{y}");
        assert_eq!(found[2], [0.0, 40.0, 15.0, 10.0]);
        let (x, _, _) = find(&pages[0], &document, "text").expect("item after the pseudo-element");
        assert_eq!(x, 20.0);
    }

    #[test]
    fn gives_lines_of_line_breaks_their_height() {
        // A line that ends with a forced break is not empty (CSS 2.1, §9.4.2): every `<br>` after
        // another, or between blocks, makes a line of the line height, as in browsers.
        let (document, pages) = render_document(
            "<style>@page { size: 300px 300px; margin: 0 } body { margin: 0; font: 10px/20px Inter }
               p { margin: 0 }</style>
             <p class=a>a</p><br><br><p class=b>b</p><p class=c>x<br><br>y</p><br><p class=d>d</p>",
        );
        let place = |class: &str| {
            let (_, y, fragment) = find(&pages[0], &document, class).expect("paragraph");
            (y, fragment.rect.height.to_px())
        };
        assert_eq!(place("b"), (60.0, 20.0));
        assert_eq!(place("c"), (80.0, 60.0));
        assert_eq!(place("d"), (160.0, 20.0));
    }

    #[test]
    fn places_list_symbols_like_browsers() {
        // The center of the second marker glyph on a page.
        fn walk(fragment: &Fragment, origin: (f32, f32), found: &mut Option<f32>) {
            let x = origin.0 + fragment.rect.x.to_px();
            let y = origin.1 + fragment.rect.y.to_px();
            if let FragmentKind::Text(run) = &fragment.kind
                && run.marker
                && y > 20.0
            {
                *found = Some(x + fragment.rect.width.to_px() / 2.0);
            }
            for child in &fragment.children {
                walk(child, (x, y), found);
            }
        }
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 20px/1.4 Inter }
               ul { margin: 0; padding: 0 0 0 100px }</style>
             <ul style='list-style-position: inside'><li><span class=inside>X</span></li></ul>
             <ul><li><span class=outside>X</span></li></ul>",
        );
        // Browsers draw the bullet of 20px Inter (rounded ascent 19) six pixels wide, a pixel
        // into a box two pixels wider; inside, a margin of 1em follows that box.
        let (x, _, _) = find(&pages[0], &document, "inside").expect("inside marker");
        assert_eq!(x, 100.0 - 1.0 + 8.0 + 20.0);
        // Outside, the bullet is 12 + 7 pixels before the text: its glyph is centered there.
        let page = &pages[0];
        let mut bullet = None;
        for fragment in &page.fragments {
            walk(fragment, (0.0, 0.0), &mut bullet);
        }
        assert_eq!(bullet, Some(100.0 - 19.0 + 3.0));
        assert_eq!(find(page, &document, "outside").map(|(x, _, _)| x), Some(100.0));
    }

    #[test]
    fn draws_list_symbols_as_shapes_of_blinks_size() {
        fn symbols(fragment: &Fragment, found: &mut Vec<(f32, MarkerSymbol)>) {
            if let FragmentKind::Text(run) = &fragment.kind
                && let Some(symbol) = run.symbol
            {
                found.push((fragment.rect.width.to_px(), symbol));
            }
            for child in &fragment.children {
                symbols(child, found);
            }
        }
        let (_, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 20px/1.4 Inter }</style>
             <ul><li>disc</li><li style='list-style-type: disclosure-open'>open</li></ul>",
        );
        let mut found = Vec::new();
        for fragment in &pages[0].fragments {
            symbols(fragment, &mut found);
        }
        // 20px Inter has a rounded ascent of 19: a bullet of 6 pixels, 10 below the top of a
        // box 8 wide; a triangle of 0.66em in layout units, on the baseline.
        let symbol = |shape, x, y, size| MarkerSymbol { shape, x, y, size };
        assert_eq!(
            found,
            [
                (8.0, symbol(SymbolShape::Disc, 1.0, 10.0, 6.0)),
                (
                    13.1875,
                    symbol(
                        SymbolShape::Disclosure { open: true },
                        0.0,
                        19.0 - 13.1875,
                        13.1875
                    )
                ),
            ]
        );
    }

    #[test]
    fn places_spanning_cells_and_repeats_footers() {
        let rows = "<tr><td>Position</td><td>1</td><td>95,00 €</td></tr>".repeat(60);
        let (document, pages) = render_document(&format!(
            "<style>body {{ font-family: Inter }} table {{ border-collapse: collapse; width: 300px }}
             td, th {{ padding: 6px; border: 1px solid }}</style>
             <table><thead><tr><th rowspan=2 class=head>A</th><th colspan=2 class=wide>B</th></tr>
             <tr><th class=c>C</th><th class=d>D</th></tr></thead>
             <tbody>{rows}</tbody><tfoot><tr><td colspan=3>Übertrag</td></tr></tfoot></table>"
        ));
        assert!(pages.len() >= 2);
        let (head_x, head_y, head) = find(&pages[0], &document, "head").expect("rowspan cell");
        let (wide_x, _, wide) = find(&pages[0], &document, "wide").expect("colspan cell");
        let (c_x, c_y, _) = find(&pages[0], &document, "c").expect("cell under the span");
        let (d_x, _, d) = find(&pages[0], &document, "d").expect("cell under the span");
        assert!(c_y > head_y, "second header row below the first");
        assert!(
            head.rect.height.to_px() > wide.rect.height.to_px() * 1.5,
            "rowspan covers two rows"
        );
        assert!(
            (c_x - wide_x).abs() < 1.0 && c_x > head_x,
            "C starts under B, right of A"
        );
        let span_right = wide_x + wide.rect.width.to_px();
        let d_right = d_x + d.rect.width.to_px();
        assert!((span_right - d_right).abs() < 1.0, "B covers C and D");
        for page in &pages {
            assert!(
                contains_text(&page.fragments[0], "Übertrag"),
                "the footer closes every page"
            );
        }
    }

    #[test]
    fn aligns_cell_content_in_rows_their_height_sets() {
        let (document, pages) = render_document(
            "<!DOCTYPE html><style>@page { margin: 0 } body { margin: 0; font: 10px/10px Inter }
               table { border-spacing: 0 } td { padding: 0 }</style>
             <table><tr><td class=tall style='height: 60px'><span class=middle>m</span></td>
               <td style='vertical-align: bottom'><span class=bottom>b</span></td></tr></table>",
        );
        // A cell's height sets that of its row, in which its content is aligned: in the middle
        // by default (from the row group), at the bottom here.
        let (_, _, tall) = find(&pages[0], &document, "tall").expect("tall cell");
        assert_eq!(tall.rect.height.to_px(), 60.0);
        // The spans sit a little above their lines of 10px (their fonts are taller).
        let (_, middle, _) = find(&pages[0], &document, "middle").expect("middle");
        let (_, bottom, _) = find(&pages[0], &document, "bottom").expect("bottom");
        assert!((middle - 25.0).abs() < 2.0, "{middle}");
        assert_eq!(bottom - middle, 25.0);
    }

    #[test]
    fn keeps_border_spacing_in_whole_pixels() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 10px/10px Inter } td { padding: 0 }</style>
             <table style='border-spacing: 2mm'><tr><td class=spaced>s</td></tr></table>",
        );
        // As in browsers: 2mm come out 7 pixels, not 7.56.
        let (x, y, _) = find(&pages[0], &document, "spaced").expect("spaced cell");
        assert_eq!((x, y), (7.0, 7.0));
    }

    #[test]
    fn widens_fixed_cells_to_their_content() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 10px/10px Inter } table { width: 300px; border-collapse: collapse }
               td { padding: 0 } .narrow { width: 5px }</style>
             <table><tr><td class=narrow>Breite</td><td>x</td></tr></table>",
        );
        // A cell is at least as wide as its content, even with a smaller fixed width, as in
        // browsers: its text does not run into the next column.
        let (_, _, cell) = find(&pages[0], &document, "narrow").expect("cell");
        let mut texts = Vec::new();
        line_texts(cell, &mut texts);
        assert_eq!(texts, ["Breite"]);
        assert!(cell.rect.width.to_px() > 25.0, "{:?}", cell.rect.width);
    }

    #[test]
    fn fills_pages_with_rows_before_the_bottom_of_a_table() {
        use std::fmt::Write;
        let mut rows = String::new();
        for row in 1..=15 {
            write!(rows, "<tr><td class=r{row}>{row}</td></tr>").expect("written");
        }
        let (document, pages) = render_document(&format!(
            "<!DOCTYPE html><style>@page {{ size: 300px 100px; margin: 0 }} body {{ margin: 0; font: 10px/10px Inter }}
               table {{ border-spacing: 0; border-bottom: 4px solid }} td {{ padding: 0 }}</style>
             <table>{rows}</table>"
        ));
        // Ten rows fill the first page: the table's bottom border needs room only on its last.
        assert_eq!(pages.len(), 2);
        assert!(find(&pages[0], &document, "r10").is_some());
        assert_eq!(find(&pages[1], &document, "r11").map(|(_, y, _)| y), Some(0.0));
    }

    #[test]
    fn breaks_tables_below_their_captions_and_shrinks_the_last_spacing() {
        let (document, pages) = render_document(
            "<!DOCTYPE html><style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               table { border: 2px solid; border-spacing: 2px } td { padding: 0 } p { margin: 0 }</style>
             <p style='height: 75px'></p>
             <table class=first><caption class=caption>Caption</caption><tr><td class=cell>1</td></tr></table>
             <p style='break-before: page; height: 84px'></p>
             <table class=second><tr><td>2</td></tr></table>
             <p style='break-before: page; height: 88px'></p>
             <table><caption class=late>Caption</caption><tr><td>3</td></tr></table>",
        );
        // The rows do not fit below the caption: the caption stays, the table box moves, as in
        // Chrome.
        assert!(find(&pages[0], &document, "caption").is_some());
        assert!(find(&pages[0], &document, "cell").is_none());
        assert_eq!(find(&pages[1], &document, "cell").map(|(_, y, _)| y), Some(4.0));
        // The spacing below the last row shrinks to let the table end on the page.
        let (_, y, table) = find(&pages[2], &document, "second").expect("table on the page");
        assert_eq!((y, table.rect.height.to_px()), (84.0, 16.0));
        // Without room for the table's top border and spacing below the caption, the caption
        // moves along.
        assert!(find(&pages[3], &document, "late").is_none());
        assert_eq!(find(&pages[4], &document, "late").map(|(_, y, _)| y), Some(0.0));
    }

    #[test]
    fn leaves_the_top_of_a_column_flex_container_whose_first_item_moves() {
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p { margin: 0; height: 80px } .flex { display: flex; flex-direction: column }</style>
             <p></p><div class='flex top' style='border-top: 3px solid'><div class=a style='line-height: 30px'>a</div></div>
             <p style='break-before: page'></p><div class='flex plain'><div class=b style='line-height: 30px'>b</div></div>",
        );
        // The border separates the item from the container's top, which stays on the page.
        assert!(find(&pages[0], &document, "top").is_some());
        assert_eq!(find(&pages[1], &document, "a").map(|(_, y, _)| y), Some(0.0));
        // Without it, the container moves whole.
        assert!(find(&pages[2], &document, "plain").is_none());
        assert_eq!(find(&pages[3], &document, "plain").map(|(_, y, _)| y), Some(0.0));
    }

    #[test]
    fn moves_boxes_with_size_containment_whole() {
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p { margin: 0; height: 80px } .box { height: 40px }</style>
             <p></p><div class=contained style='contain: size; height: 40px'>a</div>",
        );
        assert!(find(&pages[0], &document, "contained").is_none());
        assert_eq!(
            find(&pages[1], &document, "contained").map(|(_, y, _)| y),
            Some(0.0)
        );
    }

    #[test]
    fn balances_columns_of_floats_with_orphans_and_widows() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 10px/10px Inter }</style>
             <div class=float style='float: left; width: 300px; columns: 3; column-gap: 0'><div>a<br>b</div></div>",
        );
        // Two lines cannot break into columns of one line each without an orphan and a widow.
        let (_, _, float) = find(&pages[0], &document, "float").expect("float");
        assert_eq!(float.rect.height.to_px(), 20.0);
    }

    #[test]
    fn sizes_columns_for_grid_items_with_an_aspect_ratio_in_fixed_rows() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 10px/10px Inter }</style>
             <div style='display: grid; grid-template-columns: auto 1fr; grid-template-rows: 40px; width: 300px'>
             <div class=ratio style='aspect-ratio: 2'></div><div>x</div></div>
             <table style='table-layout: fixed; width: 300px; border-spacing: 0'><col style='width: 100%'>
             <tr><td>a</td><td class=narrow style='padding: 5px'>b</td></tr></table>",
        );
        let (_, _, ratio) = find(&pages[0], &document, "ratio").expect("item");
        assert_eq!(
            (ratio.rect.width.to_px(), ratio.rect.height.to_px()),
            (80.0, 40.0)
        );
        // A cell of a fixed layout narrower than its padding is as wide as its column.
        let (_, _, narrow) = find(&pages[0], &document, "narrow").expect("cell");
        assert_eq!(narrow.rect.width.to_px(), 0.0);
    }

    #[test]
    fn rounds_lengths_like_browsers() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 8.5pt/1.45 Inter } p { margin: 0 }
               div { padding-top: 2.5mm }</style>
             <p>1</p><p>2</p><p class=third>3</p><div class=box></div>",
        );
        // Lengths are cut to whole units of 1/64 pixel as in Chrome, where 100 lines of 8.5pt type
        // at 1.45 come out 1.6 pixels shorter than rounded ones, enough to change a page break.
        let (_, y, _) = find(&pages[0], &document, "third").expect("third line");
        assert_eq!(y, 2.0 * 1051.0 / 64.0);
        let (_, _, padded) = find(&pages[0], &document, "box").expect("padded box");
        assert_eq!(padded.rect.height.to_px(), 604.0 / 64.0);
    }

    #[test]
    fn gives_percentage_columns_their_share() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } table { width: 400px; border-spacing: 0 }
             td { width: 25%; padding: 0 }</style>
             <table><tr><td class=a>Ein langer Text in der ersten Spalte</td><td class=b>b</td>
             <td>c</td><td>d</td></tr></table>",
        );
        let (_, _, a) = find(&pages[0], &document, "a").expect("cell");
        let (_, _, b) = find(&pages[0], &document, "b").expect("cell");
        assert_eq!(a.rect.width.to_px(), 100.0);
        assert_eq!(b.rect.width.to_px(), 100.0);
    }

    #[test]
    fn repeats_table_headers_on_every_page() {
        let rows = "<tr><td>Position</td><td>95,00 €</td></tr>".repeat(80);
        let pages = render(&format!(
            "<style>body {{ font-family: Inter }} td, th {{ padding: 8px }}</style>
             <table><thead><tr><th>Beschreibung</th><th>Betrag</th></tr></thead><tbody>{rows}</tbody></table>"
        ));
        assert!(pages.len() >= 2);
        for page in &pages {
            assert!(
                contains_text(&page.fragments[0], "Beschreibung"),
                "the header is repeated on every page"
            );
        }
    }

    #[test]
    fn repeats_only_table_headers_that_avoid_breaks() {
        let rows = "<div style='display: table-row'><div style='display: table-cell'>Position</div></div>"
            .repeat(80);
        let pages = render(&format!(
            "<style>body {{ font-family: Inter }}</style>
             <div><div style='display: table-header-group'><div style='display: table-cell'>Kopf</div></div>{rows}</div>"
        ));
        assert!(pages.len() >= 2);
        assert!(contains_text(&pages[0].fragments[0], "Kopf"));
        assert!(
            !contains_text(&pages[1].fragments[0], "Kopf"),
            "a header group that may break inside comes once, as in Chrome"
        );
    }

    #[test]
    fn widens_auto_tables_for_percentage_columns() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } td { padding: 0 }</style>
             <table style='border-spacing: 0'><tr><td class=a style='width: 50%'>AAAAAAAA</td><td class=b>B</td></tr>
             </table><table style='border-spacing: 0'><tr><td class=c style='width: 30%'>short</td>
             <td class=d>other text here</td></tr></table>",
        );
        let width = |class: &str| {
            find(&pages[0], &document, class)
                .expect(class)
                .2
                .rect
                .width
                .to_px()
        };
        // A column of 50 % takes half of the table, which grows to give it its content's width.
        assert!(
            (width("a") - width("b")).abs() <= 1.0,
            "{} {}",
            width("a"),
            width("b")
        );
        // The other columns keep their max-content widths: nothing wraps.
        let (_, _, d) = find(&pages[0], &document, "d").expect("cell");
        assert_eq!(lines(d), 1);
    }

    #[test]
    fn draws_column_backgrounds_in_the_cells_that_start_in_them() {
        /// The boxes drawn with a background, as (x, y, width, height, clipped child).
        fn painted(fragment: &Fragment, origin: (f32, f32), out: &mut Vec<(f32, f32, f32, f32, bool)>) {
            let x = origin.0 + fragment.rect.x.to_px();
            let y = origin.1 + fragment.rect.y.to_px();
            if let FragmentKind::Box {
                style, node: None, ..
            } = &fragment.kind
                && (!style.background_color.is_transparent()
                    || style.overflow_x == fagus_style::values::Overflow::Hidden)
            {
                let rect = (x, y, fragment.rect.width.to_px(), fragment.rect.height.to_px());
                out.push((rect.0, rect.1, rect.2, rect.3, !fragment.children.is_empty()));
            }
            for child in &fragment.children {
                painted(child, (x, y), out);
            }
        }
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } td { padding: 0; width: 40px; height: 20px }</style>
             <table style='border-spacing: 10px'>
               <col style='background: rgb(0, 0, 255)'>
               <col style='background: linear-gradient(red, blue)'>
               <tr><td class=a></td><td class=b></td></tr><tr><td class=c></td><td class=d></td></tr>
             </table>",
        );
        let mut boxes = Vec::new();
        for fragment in &pages[0].fragments {
            painted(fragment, (0.0, 0.0), &mut boxes);
        }
        let cell = |class: &str| {
            let (x, y, fragment) = find(&pages[0], &document, class).expect(class);
            (x, y, fragment.rect.width.to_px(), fragment.rect.height.to_px())
        };
        // A color fills the cells of its column.
        for class in ["a", "c"] {
            let (x, y, width, height) = cell(class);
            assert!(
                boxes.contains(&(x, y, width, height, false)),
                "{class}: {boxes:?}"
            );
        }
        // An image is drawn in a box as tall as the column, clipped to each cell.
        for class in ["b", "d"] {
            let (x, y, width, height) = cell(class);
            assert!(boxes.contains(&(x, y, width, height, true)), "{class}: {boxes:?}");
        }
    }

    #[test]
    fn makes_tables_at_least_as_wide_as_their_captions() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter } td { padding: 0 }</style>
             <table style='border-spacing: 0'><caption class=caption>A caption longer than the cells</caption>
             <tr><td class=cell>x</td></tr></table>",
        );
        let width = |class: &str| {
            find(&pages[0], &document, class)
                .expect(class)
                .2
                .rect
                .width
                .to_px()
        };
        assert!(
            width("cell") >= width("caption") - 1.0,
            "{} {}",
            width("cell"),
            width("caption")
        );
    }

    #[test]
    fn aligns_cells_without_lines_at_their_top() {
        let (document, pages) = render_document(
            "<style>body { font-family: Inter; line-height: 20px }</style>
             <div class=wrap style='display: table'><div style='display: table-cell'>Text</div><div class=float style='float: left; width: 10px; height: 10px'></div></div>",
        );
        let (_, y, _) = find(&pages[0], &document, "float").expect("float");
        let (_, top, _) = find(&pages[0], &document, "wrap").expect("wrapper");
        // The float's anonymous cell has no line to align on the row's baseline.
        assert!((y - top).abs() < 0.5, "{y} {top}");
    }

    #[test]
    fn sets_tabs_to_tab_stops() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 10px/10px Inter } pre { font: inherit; margin: 0 }</style>
             <pre class=a style='tab-size: 40px'>A\tB\tC</pre>
             <pre class=b style='tab-size: 40px'>ABCDEFGHIJKLMNOP\tQ</pre>",
        );
        // A tab goes on to the next multiple of the tab size, whatever the text before it.
        let (_, _, pre) = find(&pages[0], &document, "a").expect("pre");
        assert_eq!(glyph_x(pre, 'B'), Some(40.0));
        assert_eq!(glyph_x(pre, 'C'), Some(80.0));
        let (_, _, wide) = find(&pages[0], &document, "b").expect("pre");
        let q = glyph_x(wide, 'Q').expect("Q");
        assert!(q % 40.0 == 0.0 && q > 0.0, "{q}");
    }

    #[test]
    fn aligns_last_lines_by_text_align_last() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 10px/10px Inter } p { width: 200px; margin: 0; text-align: justify }</style>
             <p class=c style='text-align-last: center'>Kurz</p>
             <p class=r style='text-align-last: right'>Kurz</p>
             <p class=s>Kurz</p>",
        );
        let start = |class: &str| {
            let (_, _, paragraph) = find(&pages[0], &document, class).expect("p");
            first_text_x(paragraph).expect("text")
        };
        assert_eq!(start("s"), 0.0);
        assert!(start("c") > 50.0 && start("c") < 100.0);
        assert!(start("r") > 150.0);
    }

    #[test]
    fn hides_empty_cells() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 10px/10px Inter } td { border: 1px solid; width: 20px }</style>
             <table style='empty-cells: hide'><tr><td class=a>A</td><td class=empty></td><td class=space> </td></tr></table>
             <table><tr><td class=shown></td></tr></table>",
        );
        assert!(find(&pages[0], &document, "a").is_some());
        assert!(find(&pages[0], &document, "empty").is_none());
        assert!(find(&pages[0], &document, "space").is_none());
        assert!(find(&pages[0], &document, "shown").is_some());
    }

    #[test]
    fn ends_overflowing_lines_with_an_ellipsis() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 14px/20px Inter }
               div { overflow: hidden; white-space: nowrap; text-overflow: ellipsis }</style>
             <div class=a style='width: 150px'>Beratungsleistung für die Umstellung</div>
             <div class=b style='width: 120px; text-align: right'>Ein sehr langer Text, der nicht passt</div>
             <div class=c style='width: 12px'>Abc</div>
             <div class=d style='width: 300px'>Kurz</div>
             <div class=e style='width: 150px; overflow: visible'>Ohne overflow hidden kein Zeichen</div>",
        );
        let line = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect("box");
            let mut lines = Vec::new();
            line_texts(fragment, &mut lines);
            lines.join(" ")
        };
        // Cut at the end of a character, as in Chrome.
        assert_eq!(line("a"), "Beratungsleistung fü…");
        // A cut line starts at the start, whatever its alignment, and keeps one character.
        assert_eq!(line("b"), "Ein sehr langer …");
        let (_, _, right) = find(&pages[0], &document, "b").expect("box");
        assert_eq!(first_text_x(right), Some(0.0));
        assert_eq!(line("c"), "A…");
        assert_eq!(line("d"), "Kurz");
        assert_eq!(line("e"), "Ohne overflow hidden kein Zeichen");
    }

    #[test]
    fn places_ellipses_and_strings_as_blink() {
        fn tops(fragment: &Fragment, y: f32, out: &mut Vec<(String, f32)>) {
            let y = y + fragment.rect.y.to_px();
            if let FragmentKind::Text(run) = &fragment.kind {
                out.push((run.text.trim().to_owned(), y));
            }
            for child in &fragment.children {
                tops(child, y, out);
            }
        }
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 14px/20px Inter }
               div { overflow: hidden; white-space: nowrap; text-overflow: ellipsis; width: 60px }
               i { display: inline-block; width: 100px; height: 10px }</style>
             <div class=a><i></i>xxx</div>
             <div class=b><span style='margin-left: 5px'></span><i></i>xxx</div>
             <div class=c>ab <span style='vertical-align: 10px'>cdefghijklmnop</span></div>
             <div class=d style='text-overflow: \"[\\A]\"'>Beratungsleistung</div>",
        );
        let line = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect("box");
            let mut lines = Vec::new();
            line_texts(fragment, &mut lines);
            lines.join(" ")
        };
        // The first atomic inline of a line stays, the ellipsis after it.
        assert_eq!(line("a"), "\u{2026}");
        // After an empty inline box, it is not the first and goes, and no ellipsis follows.
        assert_eq!(line("b"), "");
        // The ellipsis stands on the line, outside the raised box it is cut in.
        let (_, _, raised) = find(&pages[0], &document, "c").expect("box");
        let mut runs = Vec::new();
        tops(raised, 0.0, &mut runs);
        let top = |text: &str| runs.iter().find(|run| run.0 == text).map(|run| run.1);
        assert_eq!(top("\u{2026}"), top("ab"), "{runs:?}");
        // A string, its line break made a space.
        assert!(line("d").ends_with("[ ]"), "{}", line("d"));
    }

    #[test]
    fn keeps_the_content_out_of_fixed_table_layouts() {
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 14px/20px Inter }
               table { table-layout: fixed; width: 300px; border-collapse: collapse }
               td { border: 1px solid; padding: 4px }</style>
             <table class=t><tr><td style='width: 80px'>Pos. 1</td>
             <td style='white-space: nowrap'>Lizenz für die Software Fagus Enterprise in allen Ausbaustufen</td></tr></table>",
        );
        let (_, _, table) = find(&pages[0], &document, "t").expect("table");
        assert_eq!(table.rect.width.to_px(), 300.0);
    }

    #[test]
    fn carries_running_totals_over_page_breaks() {
        use std::fmt::Write;
        // The running total after each row, as the template's code computes it.
        let mut rows = String::new();
        for n in 1..=30 {
            let total = n * (n + 1) / 2;
            write!(
                rows,
                "<tr data-total=\"{total}\"><td>Position {n}</td><td>{n}</td></tr>"
            )
            .expect("written");
        }
        let pages = render(&format!(
            "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 10px/20px Inter }}
               table {{ border-spacing: 0 }} td, th {{ padding: 0 }}
               tbody tr {{ string-set: total attr(data-total) }}
               .carry {{ -fagus-repeat: at-break }}
               .carry td + td::after, .sum td + td::after {{ content: string(total) }}
               .from::before {{ content: \"Übertrag von Seite \" counter(-fagus-previous-page) }}
               .to::before {{ content: \"Weiter auf Seite \" counter(-fagus-next-page) }}</style>
             <table><thead><tr><th>Beschreibung</th><th>Betrag</th></tr>
             <tr class=carry><td class=from></td><td></td></tr></thead>
             <tfoot><tr class=carry><td class=to></td><td></td></tr>
             <tr class=sum><td>Summe</td><td></td></tr></tfoot>
             <tbody>{rows}</tbody></table>"
        ));
        let texts: Vec<Vec<String>> = pages
            .iter()
            .map(|page| {
                let mut out = Vec::new();
                line_texts(&page.fragments[0], &mut out);
                out
            })
            .collect();
        // 15 rows of 20px fit on a page. The first page has no carry-over row in its header and
        // room for 12 positions above the footer's two rows; the total after position 12 is 78.
        assert_eq!(texts[0][..2], ["Beschreibung", "Betrag"]);
        assert_eq!(texts[0][2], "Position 1");
        assert_eq!(
            texts[0][texts[0].len() - 6..],
            ["Position 12", "12", "Weiter auf Seite 2", "78", "Summe", "78"]
        );
        // The next page carries 78 over and holds 11 positions, up to a total of 23 · 24 / 2.
        assert_eq!(
            texts[1][..6],
            [
                "Beschreibung",
                "Betrag",
                "Übertrag von Seite 1",
                "78",
                "Position 13",
                "13"
            ]
        );
        assert_eq!(
            texts[1][texts[1].len() - 4..],
            ["Weiter auf Seite 3", "276", "Summe", "276"]
        );
        // The last page ends without a carry-over row, with the final total.
        assert_eq!(pages.len(), 3);
        assert_eq!(texts[2][2..4], ["Übertrag von Seite 2", "276"]);
        assert_eq!(
            texts[2][texts[2].len() - 4..],
            ["Position 30", "30", "Summe", "465"]
        );
    }

    #[test]
    fn assigns_named_strings_and_counters_of_table_rows() {
        use std::fmt::Write;
        let mut rows = String::new();
        for n in 1..=12 {
            write!(rows, "<tr data-page-total=\"{n}\"><td></td><td>Zeile</td></tr>").expect("written");
        }
        let (document, pages) = render_document(&format!(
            "<style>@page {{ size: 300px 120px; margin: 20px 0;
                 @bottom-left {{ content: \"Stand \" string(total, last) }} }}
               body {{ margin: 0; font: 10px/20px Inter }} table {{ border-spacing: 0 }} td {{ padding: 0 }}
               tbody {{ counter-reset: position }}
               tr {{ counter-increment: position; string-set: total attr(data-page-total) }}
               td:first-child::before {{ content: counter(position) \".\" }}</style>
             <table><tbody>{rows}</tbody></table>"
        ));
        assert_eq!(pages.len(), 3);
        // The rows count themselves, and the last row on a page sets the margin box's string.
        let mut first = Vec::new();
        line_texts(&pages[0].fragments[0], &mut first);
        assert_eq!(first[..4], ["1.", "Zeile", "2.", "Zeile"]);
        assert_eq!(margin_text(&pages[0]), "Stand 4|");
        assert_eq!(margin_text(&pages[2]), "Stand 12|");
        let _ = document;
    }

    #[test]
    fn lays_out_columns() {
        let (document, pages) = render_document(
            "<style>@page { size: 500px 800px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               .m { columns: 3; column-gap: 20px; column-rule: 2px solid red; width: 340px }
               p { margin: 0; height: 30px }</style>
             <div class=m><p class=a>1</p><p>2</p><p>3</p><p>4</p><p>5</p><p class=f>6</p></div>",
        );
        assert_eq!(pages.len(), 1);
        let (_, _, multicol) = find(&pages[0], &document, "m").expect("container");
        // Three columns of 100px, 20px apart, balanced at two paragraphs each.
        assert_eq!(multicol.rect.height.to_px(), 60.0);
        let columns: Vec<f32> = multicol
            .children
            .iter()
            .filter(|child| matches!(child.kind, FragmentKind::Anonymous))
            .map(|column| column.rect.x.to_px())
            .collect();
        assert_eq!(columns, [0.0, 120.0, 240.0]);
        let rules = multicol
            .children
            .iter()
            .filter(|child| matches!(child.kind, FragmentKind::Box { node: None, .. }))
            .count();
        assert_eq!(rules, 2);
        let (x, y, _) = find(&pages[0], &document, "f").expect("last paragraph");
        assert_eq!((x, y), (240.0, 30.0));
    }

    #[test]
    fn continues_columns_on_the_next_page() {
        let (document, pages) = render_document(
            "<style>@page { size: 500px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               .m { column-count: 2; column-gap: 0; column-fill: auto } p { margin: 0; line-height: 40px }</style>
             <div class=m><p>1</p><p>2</p><p>3</p><p>4</p><p>5</p><p>6</p><p class=last>7</p></div>",
        );
        // Two columns of two lines fill a page; seven lines take two pages.
        assert_eq!(pages.len(), 2);
        let (x, y, _) = find(&pages[1], &document, "last").expect("on the second page");
        assert_eq!((x, y), (250.0, 0.0));
    }

    #[test]
    fn balances_columns_that_hold_more_than_their_height() {
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               .m { column-count: 2; column-gap: 0 } p { margin: 0; height: 70px } .second { margin-top: 61px }</style>
             <div class=m><p>1</p><p class=second>2</p></div>",
        );
        // In one column, the paragraphs take more than both columns on the page (201px), but the
        // margin between them is truncated at the break between the columns.
        assert_eq!(pages.len(), 1);
        let (x, y, _) = find(&pages[0], &document, "second").expect("second paragraph");
        assert_eq!((x, y), (150.0, 0.0));
    }

    #[test]
    fn keeps_margins_after_forced_column_breaks() {
        let (document, pages) = render_document(
            "<style>@page { size: 300px 200px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p { margin: 0 } .m { columns: 3; column-gap: 0; column-fill: auto; height: 100px }</style>
             <div class=m><p>a</p><p class=forced style='break-before: column; margin-top: 30px'>forced</p>
               <div style='height: 85px'></div><p class=unforced style='margin-top: 30px'>unforced</p></div>",
        );
        let (x, y, _) = find(&pages[0], &document, "forced").expect("second column");
        assert_eq!((x, y), (100.0, 30.0));
        let (x, y, _) = find(&pages[0], &document, "unforced").expect("third column");
        assert_eq!((x, y), (200.0, 55.0));
    }

    #[test]
    fn cuts_the_last_margin_at_the_end_of_the_columns() {
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0; font: 10px/10px Inter }
               .m { columns: 2; column-gap: 0 } p { margin: 0; height: 40px; break-inside: avoid }</style>
             <div class=m><p style='margin-bottom: 10px'>1</p><p style='margin-bottom: 30px'>2</p></div>
             <div class=after></div>",
        );
        // The paragraphs balance at half their 120px with margins; the margin below the second
        // reaches out of its column and is cut there, as in Chrome.
        let (_, y, _) = find(&pages[0], &document, "after").expect("box after the columns");
        assert_eq!(y, 60.0);
    }

    #[test]
    fn spans_columns() {
        let (document, pages) = render_document(
            "<style>@page { size: 500px 800px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               .m { columns: 3; column-gap: 20px; width: 340px; margin-bottom: 20px } p { margin: 0; height: 30px }
               h2 { column-span: all; margin: 10px 0 15px; height: 20px; font-size: 10px }
               section { border: 3px solid; padding: 4px; margin: 5px 0 }</style>
             <div class=m><p>1</p><p>2</p><p>3</p><p>4</p><h2 class=a>A</h2><p class=after-a>5</p><p>6</p></div>
             <div class=m><section><p>1</p><p>2</p><p>3</p><h2 class=b>B</h2><p class=after-b>4</p></section></div>",
        );
        // The paragraphs before a spanner are balanced in a row of columns above it, those after
        // it in a row below it.
        let (x, y, spanner) = find(&pages[0], &document, "a").expect("spanner");
        assert_eq!((x, y, spanner.rect.width.to_px()), (0.0, 50.0, 340.0));
        let (x, y, _) = find(&pages[0], &document, "after-a").expect("paragraph below");
        assert_eq!((x, y), (0.0, 85.0));
        // A spanner splits the section it is in, which goes on below it without its top border
        // and padding. The first row holds the section's top (12px) and three paragraphs.
        let (x, y, _) = find(&pages[0], &document, "b").expect("spanner in a section");
        assert_eq!((x, y), (0.0, 125.0 + 34.0 + 10.0));
        let (x, y, _) = find(&pages[0], &document, "after-b").expect("paragraph below");
        assert_eq!((x, y), (7.0, 169.0 + 20.0 + 15.0));
    }

    #[test]
    fn keeps_margins_next_to_spanners() {
        let (document, pages) = render_document(
            "<style>@page { size: 500px 900px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               .m { columns: 1; margin-bottom: 40px } p { margin: 0; height: 30px }
               h2 { column-span: all; margin: 11px 0 5px; height: 20px; font-size: 10px }</style>
             <div class=m><p>1</p><p style='margin-bottom: 13px'>2</p><h2 class=g>G</h2><p>3</p></div>
             <div class=m><section><p>1</p><p style='margin-bottom: 13px'>2</p><h2 class=h>H</h2>
               <p class=after-h style='margin-top: 17px'>3</p></section></div>",
        );
        // The margin above a spanner stays in the columns when the spanner is a child of the
        // multi-column container's content, and is truncated as at a break inside a box that it
        // splits. The margin of the content below a spanner stays (as in browsers).
        let (_, y, _) = find(&pages[0], &document, "g").expect("spanner");
        assert_eq!(y, 60.0 + 13.0 + 11.0);
        let (_, y, _) = find(&pages[0], &document, "h").expect("spanner in a section");
        assert_eq!(y, 179.0 + 60.0 + 11.0);
        let (_, y, _) = find(&pages[0], &document, "after-h").expect("paragraph below");
        assert_eq!(y, 250.0 + 20.0 + 5.0 + 17.0);
    }

    #[test]
    fn breaks_boxes_before_their_bottom_border_and_padding() {
        let paragraphs = |count: usize| {
            let mut html = String::new();
            for line in 1..=count {
                html.push_str("<p>");
                html.push_str(&line.to_string());
                html.push_str("</p>");
            }
            html
        };
        let render_box = |before: &str, content: &str, display: &str| {
            render_document(&format!(
                "<style>@page {{ size: 300px 100px; margin: 0 }} body {{ margin: 0; font: 10px/10px Inter }}
                   p {{ margin: 0 }} .box {{ border-bottom: 5px solid; padding-bottom: 15px; {display} }}</style>
                 {before}<div class=box>{content}</div><p class=after>after</p>"
            ))
        };
        // Lines fill the page while the box goes on, and the last one moves to the next page
        // with the bottom border and padding that do not fit below it; in column flex
        // containers too.
        for display in ["", "display: flex; flex-direction: column"] {
            for (count, first_page) in [(9, 8), (14, 10)] {
                let (document, pages) = render_box("", &paragraphs(count), display);
                assert_eq!(pages.len(), 2);
                let (_, _, part) = find(&pages[0], &document, "box").expect("first part");
                assert_eq!(lines(part), first_page, "{count} lines, {display}");
                // A box broken inside its content extends to the end of the page.
                assert_eq!(part.rect.height.to_px(), 100.0);
                let (_, y, _) = find(&pages[1], &document, "after").expect("after the box");
                assert_eq!(y, ((count - first_page) * 10 + 20) as f32);
            }
        }
        // Without a breakpoint before the end of the content (the empty space of a box taller
        // than its content is none), the border and padding move on their own at the top of a
        // page, and the whole box moves to the next page below other content.
        let (document, pages) = render_box("", "<div style='height: 85px'>tall</div>", "");
        let (_, _, part) = find(&pages[0], &document, "box").expect("first part");
        assert_eq!(part.rect.height.to_px(), 85.0);
        let (_, y, _) = find(&pages[1], &document, "after").expect("after the box");
        assert_eq!(y, 20.0);
        let (document, pages) = render_box(
            "<div style='height: 70px'></div>",
            "<div style='height: 25px'>t</div>",
            "",
        );
        assert!(find(&pages[0], &document, "box").is_none());
        let (_, y, _) = find(&pages[1], &document, "after").expect("after the whole box");
        assert_eq!(y, 45.0);
    }

    #[test]
    fn sizes_broken_boxes_as_a_whole() {
        let paragraphs = "<p>line</p>".repeat(14);
        let render_box = |sizing: &str, display: &str| {
            render_document(&format!(
                "<style>@page {{ size: 300px 100px; margin: 0 }} body {{ margin: 0; font: 10px/10px Inter }}
                   p {{ margin: 0 }} .box {{ position: relative; padding-bottom: 10px; {sizing}; {display} }}
                   .foot {{ position: absolute; bottom: 0; height: 5px }}</style>
                 <div class=box>{paragraphs}<div class=foot></div></div><p class=after>after</p>"
            ))
        };
        // The height properties hold for the parts of a broken box together, the first one
        // stretched to the end of the page: 10 lines of 14 fill it, and the parts on the next
        // pages are what is left of the height (CSS Fragmentation 3, §5.3).
        let cases = [
            ("min-height: 120px", "", 2, 40.0),
            (
                "min-height: 120px",
                "display: flex; flex-direction: column",
                2,
                40.0,
            ),
            ("height: 130px", "", 2, 30.0),
            ("min-height: 250px", "", 3, 150.0),
        ];
        for (sizing, display, page_count, rest) in cases {
            let (document, pages) = render_box(sizing, display);
            assert_eq!(pages.len(), page_count, "{sizing}, {display}");
            let (_, _, part) = find(&pages[1], &document, "box").expect("second part");
            let second = (rest + 10.0_f32).min(100.0);
            assert_eq!(part.rect.height.to_px(), second, "{sizing}, {display}");
            let (_, after, _) = find(&pages[page_count - 1], &document, "after").expect("after");
            assert_eq!(after, (rest + 10.0) % 100.0, "{sizing}, {display}");
            // A box positioned at the bottom of the box goes to the bottom of its last part.
            if page_count == 2 && display.is_empty() {
                let (_, foot, _) = find(&pages[1], &document, "foot").expect("foot");
                assert_eq!(foot, after - 5.0, "{sizing}, {display}");
            }
        }
    }

    #[test]
    fn aligns_flex_items_by_their_baselines() {
        // The baseline of the first line of a box, on the page.
        fn baseline(page: &Page, document: &Document, class: &str) -> f32 {
            fn first(fragment: &Fragment) -> Option<f32> {
                fragment.children.iter().find_map(|child| match &child.kind {
                    FragmentKind::Line { baseline } => Some(child.rect.y.to_px() + baseline.to_px()),
                    _ => first(child).map(|baseline| child.rect.y.to_px() + baseline),
                })
            }
            let (_, y, fragment) = find(page, document, class).expect("item");
            y + first(fragment).expect("a line")
        }
        // `align-items: baseline` puts the first baselines of the items on one line; an item
        // without lines puts its bottom edge there (CSS Flexbox 1, §8.3).
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font-family: Inter }
               .f { display: flex; align-items: baseline } .big { font: 30px/30px Inter }
               .small { font: 10px/10px Inter; margin-top: 5px } .box { width: 8px; height: 8px }</style>
             <div class=f><div class=big>A</div><div class=small>b</div><div class=box></div></div>",
        );
        let line = baseline(&pages[0], &document, "big");
        assert!(line > 20.0, "{line}");
        assert_eq!(baseline(&pages[0], &document, "small"), line);
        let (_, y, square) = find(&pages[0], &document, "box").expect("box");
        assert_eq!(y + square.rect.height.to_px(), line);
    }

    #[test]
    fn resolves_percentage_heights() {
        // A percentage height refers to the height of the containing block where that does not
        // depend on the content, the page area for the root element; others are `auto` (CSS 2.1,
        // §10.1 and §10.5). A minimum height wins over a maximum (§10.7).
        let (document, pages) = render_document(
            "<!DOCTYPE html><style>@page { size: 300px 200px; margin: 0 } html, body { margin: 0; height: 100% }
               body { font: 10px/10px Inter } .frame { height: 100%; border: 5px solid; box-sizing: border-box }
               .half { height: 50% } .auto .inner { height: 50% } .min { min-height: 25%; max-height: 10px }
               .flex { display: flex; flex-direction: column; justify-content: center; height: 20% }</style>
             <div class=frame><div class=half></div><div class=auto><div class=inner>x</div></div>
               <div class=min></div><div class=flex><p class=centered style='margin: 0'>c</p></div></div>",
        );
        let height = |class: &str| {
            let (_, _, fragment) = find(&pages[0], &document, class).expect("box");
            fragment.rect.height.to_px()
        };
        assert_eq!(height("frame"), 200.0);
        assert_eq!(height("half"), 95.0);
        assert_eq!(height("inner"), 10.0);
        assert_eq!(height("min"), 47.5);
        assert_eq!(height("flex"), 38.0);
        // The flex column centers its content in its height.
        let (_, flex_y, _) = find(&pages[0], &document, "flex").expect("flex");
        let (_, y, _) = find(&pages[0], &document, "centered").expect("centered");
        assert_eq!(y - flex_y, 14.0);
    }

    #[test]
    fn breaks_after_empty_boxes() {
        // `<div style="page-break-after: always"></div>`, as many templates break pages: the box
        // is empty and its margins collapse through it, but its break still forces one.
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p { margin: 5px 0 }</style>
             <p>a</p><div style='page-break-after: always'></div><p class=b>b</p>",
        );
        assert_eq!(pages.len(), 2);
        assert!(find(&pages[1], &document, "b").is_some());
    }

    #[test]
    fn passes_breaks_of_first_and_last_children_on() {
        // Breaks fall only between siblings: the `break-before` of a first child and the
        // `break-after` of a last child belong to the break point of their parent (CSS
        // Fragmentation 3, §3.1).
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p, h1 { margin: 0; font-size: 10px }</style>
             <p>a</p><section class=s style='margin-top: 10px'><h1 style='break-before: page'>h</h1></section>
             <div><p>b</p><div style='break-after: page'></div></div><p class=c>c</p>",
        );
        assert_eq!(pages.len(), 3);
        // The margin after the forced break stays.
        let (_, y, _) = find(&pages[1], &document, "s").expect("the section on the second page");
        assert_eq!(y, 10.0);
        assert!(find(&pages[2], &document, "c").is_some());
    }

    #[test]
    fn keeps_margins_after_forced_breaks() {
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p { margin: 0 }</style>
             <p>a</p><p class=forced style='break-before: page; margin-top: 30px'>forced</p>
             <div style='height: 85px'></div><p class=unforced style='margin-top: 30px'>unforced</p>
             <section style='break-before: page'><p class=nested style='margin-top: 20px'>nested</p></section>",
        );
        // Margins after a forced break are kept, after an unforced one truncated (CSS
        // Fragmentation 3, §5.2): the box after the forced break pushes the next one over the
        // end of the page, so that it breaks there and the paragraph after it starts with its
        // margin.
        assert_eq!(pages.len(), 4);
        assert_eq!(
            find(&pages[1], &document, "forced").map(|(_, y, _)| y),
            Some(30.0)
        );
        assert_eq!(
            find(&pages[2], &document, "unforced").map(|(_, y, _)| y),
            Some(55.0)
        );
        assert_eq!(
            find(&pages[3], &document, "nested").map(|(_, y, _)| y),
            Some(20.0)
        );
    }

    #[test]
    fn keeps_boxes_with_the_next_one() {
        let (document, pages) = render_document(
            "<style>@page { size: 300px 100px; margin: 0 } body { margin: 0; font: 10px/10px Inter }
               p, h2 { margin: 0; height: 30px } h2 { break-after: avoid } .keep { break-inside: avoid }</style>
             <p>1</p><p>2</p><h2 class=h>Titel</h2><p class=keep style='height: 40px'>Text</p>",
        );
        // The heading would fit at the bottom of the first page, but goes with its paragraph.
        assert_eq!(pages.len(), 2);
        let (_, y, _) = find(&pages[1], &document, "h").expect("heading on the second page");
        assert_eq!(y, 0.0);
    }

    #[test]
    fn lays_the_page_area_out_inside_the_page_box() {
        // Inside the page box's margins, border and padding.
        let pages = render(
            "<style>@page { size: 300px 200px; margin: 30px; border: 4px solid; padding: 10px }
               body { margin: 0 }</style><p>Text</p>",
        );
        let root = &pages[0].fragments[0];
        assert_eq!((root.rect.x.to_px(), root.rect.y.to_px()), (44.0, 44.0));
        assert_eq!(pages[0].margin, [units::Au::from_px(30.0); 4]);
        // The root element's top margin starts the first page.
        let pages = render(
            "<style>@page { size: 300px 200px; margin: 30px } html { margin-top: 10px }
               body { margin: 0 }</style><p>Text</p>",
        );
        assert_eq!(pages[0].fragments[0].rect.y.to_px(), 40.0);
    }

    #[test]
    fn moves_the_background_of_the_root_or_the_body_to_the_canvas() {
        // The color on the canvas, and how many boxes still draw one.
        fn colored(fragment: &Fragment) -> usize {
            let own = matches!(&fragment.kind, FragmentKind::Box { style, .. }
                if !style.background_color.is_transparent());
            usize::from(own) + fragment.children.iter().map(colored).sum::<usize>()
        }
        let canvas = |html: &str| {
            let pages = render(html);
            let color = pages[0].canvas.as_ref().map(|style| style.background_color);
            (color, pages[0].fragments.iter().map(colored).sum::<usize>())
        };
        let red = Some(fagus_style::values::Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        let blue = Some(fagus_style::values::Rgba {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        });
        // The body's, when the root element has none; the root element's, when it has one.
        assert_eq!(
            canvas("<body style='background: red'><p>Text</p></body>"),
            (red, 0)
        );
        assert_eq!(
            canvas("<html style='background: blue'><body style='background: red'><p>Text</p></body></html>"),
            (blue, 1)
        );
        assert_eq!(canvas("<p style='background: red'>Text</p>"), (None, 1));
        // Not the body's where it or the root element has containment.
        assert_eq!(
            canvas("<html style='contain: paint'><body style='background: red'><p>Text</p></body></html>"),
            (None, 1)
        );
        assert_eq!(
            canvas("<body style='contain: layout; background: red'><p>Text</p></body>"),
            (None, 1)
        );
        // As in Chrome, any containment does: `style`, and that of query containers.
        assert_eq!(
            canvas("<html style='contain: style'><body style='background: red'><p>Text</p></body></html>"),
            (None, 1)
        );
        assert_eq!(
            canvas("<body style='container-type: inline-size; background: red'><p>Text</p></body>"),
            (None, 1)
        );
    }

    #[test]
    fn wraps_the_rest_of_a_cloned_box_in_its_border_and_padding() {
        let pages = render(
            "<style>@page { size: 300px 200px; margin: 20px } body { margin: 0 }
               div { height: 300px; border: 2px solid; padding: 5px; box-decoration-break: clone }</style>
             <div></div>",
        );
        // 146 pixels of its height on the first two pages, the 8 left on the third, each part
        // with the border and padding around it.
        let heights: Vec<f32> = pages
            .iter()
            .map(|page| page.fragments[0].children[0].rect.height.to_px())
            .collect();
        assert_eq!(heights, [160.0, 160.0, 22.0]);
    }

    #[test]
    fn clamps_grids_like_browsers() {
        // Huge track counts, lines and spans are clamped instead of exhausting memory and time.
        let start = std::time::Instant::now();
        let pages = render(
            "<div style=\"display: grid; grid-template-columns: repeat(100000000, 1px); \
             grid-template-rows: repeat(100000000, 1px)\"><div>a</div>\
             <div style=\"grid-column: 99999999; grid-row: span 100000000\">b</div></div>\
             <div style=\"display: grid; grid-template-columns: repeat(auto-fill, minmax(0.01px, 1fr))\">\
             <div style=\"grid-area: span 1000 / span 1000\">c</div><div>d</div></div>",
        );
        assert!(!pages.is_empty());
        assert!(start.elapsed().as_secs() < 10, "{:?}", start.elapsed());
    }

    #[test]
    fn shifts_middle_aligned_boxes_in_layout_units() {
        // Half the x-height rounded to 1/64 px, as in Blink: the line stays 21px high.
        let (document, pages) = render_document(
            "<style>body { margin: 0; font: 11pt/1.4 Inter }</style>
             <p class=line>Bezahlt <span style='display: inline-block; width: 20px; height: 20px; vertical-align: middle'></span></p>",
        );
        let (_, _, line) = find(&pages[0], &document, "line").expect("paragraph");
        assert_eq!(line.rect.height.to_px(), 21.0);
    }

    #[test]
    fn wraps_loose_table_cells_in_rows() {
        // Cells without a row get an anonymous one, also in an inline table.
        let (document, pages) = render_document(
            "<style>body { font: 16px Inter }</style>
             <p>A <span class=table style='display: inline-table'><span class=cell style='display: table-cell'>x</span><span style='display: table-cell'>y</span></span> B</p>",
        );
        let (x, _, cell) = find(&pages[0], &document, "cell").expect("cell");
        let (table_x, _, table) = find(&pages[0], &document, "table").expect("table");
        assert!(cell.rect.width.to_px() > 0.0 && x >= table_x && table.rect.width > cell.rect.width);
    }

    #[test]
    fn draws_collapsed_borders_as_blink_joins_them() {
        // The wider border reaches over the joint by half the width across it; cells of whole
        // rows leave their borders to the table.
        let (document, pages) = render_document(
            "<style>@page { margin: 0 } body { margin: 0 } table { border-collapse: collapse }
               td { width: 30px; height: 20px; padding: 0; border: 2px solid #999 } .red { border: 6px solid red }</style>
             <table class=table><tr><td></td><td class=red></td><td></td></tr></table>",
        );
        let (_, _, table) = find(&pages[0], &document, "table").expect("table");
        let (red_x, _, red) = find(&pages[0], &document, "red").expect("cell");
        let FragmentKind::Box { style, .. } = &red.kind else {
            panic!("a cell box");
        };
        assert_eq!(style.border_style.top, fagus_style::values::BorderStyle::None);
        let red_top: Vec<&Fragment> = table
            .children
            .iter()
            .filter(|piece| match &piece.kind {
                FragmentKind::Box {
                    style, node: None, ..
                } => {
                    (style.border_width.top - 6.0).abs() < 0.01
                        && style.border_color.top == Some(fagus_style::values::Rgba::from_u8(255, 0, 0, 1.0))
                }
                _ => false,
            })
            .collect();
        assert_eq!(red_top.len(), 2, "the top and bottom lines");
        // From the left side's outer edge to the right side's.
        let table_x = red_x - red.rect.x.to_px();
        assert_eq!(red_top[0].rect.width.to_px(), red.rect.width.to_px());
        assert_eq!(table_x + red_top[0].rect.x.to_px(), red_x);
    }

    mod broken_images;
    mod business_documents;
    mod column_rows;
    mod column_spans;
    mod contained_boxes;
    mod containment;
    mod editor_templates;
    mod extended_wpt;
    mod flex_alignment;
    mod flex_ratio_lines;
    mod flex_wrapping;
    mod float_room;
    mod float_shapes;
    mod flow_details;
    mod flow_tables;
    mod footnotes;
    mod forced_breaks;
    mod fragmentation_choices;
    mod fragmentation_layout;
    mod fragmentation_leftovers;
    mod framework_documents;
    mod generated_documents;
    mod grid_alignment;
    mod grid_leftovers;
    mod grid_sizing;
    mod hanging_punctuation;
    mod inline_layout;
    mod invoice_templates;
    mod lanes_fallback;
    mod legacy_documents;
    mod list_markers;
    mod long_documents;
    mod mathml_layout;
    mod modern_css;
    mod office_documents;
    mod overflow_breaks;
    mod overflow_pages;
    mod padding_breaks;
    mod page_breaks;
    mod page_groups;
    mod page_heights;
    mod page_overflow;
    mod page_pieces;
    mod page_sides;
    mod parallel_flows;
    mod positioned_columns;
    mod positioned_flow;
    mod positioned_leftovers;
    mod positioned_pages;
    mod positioned_parts;
    mod print_documents;
    mod ratio_flex_sizing;
    mod report_widgets;
    mod robustness;
    mod row_breaks;
    mod row_parts;
    mod sizing_layout;
    mod table_baselines;
    mod table_breaks;
    mod table_parts;
    mod text_layout;
    mod text_leftovers;
    mod text_spaces;
    mod vertical_flows;
    mod web_app_forms;
}

#[cfg(test)]
mod debug {
    use super::*;

    /// Prints the fragment tree of a corpus document: `FAGUS_DUMP=path cargo test -p fagus-layout dump -- --nocapture`.
    #[test]
    fn dump_corpus_document() {
        let Ok(path) = std::env::var("FAGUS_DUMP") else {
            return;
        };
        let html = std::fs::read_to_string(&path).expect("document");
        let mut fonts = FontDb::new();
        fonts
            .add_font_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts"))
            .expect("fonts");
        let document = Document::parse_html(&html);
        let ua = fagus_css::Stylesheet::parse(
            fagus_style::USER_AGENT_CSS,
            fagus_css::Origin::UserAgent,
            "ua.css",
        );
        let css: String = document
            .descendants(fagus_dom::NodeId::DOCUMENT)
            .filter(|&node| document.element(node).is_some_and(|data| data.is("style")))
            .map(|node| document.text_content(node))
            .collect();
        let author = fagus_css::Stylesheet::parse(&css, fagus_css::Origin::Author, "style");
        let environment = fagus_css::MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = fagus_style::compute_styles(&document, &rules, (environment.width, environment.height));
        let pages = layout(&document, &styled, &rules, &fonts, &Images::default());
        for (index, page) in pages.iter().enumerate() {
            println!("page {}\n{}", index + 1, dump(page, &document));
        }
    }
}
