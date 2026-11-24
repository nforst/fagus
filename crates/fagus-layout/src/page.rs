//! Paged media: page boxes from `@page` rules, pagination and margin boxes.

use std::sync::Arc;

use fagus_css::PageSelector;
use fagus_css::lightningcss::properties::Property;
use fagus_css::lightningcss::properties::custom::{CustomPropertyName, Token, TokenOrValue};
use fagus_css::lightningcss::rules::page::{PageMarginBox, PagePseudoClass};
use fagus_css::page_size::{A4, MAX_PAGE_SIDE, page_height_fits, page_size};
use fagus_css::{CascadeBand, PageRuleData, RuleSet};
use fagus_style::values::{
    Content, ContentItem, LengthPercentage, LengthPercentageAuto, Size, StringWhich, TextAlign, VerticalAlign,
};
use fagus_style::{ComputedStyle, style_from_declarations};
use fagus_text::FontDb;

use rustc_hash::FxHashMap;

use crate::block::{BreakToken, Layouter, Space};
use crate::counter_style::CounterStyles;
use crate::fragment::{
    Carried, Children, Fragment, FragmentKind, Page, PageMarks, PageOrientation, VisibleSides,
};
use crate::inline::{intrinsic_widths, layout_lines};
use crate::positioned::{Flowing, PageFlow};
use crate::tree::{BoxId, BoxTree, InlineItem, PageCounter, StringValue, process_white_space};
use crate::units::{Au, Rect};

const MM: f32 = 96.0 / 25.4;
const INCH: f32 = 96.0;

/// How many pages a document has at most: content that goes on further (a box billions of pixels
/// tall) is left out. Chrome has no such limit.
pub const MAX_PAGES: usize = 10_000;

/// How many fragments the content of the pages of a document holds at most, together (about a
/// gigabyte by the time the PDF is written); with positioned boxes and margin boxes, twice as
/// many. Content repeated on every page (a table header of a thousand cells, a fixed box, across
/// thousands of pages) or text of one character per line would otherwise take gigabytes; a
/// thousand pages of dense text hold a third of this.
const MAX_FRAGMENTS: usize = 1_000_000;

/// The fonts that the margin boxes of `pages` are set in: as Chrome sets them while printing,
/// when it loads no more faces of `@font-face` rules, those faces that the content of the
/// document does not use are not loaded there (text that would take them goes on to the next
/// family: running heads in an italic used nowhere else come out in the system's italic). The
/// content uses the faces of its text and the first face of the font of each of its boxes.
/// `None` where it uses all faces of the document.
fn unloaded_for_margins(tree: &BoxTree, fonts: &FontDb, pages: &[Page]) -> Option<FontDb> {
    if !fonts.faces().any(|face| face.document) {
        return None;
    }
    let mut used = vec![false; fonts.len()];
    let mut mark = |id: fagus_text::FaceId| {
        if let Some(used) = used.get_mut(id.index()) {
            *used = true;
        }
    };
    let mut stack: Vec<&Fragment> = pages.iter().flat_map(|page| &page.fragments).collect();
    while let Some(fragment) = stack.pop() {
        if let FragmentKind::Text(run) = &fragment.kind {
            mark(run.face);
        }
        stack.extend(&fragment.children);
    }
    for style in tree.styles() {
        if style.display != fagus_style::values::Display::None
            && let Some(face) = crate::inline::face_for(fonts, style)
        {
            mark(face);
        }
    }
    fonts.with_unloaded(|id| used.get(id.index()).copied().unwrap_or(true))
}

/// What a fragment is charged to the budget of the render (see [`fagus_dom::budget`]): itself
/// and about as much again for what it points to (its children, the glyphs of its text).
const FRAGMENT_BYTES: u64 = 2 * size_of::<Fragment>() as u64;

/// What the fragments of `pages` are charged to the budget of the render, in bytes.
pub fn held_by(pages: &[Page]) -> u64 {
    let fragments: usize = pages
        .iter()
        .flat_map(|page| &page.fragments)
        .map(fragment_count)
        .sum();
    fragments as u64 * FRAGMENT_BYTES
}

/// How many fragments a fragment tree holds, itself included.
fn fragment_count(fragment: &Fragment) -> usize {
    let mut count = 0;
    let mut stack = vec![fragment];
    while let Some(fragment) = stack.pop() {
        count += 1;
        stack.extend(&fragment.children);
    }
    count
}

/// The page box of one page.
#[derive(Clone, Debug)]
pub struct PageBox {
    pub width: Au,
    pub height: Au,
    /// Page margins: top, right, bottom, left.
    pub margin: [Au; 4],
    /// The page area, where the content goes: placed at the margins rounded to whole pixels,
    /// and as wide and tall as the page without them, rounded up to whole pixels, as Chrome
    /// lays out pages.
    pub area: Rect,
    /// Where the left edge of the content goes across the page: that of the page area, or, when
    /// the root element is right to left, where the page area's right edge comes to lie at the
    /// right margin rounded to whole pixels, as in Chrome, which places such content from the
    /// right (with margins of 16mm on A4, 0.7 pixels to the right of the page area).
    pub content_x: Au,
    /// Whether the root element is right to left, which places the content and the margin
    /// boxes from the right.
    pub rtl: bool,
    /// The size of the page area cut to whole pixels: the viewport, which is the containing
    /// block of fixed boxes and of absolutely positioned boxes without a positioned ancestor,
    /// and what viewport units are relative to, as Chrome lays out pages (where the page area
    /// is rounded up for the content that flows through it).
    pub viewport: (Au, Au),
    pub style: Arc<ComputedStyle>,
    /// The page area before content wider than the page shrinks (see [`Page::scale`]).
    pub area_unshrunk: Rect,
    /// The size of the page area before it is rounded.
    pub exact: (f32, f32),
    /// The page's margins, borders and padding (top, right, bottom, left) in pixels, before they
    /// are cut to layout units.
    pub insets: [f32; 4],
    /// Margin boxes with their style.
    pub margin_boxes: Vec<(PageMarginBox, Arc<ComputedStyle>)>,
    /// The style of the footnote area (`@footnote`), which inherits from the page.
    pub footnote: Arc<ComputedStyle>,
    /// The value the page resets the `footnote` counter to (`counter-reset: footnote`), if it
    /// does.
    pub footnote_reset: Option<i32>,
    /// `bleed`, in pixels.
    pub bleed: f32,
    pub marks: PageMarks,
    /// How the page is turned on its sheet.
    pub orientation: PageOrientation,
    /// Whether the page grows with its content (`-fagus-page-height: fit-content`, a Fagus
    /// extension): as tall as its content and margins, at least as tall as `height`.
    pub fit_content: bool,
}

impl PageBox {
    pub fn content_width(&self) -> Au {
        self.area.width
    }

    pub fn content_height(&self) -> Au {
        self.area.height
    }

    /// The size of the page area before it is rounded, where content wider than the page lays
    /// out `factor` times as large: as Chrome scales the page and the insets (each cut to layout
    /// units) rather than the page area.
    pub fn exact_scaled(&self, factor: f32) -> (f32, f32) {
        let cut = |px: f32| (px * factor * 64.0).trunc() / 64.0;
        (
            (cut(self.width.to_px()) - cut(self.insets[1]) - cut(self.insets[3])).max(0.0),
            (cut(self.height.to_px()) - cut(self.insets[0]) - cut(self.insets[2])).max(0.0),
        )
    }

    /// The tallest this page may grow with its content (see [`Self::fit_content`]), in pixels:
    /// its sheet, with the bleed and the room for printer's marks around it, as tall as a PDF
    /// page may be.
    fn tallest(&self) -> f32 {
        // (The room for marks is that of `fagus-pdf`, half an inch on each side.)
        let marks = if self.marks.crop || self.marks.cross {
            INCH / 2.0
        } else {
            0.0
        };
        let tallest = MAX_PAGE_SIDE - 2.0 * (self.bleed + marks);
        // Whole points, so that the sheet is not rounded past the limit.
        (tallest * 0.75).floor() / 0.75
    }

    /// The page box with its height changed to `height` pixels (rounded up to whole points, as
    /// the sheet is): its page area and viewport as tall as that leaves.
    fn with_height(&self, height: f32) -> Self {
        let page = (height * 0.75).ceil() / 0.75;
        let page = (page * 64.0).round() / 64.0;
        let cut = |px: f32| (px * 64.0).trunc() / 64.0;
        let exact = (page - cut(self.insets[0]) - cut(self.insets[2])).max(0.0);
        let mut grown = self.clone();
        grown.height = Au::from_px(page);
        grown.exact.1 = exact;
        grown.area.height = Au::from_px(exact.ceil());
        grown.area_unshrunk.height = grown.area.height;
        grown.viewport.1 = Au::from_px(exact.floor());
        grown
    }

    /// The page box of a page that grows with its content (see [`Self::fit_content`]), for
    /// content that reaches `bottom` below the top of its page area: as tall as that, its
    /// margins, border and padding, at least as tall as it is and at most as tall as a PDF page
    /// may be.
    fn fitted(&self, bottom: Au) -> Self {
        // Content that fits in the page area as it is laid out (rounded up to whole pixels)
        // keeps the page as it is.
        if bottom <= self.area.height {
            return self.clone();
        }
        let cut = |px: f32| (px * 64.0).trunc() / 64.0;
        let needed = bottom.to_px() + cut(self.insets[0]) + cut(self.insets[2]);
        let height = needed.min(self.tallest()).max(self.height.to_px());
        if height <= self.height.to_px() {
            return self.clone();
        }
        self.with_height(height)
    }
}

/// A page group that a page is in (CSS Generated Content for Paged Media 3, §3.2): the pages of
/// an element whose `page` is not `auto` and that has a forced break before it, from the page it
/// starts on. Each element starts a group of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageGroup {
    /// The page name of the element.
    pub name: Arc<str>,
    /// The page the element starts on (0-based).
    pub start: usize,
}

/// What the pseudo-classes of page selectors match on a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageType<'a> {
    /// The page's index in the document (0-based), for `:nth()`; `:first` matches index 0.
    index: usize,
    /// Whether it is a left page (`:left`), else a right one (`:right`).
    left: bool,
    /// Whether it is a blank page inserted for a forced break to a left or right page
    /// (`:blank`).
    blank: bool,
    /// The page groups the page is in, for `:nth()` of a page name.
    groups: &'a [PageGroup],
}

impl<'a> PageType<'a> {
    /// Page `index` (0-based) of a document whose first page is a left page (`first_left`) or a
    /// right one, blank or not, in `groups`.
    fn of(index: usize, first_left: bool, blank: bool, groups: &'a [PageGroup]) -> Self {
        Self {
            index,
            left: index.is_multiple_of(2) == first_left,
            blank,
            groups,
        }
    }

    /// Whether `:nth()` matches the page (CSS Generated Content for Paged Media 3, §3): with a
    /// page name after `of`, a page that is the nth of a page group of that name; on a selector
    /// with a page name, the nth page of a page group of that name where the page is in one
    /// (where "the `:nth()` selector is applied to a named page, and that named page is part of
    /// a page-group, it selects the nth page in the page group"); else the nth page of the
    /// document.
    fn nth(self, nth: &fagus_css::PageNth, selector_name: Option<&str>) -> bool {
        let in_groups = |name: &str| {
            self.groups
                .iter()
                .filter(|group| &*group.name == name)
                .map(|group| (self.index + 1).saturating_sub(group.start))
                .collect::<Vec<_>>()
        };
        match (nth.of.as_deref(), selector_name) {
            (Some(name), _) => in_groups(name).into_iter().any(|index| nth.matches(index)),
            (None, Some(name)) if self.groups.iter().any(|group| &*group.name == name) => {
                in_groups(name).into_iter().any(|index| nth.matches(index))
            }
            _ => nth.matches(self.index + 1),
        }
    }
}

/// Whether a page selector matches a page of type `page` with the page name `name`.
fn matches(selector: &PageSelector, page: PageType<'_>, name: Option<&str>) -> bool {
    selector
        .name
        .as_deref()
        .is_none_or(|selector_name| Some(selector_name) == name)
        && selector.pseudo_classes.iter().all(|pseudo| match pseudo {
            PagePseudoClass::First => page.index == 0,
            PagePseudoClass::Right => !page.left,
            PagePseudoClass::Left => page.left,
            PagePseudoClass::Blank => page.blank,
            PagePseudoClass::Last => false,
        })
        && selector
            .nth
            .iter()
            .all(|nth| page.nth(nth, selector.name.as_deref()))
}

/// The specificity of the most specific selector of `rule` that matches the page, if one does
/// (CSS Paged Media 3, §4.2: a page name, then `:first` and `:blank`, then `:left` and `:right`).
/// CSS Generated Content for Paged Media 3 leaves that of `:nth()` open (w3c/csswg-drafts#3524):
/// as proposed there and as `WeasyPrint` ranks it, it counts as `:first`, and its page name after
/// `of` as a page name.
fn specificity(rule: &PageRuleData, page: PageType<'_>, name: Option<&str>) -> Option<u32> {
    if rule.selectors.is_empty() {
        return Some(0);
    }
    rule.selectors
        .iter()
        .filter(|selector| matches(selector, page, name))
        .map(|selector| {
            let count = |count: usize| u32::try_from(count).unwrap_or(u32::MAX).min(255);
            let named = u32::from(selector.name.is_some())
                + count(selector.nth.iter().filter(|nth| nth.of.is_some()).count());
            let (first, sides) = selector.pseudo_classes.iter().fold(
                (count(selector.nth.len()), 0),
                |(first, sides), pseudo| match pseudo {
                    PagePseudoClass::First | PagePseudoClass::Blank => (first + 1, sides),
                    _ => (first, sides + 1),
                },
            );
            (named.min(255) << 16) | (first.min(255) << 8) | sides.min(255)
        })
        .max()
}

/// Computes the page box of page `page_index` (0-based), whose page name is `name` and which is
/// in the page groups `groups`. Left-to-right documents start with a right page, right-to-left
/// ones with a left page (CSS Paged Media 3, §4.3.2), as in Chrome.
pub fn page_box(
    rules: &RuleSet,
    root: &ComputedStyle,
    page_index: usize,
    name: Option<&str>,
    groups: &[PageGroup],
) -> PageBox {
    let page = PageType::of(page_index, root.is_rtl(), false, groups);
    page_box_from(&matching_rules(rules, page, name), root)
}

/// The page boxes of a document's pages, kept by the `@page` rules that match them: pages that
/// the same rules match share their box, and the styles of its page and margin boxes, which the
/// fragments of each page refer to, rather than each having styles of its own.
#[derive(Default)]
struct PageBoxes(std::cell::RefCell<FxHashMap<Vec<usize>, PageBox>>);

impl PageBoxes {
    /// [`page_box`], from the boxes made before where the same rules match.
    fn get(&self, rules: &RuleSet, root: &ComputedStyle, page: PageType<'_>, name: Option<&str>) -> PageBox {
        let matching = matching_rules(rules, page, name);
        let key: Vec<usize> = matching
            .iter()
            .map(|&rule| std::ptr::from_ref(rule) as usize)
            .collect();
        if let Some(found) = self.0.borrow().get(&key) {
            return found.clone();
        }
        let made = page_box_from(&matching, root);
        self.0.borrow_mut().insert(key, made.clone());
        made
    }
}

/// The `@page` rules that match a page of type `page` with the page name `name`, in cascade
/// order.
fn matching_rules<'a>(rules: &'a RuleSet, page: PageType<'_>, name: Option<&str>) -> Vec<&'a PageRuleData> {
    let mut matching: Vec<(&PageRuleData, u32)> = rules
        .page_rules()
        .iter()
        .filter_map(|rule| Some((rule, specificity(rule, page, name)?)))
        .collect();
    matching.sort_by_key(|(rule, specificity)| {
        (
            CascadeBand::of(rule.origin, false),
            rule.layer,
            *specificity,
            rule.order,
        )
    });
    matching.into_iter().map(|(rule, _)| rule).collect()
}

/// Computes a page box from the `@page` rules that match it, in cascade order.
fn page_box_from(matching: &[&PageRuleData], root: &ComputedStyle) -> PageBox {
    let (mut width, mut height) = A4;
    let mut bleed = None;
    let mut marks = PageMarks::default();
    let mut orientation = PageOrientation::Upright;
    let mut fit_content = false;
    for rule in matching {
        for property in &rule.declarations.declarations {
            let Property::Custom(custom) = property else {
                continue;
            };
            let CustomPropertyName::Unknown(name) = &custom.name else {
                continue;
            };
            match name.0.to_ascii_lowercase().as_str() {
                "size" => {
                    if let Some(size) = page_size(&custom.value.0) {
                        (width, height) = size;
                    }
                }
                "marks" => {
                    if let Some(parsed) = page_marks(&custom.value.0) {
                        marks = parsed;
                    }
                }
                "bleed" => bleed = page_bleed(&custom.value.0).or(bleed),
                "-fagus-page-height" => {
                    if let Some(fits) = page_height_fits(&custom.value.0) {
                        fit_content = fits;
                    }
                }
                "page-orientation" => {
                    if let Some(parsed) = page_orientation(&custom.value.0) {
                        orientation = parsed;
                    }
                }
                _ => {}
            }
        }
    }
    let bleed = match bleed {
        Some(Bleed::Length(length)) => length,
        _ if marks.crop => 6.0 * INCH / 72.0,
        _ => 0.0,
    };
    let blocks: Vec<_> = matching.iter().map(|rule| &rule.declarations).collect();
    // Viewport units in the page context are relative to the default page, and percentages of
    // the page margins to the page's width across and its height down, as in Chrome.
    let style = style_from_declarations(&blocks, root, A4);
    let containing = (width, height);
    let (size, margins) = page_geometry(&style, containing);
    let (width, height) = size;
    // A page without room for its content is the default page, as only the user agent's rules
    // make it (Chrome's default page size and margins). A page that grows with its content
    // needs room across only.
    let room_down = if fit_content { MAX_PAGE_SIDE } else { height };
    if too_small_for_content((width, room_down), margins) {
        let defaults: Vec<&PageRuleData> = matching
            .iter()
            .copied()
            .filter(|rule| rule.origin == fagus_css::Origin::UserAgent)
            .collect();
        if defaults.len() < matching.len() {
            return page_box_from(&defaults, root);
        }
    }
    let margin = margins.map(Au::from_px);
    // The page area lies inside the page box's border and padding as well.
    let border = &style.border_width;
    let padding = page_padding(&style, containing);
    let insets = [
        margins[0] + border.top + padding[0],
        margins[1] + border.right + padding[1],
        margins[2] + border.bottom + padding[2],
        margins[3] + border.left + padding[3],
    ];
    // As Chrome sizes it (measured): the page size rounded to layout units, less the margins
    // cut to them, rounded up to whole pixels. A4 with margins of 12mm leaves 703.02 pixels,
    // laid out 704 wide.
    let page = |px: f32| (px * 64.0).round() / 64.0;
    let cut = |px: f32| (px * 64.0).trunc() / 64.0;
    let exact = (
        (page(width) - cut(insets[1]) - cut(insets[3])).max(0.0),
        (page(height) - cut(insets[0]) - cut(insets[2])).max(0.0),
    );
    let area = Rect::new(
        Au::from_px(insets[3].round()),
        Au::from_px(insets[0].round()),
        Au::from_px(exact.0.ceil()),
        Au::from_px(exact.1.ceil()),
    );
    let viewport = (Au::from_px(exact.0.floor()), Au::from_px(exact.1.floor()));
    let content_x = if root.is_rtl() {
        Au::from_px(page(width) - insets[1].round() - exact.0.ceil())
    } else {
        area.x
    };

    // Margin boxes: later rules override earlier ones for the same box.
    let mut margin_boxes: Vec<(PageMarginBox, Vec<&fagus_css::DeclarationBlock>)> = Vec::new();
    for rule in matching {
        for margin_rule in &rule.margin_rules {
            match margin_boxes
                .iter_mut()
                .find(|(kind, _)| *kind == margin_rule.margin_box)
            {
                Some((_, blocks)) => blocks.push(&margin_rule.declarations),
                None => margin_boxes.push((margin_rule.margin_box, vec![&margin_rule.declarations])),
            }
        }
    }
    let margin_boxes = margin_boxes
        .into_iter()
        .map(|(kind, blocks)| (kind, Arc::new(style_from_declarations(&blocks, &style, A4))))
        .filter(|(_, style)| matches!(style.content, Content::Items(_)))
        .collect();
    let footnote_blocks: Vec<&fagus_css::DeclarationBlock> =
        matching.iter().flat_map(|rule| &rule.footnote_rules).collect();
    let footnote = Arc::new(style_from_declarations(&footnote_blocks, &style, A4));
    let footnote_reset = style
        .counter_reset
        .iter()
        .rev()
        .find(|(name, _)| name == "footnote")
        .map(|&(_, value)| value);

    PageBox {
        // The page size rounded to layout units, as Chrome holds it: JIS-B5 is 257mm, 971.34
        // pixels, 728.51 points, which Chrome prints as 729 points.
        width: Au::from_px(page(width)),
        height: Au::from_px(page(height)),
        margin,
        area,
        content_x,
        rtl: root.is_rtl(),
        area_unshrunk: area,
        exact,
        insets,
        viewport,
        style: Arc::new(style),
        margin_boxes,
        footnote,
        footnote_reset,
        bleed,
        marks,
        orientation,
        fit_content,
    }
}

/// The padding of the page box (top, right, bottom, left) in pixels: percentages of the page's
/// width across and its height down, as in Chrome.
fn page_padding(style: &ComputedStyle, (width, height): (f32, f32)) -> [f32; 4] {
    [
        style.padding.top.resolve(height),
        style.padding.right.resolve(width),
        style.padding.bottom.resolve(height),
        style.padding.left.resolve(width),
    ]
}

/// The size of a page whose `size` is `size` and its margins (top, right, bottom, left), in
/// pixels, as Chrome lays out the page box in a containing block of that size: the page box is
/// as wide and tall as its `width` and `height` (within their minimum and maximum) say, else it
/// fills what the margins leave; `auto` margins take the room left over (negative where there
/// is none), and where the box and its margins do not add up to `size`, the page takes their
/// size instead of `size` (CSS Paged Media 3, §7.3; Blink's `ResolvePageBoxGeometry`).
fn page_geometry(style: &ComputedStyle, size: (f32, f32)) -> ((f32, f32), [f32; 4]) {
    let padding = page_padding(style, size);
    let border = &style.border_width;
    let content_box = style.box_sizing == fagus_style::values::BoxSizing::ContentBox;
    // One axis: the containing block's length, the margins at its start and end, the frame
    // (border and padding), and the size, minimum and maximum, as lengths if they are lengths.
    let axis = |available: f32,
                (start, end): (&LengthPercentageAuto, &LengthPercentageAuto),
                frame: f32,
                (length, min, max): (&Size, &Size, &fagus_style::values::MaxSize)| {
        let border_box =
            |value: &LengthPercentage| value.resolve(available) + if content_box { frame } else { 0.0 };
        let fixed = (start.resolve_or(available, 0.0), end.resolve_or(available, 0.0));
        let mut used = match length {
            Size::Length(value) => border_box(value),
            _ => available - fixed.0 - fixed.1,
        };
        if let fagus_style::values::MaxSize::Length(value) = max {
            used = used.min(border_box(value));
        }
        if let Size::Length(value) = min {
            used = used.max(border_box(value));
        }
        let used = used.max(frame);
        let left = available - used - fixed.0 - fixed.1;
        let margins = match (start.is_auto(), end.is_auto()) {
            (true, true) => (left / 2.0, left / 2.0),
            (true, false) => (left, fixed.1),
            (false, true) => (fixed.0, left),
            (false, false) => fixed,
        };
        (margins.0 + used + margins.1, margins)
    };
    let (width, (left, right)) = axis(
        size.0,
        (&style.margin.left, &style.margin.right),
        border.left + border.right + padding[1] + padding[3],
        (&style.width, &style.min_width, &style.max_width),
    );
    let (height, (top, bottom)) = axis(
        size.1,
        (&style.margin.top, &style.margin.bottom),
        border.top + border.bottom + padding[0] + padding[2],
        (&style.height, &style.min_height, &style.max_height),
    );
    ((width, height), [top, right, bottom, left])
}

/// Parses the `page-orientation` descriptor of `@page`.
fn page_orientation(tokens: &[TokenOrValue<'static>]) -> Option<PageOrientation> {
    let mut idents = tokens.iter().filter_map(|token| match token {
        TokenOrValue::Token(Token::WhiteSpace(_)) => None,
        TokenOrValue::Token(Token::Ident(ident)) => Some(Some(ident.to_ascii_lowercase())),
        _ => Some(None),
    });
    let orientation = match idents.next()??.as_str() {
        "upright" => PageOrientation::Upright,
        "rotate-left" => PageOrientation::RotateLeft,
        "rotate-right" => PageOrientation::RotateRight,
        _ => return None,
    };
    idents.next().is_none().then_some(orientation)
}

/// Parses the `marks` descriptor of `@page`: `none`, or `crop` and `cross` in any order.
fn page_marks(tokens: &[TokenOrValue<'static>]) -> Option<PageMarks> {
    let mut marks = PageMarks::default();
    for token in tokens {
        match token {
            TokenOrValue::Token(Token::Ident(ident)) => match ident.to_ascii_lowercase().as_str() {
                "none" => return Some(PageMarks::default()),
                "crop" => marks.crop = true,
                "cross" => marks.cross = true,
                _ => return None,
            },
            TokenOrValue::Token(Token::WhiteSpace(_)) => {}
            _ => return None,
        }
    }
    Some(marks)
}

/// The `bleed` descriptor of `@page`.
#[derive(Clone, Copy)]
enum Bleed {
    /// 6pt with crop marks, otherwise none (CSS Paged Media 3, §7.2).
    Auto,
    /// A length, in pixels.
    Length(f32),
}

/// Parses the `bleed` descriptor of `@page`.
fn page_bleed(tokens: &[TokenOrValue<'static>]) -> Option<Bleed> {
    tokens.iter().find_map(|token| match token {
        TokenOrValue::Token(Token::Ident(ident)) if ident.eq_ignore_ascii_case("auto") => Some(Bleed::Auto),
        TokenOrValue::Length(length) => length.to_px().map(|px| Bleed::Length(px.max(0.0))),
        TokenOrValue::Token(Token::Dimension { value, unit, .. }) => {
            let factor = match unit.to_ascii_lowercase().as_str() {
                "mm" => MM,
                "cm" => 10.0 * MM,
                "in" => INCH,
                "pt" => INCH / 72.0,
                "pc" => INCH / 6.0,
                "px" => 1.0,
                _ => return None,
            };
            Some(Bleed::Length((value * factor).max(0.0)))
        }
        TokenOrValue::Token(Token::Number { value, .. }) if *value == 0.0 => Some(Bleed::Length(0.0)),
        _ => None,
    })
}

/// Whether a page of `size` pixels with `margins` (top, right, bottom, left) leaves less than a
/// point for its content across or down, which Chrome prints on the default page with the
/// default margins instead: the page rounded to whole points, the margins as they are.
fn too_small_for_content((width, height): (f32, f32), margins: [f32; 4]) -> bool {
    let points = |px: f32| px * 72.0 / 96.0;
    points(width).round() - points(margins[1] + margins[3]) < 1.0
        || points(height).round() - points(margins[0] + margins[2]) < 1.0
}

/// Which side a value of `break-before` or `break-after` asks the next page to be on: `Some(true)`
/// for a left page, `Some(false)` for a right one; `recto` and `verso` by the direction of the
/// document (a right-to-left document starts with a left page, its recto).
fn forced_side(value: fagus_style::values::BreakBetween, rtl: bool) -> Option<bool> {
    use fagus_style::values::BreakBetween;
    match value {
        BreakBetween::Left => Some(true),
        BreakBetween::Right => Some(false),
        BreakBetween::Recto => Some(rtl),
        BreakBetween::Verso => Some(!rtl),
        _ => None,
    }
}

/// Which side the forced break where layout of box `id` resumes at `token` asks the next page to
/// be on (see [`forced_side`]), where it falls between block-level siblings: the side of the
/// `break-before` of the box after the break, else that of the `break-after` of the in-flow box
/// before it (the later value wins, CSS Fragmentation 3, §3.1).
fn side_at_break(tree: &BoxTree, id: BoxId, token: &BreakToken, rtl: bool) -> Option<bool> {
    let (crate::tree::BoxKind::Block { children }, BreakToken::Block { index, inner, .. }) =
        (&tree.get(id).kind, token)
    else {
        return None;
    };
    if let Some(inner) = inner {
        return side_at_break(tree, *children.get(*index)?, inner, rtl);
    }
    let before = children
        .get(*index)
        .and_then(|&child| forced_side(tree.break_before(child), rtl));
    before.or_else(|| {
        children
            .get(..*index)?
            .iter()
            .rev()
            .find(|&&child| !tree.get(child).style.is_out_of_flow())
            .and_then(|&child| forced_side(tree.break_after(child), rtl))
    })
}

/// What the `@page` rules of a page (or a margin box) say of the `page` counter: its
/// `counter-reset`, `counter-set` and `counter-increment` values, `None` where it is not named.
fn page_counter_directives(style: &ComputedStyle) -> (Option<i32>, Option<i32>, Option<i32>) {
    let last = |list: &[(String, i32)]| {
        list.iter()
            .rev()
            .find(|(name, _)| name == "page")
            .map(|&(_, value)| value)
    };
    let increment = style
        .counter_increment
        .iter()
        .filter(|(name, _)| name == "page")
        .map(|&(_, value)| value)
        .reduce(i32::saturating_add);
    (last(&style.counter_reset), last(&style.counter_set), increment)
}

/// The page counter after the pages numbered so far, as Chrome keeps it (Blink's
/// `CountersAttachmentContext`): a page whose `@page` rules do not name the `page` counter in
/// `counter-reset`, `counter-set` or `counter-increment` increments it by one;
/// `counter-increment: page 2` adds 2 instead and `counter-set: page 5` sets it to 5, for the
/// pages after too; `counter-reset: page 5` gives it a value on that page only (with its
/// increment, or the value of its `counter-set`), and the page after goes on from the pages
/// before it. Resetting the counter in an element of the document has no effect on it.
#[derive(Clone, Copy, Default)]
struct PageNumbering(Option<i32>);

impl PageNumbering {
    /// The value of the counter on a page whose page box has `style`, and the counter after it.
    fn on(self, style: &ComputedStyle) -> (i32, Self) {
        let current = self.0.unwrap_or(0);
        match page_counter_directives(style) {
            (None, None, None) => {
                let value = current.saturating_add(1);
                (value, Self(Some(value)))
            }
            (Some(reset), set, increment) => (
                set.unwrap_or_else(|| reset.saturating_add(increment.unwrap_or(0))),
                self,
            ),
            (None, Some(set), _) => (set, Self(Some(set))),
            (None, None, Some(increment)) => {
                let value = current.saturating_add(increment);
                (value, Self(Some(value)))
            }
        }
    }

    /// Numbers page `index` (0-based), whose page box has `style`, for `layouter`: the value of
    /// the counter on it goes into [`Layouter::page_values`]. Returns the counter after it.
    fn number(self, layouter: &Layouter<'_>, index: usize, style: &ComputedStyle) -> Self {
        let (value, after) = self.on(style);
        let mut values = layouter.page_values.borrow_mut();
        values.truncate(index);
        values.push(value);
        after
    }
}

/// The value of the page counter in a margin box of `style` on a page where it is `page`: as in
/// Chrome, a margin box that resets, sets or increments the counter has a value of its own.
fn margin_page_value(style: &ComputedStyle, page: i32) -> Option<i32> {
    match page_counter_directives(style) {
        (None, None, None) => None,
        (Some(reset), set, increment) => {
            Some(set.unwrap_or_else(|| reset.saturating_add(increment.unwrap_or(0))))
        }
        (None, Some(set), _) => Some(set),
        (None, None, Some(increment)) => Some(page.saturating_add(increment)),
    }
}

/// Runs `lay_out` with the value of the page counter of a margin box of `style` (see
/// [`margin_page_value`]) on the page being laid out.
fn in_margin_box<T>(layouter: &Layouter<'_>, style: &ComputedStyle, lay_out: impl FnOnce() -> T) -> T {
    let index = layouter.page_number.get().0;
    let page = index
        .checked_sub(1)
        .and_then(|index| layouter.page_values.borrow().get(index).copied())
        .unwrap_or_else(|| i32::try_from(index).unwrap_or(i32::MAX));
    let outer = layouter.margin_page_value.replace(margin_page_value(style, page));
    let result = lay_out();
    layouter.margin_page_value.set(outer);
    result
}

/// The page groups of the pages laid out so far (see [`PageGroup`]), kept where `@page` rules
/// count the pages of groups with `:nth()`.
struct PageGroups {
    on: bool,
    /// The page each element that makes a group starts on.
    starts: FxHashMap<BoxId, usize>,
    /// The groups of the last page with content, which the pages added below it are in too.
    last: Vec<PageGroup>,
}

impl PageGroups {
    fn new(tree: &BoxTree, rules: &RuleSet) -> Self {
        let counts_groups = rules.page_rules().iter().any(|rule| {
            rule.selectors
                .iter()
                .any(|selector| !selector.nth.is_empty() && selector.names_pages())
        });
        Self {
            on: counts_groups && tree.names_pages(),
            starts: FxHashMap::default(),
            last: Vec::new(),
        }
    }

    /// The groups of page `index` (0-based), where layout of the root box `root` resumes at
    /// `token`: those of the elements it goes on in and, unless the page is `blank`, of those
    /// that start at its top. An element makes a group where its `page` is not `auto` and a
    /// forced break comes before it (CSS Generated Content for Paged Media 3, §3.2: "when the
    /// page property is applied to an element that also has a forced break property applied, a
    /// page group is created").
    fn at(
        &mut self,
        layouter: &Layouter<'_>,
        root: BoxId,
        token: Option<&BreakToken>,
        index: usize,
        blank: bool,
    ) -> Vec<PageGroup> {
        if !self.on {
            return Vec::new();
        }
        let (mut continued, mut started) = (Vec::new(), Vec::new());
        layouter.boxes_at_page_start(root, token, &mut continued, &mut started);
        if blank {
            started.clear();
        }
        let tree = layouter.tree;
        let mut groups = Vec::new();
        for (id, starts_here) in continued
            .into_iter()
            .map(|id| (id, false))
            .chain(started.into_iter().map(|id| (id, true)))
        {
            let Some(name) = &tree.get(id).style.page else {
                continue;
            };
            if !tree.break_before(id).is_forced_page_break() {
                continue;
            }
            // An element first met where it goes on from the page before started on that page
            // (below its top, where a forced break does not apply, as in columns).
            let start = *self.starts.entry(id).or_insert(if starts_here {
                index
            } else {
                index.saturating_sub(1)
            });
            groups.push(PageGroup {
                name: Arc::clone(name),
                start,
            });
        }
        if !blank {
            self.last.clone_from(&groups);
        }
        groups
    }
}

/// The page groups that the first page of the document laid out by `layouter` is in.
pub(crate) fn first_page_groups(layouter: &Layouter<'_>, rules: &RuleSet, root: BoxId) -> Vec<PageGroup> {
    PageGroups::new(layouter.tree, rules).at(layouter, root, None, 0, false)
}

/// Paginates the box tree, on page areas `1 / shrink` times as large as the page boxes have (see
/// [`Page::scale`]). Returns the pages with the value of the page counter on each (see
/// [`PageNumbering`]).
pub fn paginate(
    tree: &BoxTree,
    fonts: &FontDb,
    rules: &RuleSet,
    root: &ComputedStyle,
    shrink: f32,
) -> (Vec<Page>, Vec<i32>) {
    // The fonts of the margin boxes (see `unloaded_for_margins`).
    let margin_fonts: Option<FontDb>;
    let mut layouter = Layouter::new(tree, fonts);
    let made_boxes = PageBoxes::default();
    let mut pages = Vec::new();
    let mut page_boxes = Vec::new();
    let mut marks = Vec::new();
    let mut logical = Vec::new();
    let Some(root_box) = tree.root else {
        return (pages, Vec::new());
    };
    let mut token: Option<BreakToken> = None;
    // The name of the last page, which pages added for positioned boxes have too.
    let mut last_name = None;
    // The fragments of all pages, and of each page's content.
    let mut fragments = 0;
    let mut counts = Vec::new();
    // The fragments of the pages so far, charged to the budget of the render while they are
    // laid out (the caller charges the pages it keeps).
    let mut held = fagus_dom::budget::hold(fagus_dom::budget::Kind::Layout, 0);
    // How far monolithic content of the pages before (an image, a line taller than a page)
    // reaches into this page: as in Chrome, which lays the page areas end to end, the content
    // that follows starts below it (Blink's monolithic overflow), on later pages where it
    // covers whole pages.
    let mut pushed = Au::ZERO;
    // Whether that overflow ends a box in a parallel flow, and the box whose content it moves
    // once more on the page where it ends (see `monolithic`).
    // (With how far the box ends below the overflowing content, which Chrome moves the content
    // after it by, while the overflow itself counts for the rest of the box.)
    let mut parallel: Option<(crate::monolithic::Parallel, Au)> = None;
    // Where the page starts in the flow of the page areas laid end to end, and how far down that
    // flow the pages must reach (an empty page added below the content where such a box records
    // overflow past the last page).
    let mut start = Au::ZERO;
    let mut reach_floor = Au::ZERO;
    // Whether the page before was left to overflow that ended right at its end, which Chrome
    // takes for reaching onto the next page where it ends a box in a parallel flow: that page
    // is left to it as well.
    let mut exact_end = false;
    // Whether the first page is a left page: in a right-to-left document, or where the content
    // starts with a forced break to a left page (CSS Paged Media 3, §4.3.2).
    let rtl = root.is_rtl();
    let first_left = forced_side(tree.break_before(root_box), rtl).unwrap_or(rtl);
    // The page counter after the pages so far.
    let mut counter = PageNumbering::default();
    let mut groups = PageGroups::new(tree, rules);
    // The footnotes, and whether the content has ended with footnotes still waiting for pages.
    let mut notes = crate::footnote::Footnotes::of(tree);
    let mut content_done = false;
    loop {
        // Pages for the footnotes left once the content has ended, which have the name of the
        // last page, as pages added for positioned boxes have.
        if content_done {
            let Some(waiting) = notes.as_mut().filter(|notes| notes.waiting()) else {
                break;
            };
            if pages.len() >= MAX_PAGES || fragments >= MAX_FRAGMENTS {
                break;
            }
            let page_type = PageType::of(pages.len(), first_left, false, &groups.last);
            let page_box = shrunk(
                made_boxes.get(rules, root, page_type, last_name.as_deref()),
                shrink,
            );
            counter = counter.number(&layouter, pages.len(), &page_box.style);
            layouter.page_number.set((pages.len() + 1, 0));
            layouter
                .initial_containing_block
                .set((page_box.content_width(), page_box.content_height()));
            let (inline, block) = crate::writing::physical_size(
                (page_box.content_width(), page_box.content_height()),
                layouter.writing_mode(),
            );
            waiting.start_page(&layouter, page_box.footnote_reset);
            let mut content = Fragment {
                rect: Rect::new(Au::ZERO, Au::ZERO, inline, Au::ZERO),
                kind: FragmentKind::Anonymous,
                children: Children::new(),
            };
            if let Some(area) = waiting.only_notes(&layouter, &page_box.footnote, inline, block) {
                place_footnote_area(&mut content, area, block);
            }
            let count = fragment_count(&content);
            fragments += count;
            held.add(count as u64 * FRAGMENT_BYTES);
            fagus_dom::budget::check_time_now();
            counts.push(count);
            marks.push(Marks::default());
            logical.push((inline, block));
            start += block;
            pages.push(Page::new(content, &page_box, tree, shrink));
            page_boxes.push(page_box);
            continue;
        }
        let name = layouter.page_name_at(root_box, token.as_ref()).cloned();
        // A forced break to a left or right page where the next page is on the other side
        // inserts a blank page (CSS Fragmentation 3, §3.1), which `@page :blank` matches and
        // which has the page name of the page after it. (Chrome takes such breaks for `page`.)
        if pushed == Au::ZERO
            && layouter.forced_break.get()
            && pages.len() < MAX_PAGES
            && let Some(left) = token
                .as_ref()
                .and_then(|token| side_at_break(tree, root_box, token, rtl))
            && left != PageType::of(pages.len(), first_left, false, &[]).left
        {
            // (A blank page is in the page groups that go on across it, not in those that start
            // after it.)
            let in_groups = groups.at(&layouter, root_box, token.as_ref(), pages.len(), true);
            let page_type = PageType::of(pages.len(), first_left, true, &in_groups);
            let page_box = shrunk(made_boxes.get(rules, root, page_type, name.as_deref()), shrink);
            let (inline, block) = crate::writing::physical_size(
                (page_box.content_width(), page_box.content_height()),
                layouter.writing_mode(),
            );
            let content = Fragment {
                rect: Rect::new(Au::ZERO, Au::ZERO, inline, Au::ZERO),
                kind: FragmentKind::Anonymous,
                children: Children::new(),
            };
            counter = counter.number(&layouter, pages.len(), &page_box.style);
            start += block;
            counts.push(1);
            fragments += 1;
            marks.push(Marks::default());
            logical.push((inline, block));
            pages.push(Page::new(content, &page_box, tree, shrink));
            page_boxes.push(page_box);
            continue;
        }
        let in_groups = groups.at(&layouter, root_box, token.as_ref(), pages.len(), false);
        let page_type = PageType::of(pages.len(), first_left, false, &in_groups);
        let made = made_boxes.get(rules, root, page_type, name.as_deref());
        // A page that grows with its content (in a horizontal writing mode) gives it room down
        // to the tallest page a PDF holds; its page area as declared is the initial containing
        // block, and how tall it is at least (see `fitted` below).
        let grows = made.fit_content && !layouter.writing_mode().is_vertical();
        let declared = shrunk(made.clone(), shrink);
        let page_box = if grows {
            shrunk(made.with_height(made.tallest()), shrink)
        } else {
            declared.clone()
        };
        last_name.clone_from(&name);
        // (The counter goes on from this page once it is added.)
        let numbered = counter.number(&layouter, pages.len(), &page_box.style);
        layouter.page_number.set((pages.len() + 1, 0));
        // Footnote areas in columns are styled as the page's (see `footnote`).
        if layouter.column_footnotes {
            layouter
                .footnote_area
                .replace(Some(Arc::clone(&page_box.footnote)));
        }
        layouter
            .initial_containing_block
            .set((declared.content_width(), declared.content_height()));
        layouter
            .after_forced_break
            .set(layouter.forced_break.replace(false));
        // The page area is the initial containing block, for percentage heights (CSS 2.1, §10.1).
        // In a vertical writing mode its height is the length of the lines, and its width the
        // room they stack in.
        let mode = layouter.writing_mode();
        let (inline, block) =
            crate::writing::physical_size((page_box.content_width(), page_box.content_height()), mode);
        let containing_block =
            crate::writing::physical_size((declared.content_width(), declared.content_height()), mode).1;
        // An absolutely positioned root moved down by its `top` on the first page has that much
        // less room there (see `root_offset`).
        let moved = crate::positioned::root_top(&tree.get(root_box).style, containing_block)
            .filter(|&top| token.is_none() && !mode.is_vertical() && top < block)
            .unwrap_or(Au::ZERO);
        let space = Space {
            // A positioned root that fills the height its `top` and `bottom` leave stretches
            // over that much (see `root_fills_height`).
            containing_height: crate::positioned::root_block_room(
                &tree.get(root_box).style,
                containing_block,
            )
            .or(Some(containing_block)),
            remaining: Some(block - pushed - moved),
            // Content below monolithic overflow may move on to the next page, as there is a
            // breakpoint before it.
            at_top: pushed == Au::ZERO,
            ..Space::unfragmented(crate::positioned::root_inline_room(
                &tree.get(root_box).style,
                inline,
                mode.is_vertical(),
            ))
        };
        // The content is laid out again where footnotes take room at the bottom of the page
        // (see `footnote`), from the same state.
        let after = layouter.after_forced_break.get();
        let attempt = |room: Au| {
            layouter.page_overflow.set(pushed);
            layouter.after_forced_break.set(after);
            layouter.forced_break.set(false);
            let space = Space {
                remaining: Some(room),
                ..space
            };
            (pushed < block)
                .then(|| layouter.block_level(root_box, space, token.as_ref()))
                .flatten()
        };
        let room = block - pushed - moved;
        let (outcome, area) = match notes.as_mut() {
            Some(notes) if pushed < block => {
                notes.start_page(&layouter, page_box.footnote_reset);
                notes.fit(&layouter, &page_box.footnote, inline, room, &attempt)
            }
            _ => (attempt(room), None),
        };
        // A page after a forced break may hold nothing but the margins kept there, where the
        // content below them does not fit: it comes on the next page, below truncated margins, as
        // in Chrome, so the same token goes on once more.
        let after_forced = layouter.after_forced_break.get();
        let outcome =
            outcome.filter(|outcome| pushed == Au::ZERO || outcome.token.as_ref() != token.as_ref());
        let Some(outcome) = outcome else {
            // Footnotes moved from earlier pages take the whole page: the content goes on on
            // the next.
            if let Some(area) = area
                && pages.len() < MAX_PAGES
                && fragments < MAX_FRAGMENTS
            {
                let mut content = Fragment {
                    rect: Rect::new(Au::ZERO, Au::ZERO, inline, Au::ZERO),
                    kind: FragmentKind::Anonymous,
                    children: Children::new(),
                };
                place_footnote_area(&mut content, area, block);
                let count = fragment_count(&content);
                fragments += count;
                held.add(count as u64 * FRAGMENT_BYTES);
                fagus_dom::budget::check_time_now();
                counts.push(count);
                marks.push(Marks::default());
                logical.push((inline, block));
                start += block;
                pages.push(Page::new(content, &page_box, tree, shrink));
                page_boxes.push(page_box);
                counter = numbered;
                layouter.forced_break.set(layouter.after_forced_break.get());
                continue;
            }
            if pushed == Au::ZERO || pages.len() >= MAX_PAGES || fragments >= MAX_FRAGMENTS {
                break;
            }
            // A page that the overflow covers, or whose content does not fit below it, holds
            // nothing of its own. Where the overflow ends on it, a box ending with it moves the
            // content of its parent on the next page (see `monolithic`).
            let ends = pushed < block;
            // (Only overflow that ends a box in a parallel flow does: monolithic content in the
            // flow of its parent that goes on, a line taller than the page among them, leaves
            // the next page to what follows it, as in Chrome 157.)
            let exact = pushed == block && !exact_end && parallel.is_some();
            pushed = match parallel {
                Some((crate::monolithic::Parallel::Moves(_), below)) if ends => {
                    ((pushed - below) * 2 - block).max(Au::ZERO)
                }
                _ if exact => block,
                _ => (pushed - block).max(Au::ZERO),
            };
            exact_end = exact;
            if ends {
                parallel = None;
            }
            start += block;
            let content = Fragment {
                rect: Rect::new(Au::ZERO, Au::ZERO, inline, Au::ZERO),
                kind: FragmentKind::Anonymous,
                children: Children::new(),
            };
            counts.push(1);
            fragments += 1;
            marks.push(Marks::default());
            logical.push((inline, block));
            pages.push(Page::new(content, &page_box, tree, shrink));
            page_boxes.push(page_box);
            counter = numbered;
            continue;
        };
        let mut content = outcome.fragment;
        // The root element's top margin, which collapses with no other, starts the first page.
        if token.is_none() {
            content.rect.y += outcome.collapsed_top.resolve();
        }
        // A positioned root moves by or to its offsets.
        if !mode.is_vertical() {
            let (dx, dy) = crate::positioned::root_offset(
                &tree.get(root_box).style,
                content.rect,
                (inline, containing_block),
                token.is_none(),
            );
            content.rect.x += dx;
            content.rect.y += dy;
        }
        content.rect.y += pushed;
        // A page that grows with its content ends below it: below the root element's margin
        // box where the content ends on the page, below what ends on it where the content
        // breaks (at a forced break, or past the tallest page), or below the monolithic content
        // that reaches further.
        // On a page that grows, the footnote area follows the content and the page grows with it.
        let mut footnotes = area;
        let page_box = if grows {
            let root_end = if outcome.token.is_none() {
                content.rect.y + content.rect.height + outcome.collapsed_bottom.resolve()
            } else {
                flow_bottom(&content, Au::ZERO)
            };
            let bottom = crate::monolithic::content_bottom(&content, Au::ZERO)
                .map_or(root_end, |bottom| bottom.max(root_end));
            let extent = footnotes.as_ref().map_or(Au::ZERO, |area| area.extent);
            if let Some(area) = footnotes.take() {
                place_footnote_area(&mut content, area, bottom + extent);
            }
            shrunk(
                made.fitted(Au::from_px((bottom + extent).to_px() * shrink)),
                shrink,
            )
        } else {
            page_box
        };
        if let Some(area) = footnotes {
            place_footnote_area(&mut content, area, block);
        }
        let page_block =
            crate::writing::physical_size((page_box.content_width(), page_box.content_height()), mode).1;
        let count = fragment_count(&content);
        fragments += count;
        held.add(count as u64 * FRAGMENT_BYTES);
        fagus_dom::budget::check_time_now();
        counts.push(count);
        let mut page_marks = Marks::default();
        // (Where the page's assignments are in the string log, for `string()` in the content of
        // this page and the pages after.)
        let mut logged = Vec::new();
        page_marks.collect(tree, &content, &mut false, &mut logged);
        let mut string_pages = layouter.string_pages.borrow_mut();
        for at in logged {
            string_pages[at] = pages.len() + 1;
        }
        drop(string_pages);
        marks.push(page_marks);
        // The page area in the coordinates of the writing mode the content is laid out in.
        logical.push((inline, page_block));
        // Monolithic content that reaches past the end of the page area pushes the content that
        // follows on the next pages; the root box grows to hold it.
        let resumed = std::mem::take(&mut pushed);
        let ended = parallel.take();
        if let Some(bottom) =
            crate::monolithic::content_bottom(&content, Au::ZERO).filter(|&bottom| bottom > block)
        {
            pushed = bottom - block;
            parallel = crate::monolithic::parallel_overflow(&content, bottom);
        }
        // Overflow from the pages before that ends here, at the end of a box with its parent's
        // content going on after it: the parent moves the rest of the box down by the overflow
        // once more, from where it starts on this page, and its content on the next page starts
        // as far below the top as that reaches past the end of this one (see `monolithic`).
        if let Some((holder, below)) = ended
            && (resumed - below) * 2 > block
        {
            let resumed = resumed - below;
            let extra = resumed * 2 - block;
            if let crate::monolithic::Parallel::Moves(node) = holder
                && crate::monolithic::continues(&content, node)
                && extra > pushed
            {
                pushed = extra;
                parallel = None;
            }
            reach_floor = reach_floor.max(start + resumed * 2);
        }
        start += page_block;
        pages.push(Page::new(content, &page_box, tree, shrink));
        page_boxes.push(page_box);
        counter = numbered;
        match outcome.token {
            Some(next)
                if (Some(&next) != token.as_ref() || after_forced)
                    && pages.len() < MAX_PAGES
                    && fragments < MAX_FRAGMENTS =>
            {
                token = Some(next);
            }
            // Footnotes left over get pages of their own.
            None if notes.as_ref().is_some_and(crate::footnote::Footnotes::waiting)
                && pages.len() < MAX_PAGES
                && fragments < MAX_FRAGMENTS =>
            {
                content_done = true;
            }
            _ => break,
        }
    }
    // Positioned boxes are laid out once all pages are, when it is known where the fixed boxes
    // have their static positions.
    let areas: Vec<Rect> = logical
        .iter()
        .map(|&(inline, block)| Rect::new(Au::ZERO, Au::ZERO, inline, block))
        .collect();
    // Positioned boxes whose containing block is the initial one are placed in the viewport (in
    // horizontal writing modes; vertical ones keep the page area): that of the first page on
    // every page, as in Chrome, where `@page :first` margins size it for the pages after too,
    // and pages of another size or orientation (named pages) have it at the corner of their
    // own page area.
    let viewports: Vec<Rect> = page_boxes
        .iter()
        .zip(&areas)
        .map(|(page_box, &area)| {
            if layouter.writing_mode().is_vertical() {
                area
            } else {
                let viewport = initial_viewport(&page_boxes, page_box);
                Rect::new(Au::ZERO, Au::ZERO, viewport.0, viewport.1)
            }
        })
        .collect();
    let mut statics = crate::positioned::FixedStatics::default();
    // Where the content of the pages lies in the flow of pages, for the sticky boxes. Chrome
    // keeps the offsets it gave them on the screen it loaded the document on, which are those
    // of the flow of pages where the screen lays the document out as paper does; where rules
    // for the screen only lay it out otherwise, those after the first page keep their places.
    let stitched = (!rules.has_screen_rules()).then(|| {
        crate::sticky::Stitched::of(pages.iter().zip(&areas).filter_map(|(page, area)| {
            let content = page.fragments.first()?;
            Some((content, area.y - content.rect.y))
        }))
    });
    for (index, (page, &area)) in pages.iter_mut().zip(&areas).enumerate() {
        if let Some(content) = page.fragments.first_mut() {
            layouter.prepare_page(content, area, (index, stitched.as_ref()), &mut statics);
        }
    }
    let mode = layouter.writing_mode();
    // Pages beyond the fragments the pages may hold, with the positioned boxes repeated on each
    // (fixed boxes) and the margin boxes, are left out.
    let mut kept = pages.len();
    // How tall the page areas are, laid end to end in the flow of the document.
    let heights: Vec<Au> = areas.iter().map(|area| area.height).collect();
    // Absolutely positioned boxes are placed in the flow of pages, as in Chrome, where the pages
    // are fragments of one flow (in horizontal writing modes).
    let mut flow = (!mode.is_vertical()).then(|| {
        PageFlow::new(
            pages.iter().filter_map(|page| page.fragments.first()),
            heights.clone(),
        )
    });
    // Where the content of each page lies in its page area, before it is moved onto the page.
    let mut origins = vec![(Au::ZERO, Au::ZERO); pages.len()];
    for (index, ((page, page_box), (&area, &viewport))) in pages
        .iter_mut()
        .zip(&page_boxes)
        .zip(areas.iter().zip(&viewports))
        .enumerate()
    {
        if fragments > MAX_FRAGMENTS.saturating_mul(2) {
            kept = index;
            break;
        }
        layouter.page_number.set((index + 1, 0));
        layouter
            .initial_containing_block
            .set((page_box.content_width(), page_box.content_height()));
        if let Some(content) = page.fragments.first_mut() {
            if let Some(flow) = &mut flow {
                flow.page = index;
            }
            layouter.position(content, viewport, Some((index, &statics)), flow.as_mut());
            // The sticky boxes in the absolutely positioned boxes placed on the page have the
            // window in the flow of pages as their viewport too.
            if let Some(stitched) = stitched.as_ref().filter(|_| !mode.is_vertical()) {
                let (width, height) = crate::sticky::WINDOW;
                crate::sticky::apply_in_absolute(
                    content,
                    (
                        area.x - content.rect.x,
                        area.y - content.rect.y,
                        Au::from_px(width),
                        Au::from_px(height),
                    ),
                    crate::sticky::OnPage {
                        stitched,
                        page: index,
                        area_top: area.y - content.rect.y,
                    },
                );
            }
            origins[index] = (content.rect.x, content.rect.y);
            // Then the content of a vertical document is turned onto the page, and all of it
            // moved into the page area.
            if mode.is_vertical() {
                crate::writing::to_physical(content, mode, (area.width, area.height), &tree.flows);
            }
            content.rect.x += page_box.area.x;
            content.rect.y += page_box.area.y;
            fragments += fragment_count(content).saturating_sub(counts[index]);
        }
    }
    // The boxes that go on other pages than their placeholders', broken where they cross the
    // end of a page; pages are added for those below the last, as Chrome adds them. The boxes
    // positioned inside them are then placed in the flow of pages in turn, which may move some
    // of those onto other pages again. Those go into the empty fragments made for their
    // containing blocks on pages where those have none (by block and page: their index in the
    // page's content and their border box origin in the page area).
    let mut pieces: FxHashMap<(crate::positioned::ChainKey, usize), (usize, (Au, Au))> = FxHashMap::default();
    for _ in 0..16 {
        let Some(flow) = flow
            .as_mut()
            .filter(|flow| !flow.out.is_empty() && kept == pages.len())
        else {
            break;
        };
        let flowing = std::mem::take(&mut flow.out);
        let starts = &flow.starts;
        let heights = &flow.heights;
        // The pages to be added below the last alternate between right and left ones, which may
        // differ in height (`:left` and `:right` margins). As in Chrome, a box goes on the page
        // its top falls on with the pages after the last as tall as the last one, and breaks
        // where the page it goes on ends.
        let added = [0, 1].map(|n| {
            shrunk(
                made_boxes.get(
                    rules,
                    root,
                    PageType::of(starts.len() + n, first_left, false, &groups.last),
                    last_name.as_deref(),
                ),
                shrink,
            )
            .content_height()
        });
        let last_height = heights.last().copied().unwrap_or(added[0]);
        let height_at = |page: usize| {
            heights
                .get(page)
                .copied()
                .unwrap_or_else(|| added[(page - starts.len()) % 2])
        };
        let end_of_pages = starts.last().copied().unwrap_or(Au::ZERO) + last_height;
        let start_at = |page: usize| {
            starts.get(page).copied().unwrap_or_else(|| {
                end_of_pages + last_height * i32::try_from(page - starts.len()).unwrap_or(i32::MAX)
            })
        };
        let page_at = |y: Au| {
            if y < end_of_pages || last_height <= Au::ZERO {
                flow.page_at(y)
            } else {
                starts.len() + usize::try_from((y - end_of_pages).0 / last_height.0).unwrap_or(0)
            }
        };
        let mut placed = Vec::new();
        for item in flowing {
            let (top, height) = (item.fragment.rect.y, item.fragment.rect.height);
            let target = page_at(top.max(Au::ZERO)).min(MAX_PAGES - 1);
            let end = start_at(target) + height_at(target);
            // A box that starts at the top of its page holds monolithic content there that does
            // not fit, as content at the top of a page does.
            let item = Flowing {
                requires_content: item.requires_content || top <= start_at(target),
                pushes_overflow: true,
                ..item
            };
            let parts = (top + height > end || layouter.breaks_inside(item.id))
                .then(|| {
                    layouter.break_absolute(&item, |n| if n == 0 { end - top } else { height_at(target + n) })
                })
                .flatten();
            match parts {
                Some(parts) => {
                    for (n, mut part) in parts.into_iter().enumerate() {
                        if n > 0 {
                            // (A part below monolithic overflow starts below it, see `break_absolute`.)
                            part.rect.y = start_at(target + n) + (part.rect.y - item.fragment.rect.y);
                        }
                        placed.push((target + n, part, item.chain));
                    }
                }
                None => placed.push((target, item.fragment, item.chain)),
            }
        }
        // The pages added below the last, with the fixed boxes that every page repeats.
        let last = placed.iter().map(|(page, ..)| *page).max().unwrap_or(0);
        let extent: Vec<(Au, Au)> = (pages.len()..=last.min(MAX_PAGES - 1))
            .map(|page| (start_at(page), height_at(page)))
            .collect();
        for (start, height) in extent {
            let page_box = shrunk(
                made_boxes.get(
                    rules,
                    root,
                    PageType::of(pages.len(), first_left, false, &groups.last),
                    last_name.as_deref(),
                ),
                shrink,
            );
            counter = counter.number(&layouter, pages.len(), &page_box.style);
            pages.push(added_page(&layouter, &page_box, &page_boxes, &statics, shrink));
            page_boxes.push(page_box);
            origins.push((Au::ZERO, Au::ZERO));
            marks.push(Marks::default());
            flow.starts.push(start);
            flow.heights.push(height);
        }
        kept = pages.len();
        // Each part goes in the fragment of its containing block on its page, else in the page's
        // content; it then holds the boxes positioned inside it there.
        let mut touched = Vec::new();
        for (page, mut part, chain) in placed {
            // A containing block without a fragment on the page gets an empty one at its top
            // (see `empty_piece`).
            let piece = chain.and_then(|key| {
                if let Some(&found) = pieces.get(&(key, page)) {
                    return Some(found);
                }
                let parts = flow.chains.get(&key)?;
                if parts.iter().any(|part| part.page == page) {
                    return None;
                }
                let near = crate::empty_piece::nearest(parts, page)?;
                let source = pages.get(near.page)?.fragments.first().and_then(|content| {
                    near.path
                        .iter()
                        .try_fold(content, |fragment, &index| fragment.children.get(index))
                })?;
                let (fragment, origin) =
                    crate::empty_piece::empty_piece(source, near.origin, *origins.get(page)?)?;
                let content = pages.get_mut(page)?.fragments.first_mut()?;
                content.children.push(fragment);
                let found = (content.children.len() - 1, origin);
                pieces.insert((key, page), found);
                Some(found)
            });
            let Some(content) = pages.get_mut(page).and_then(|page| page.fragments.first_mut()) else {
                continue;
            };
            let start = flow.starts.get(page).copied().unwrap_or(Au::ZERO);
            let (path, origin) = chain
                .and_then(|key| flow.chains.get(&key))
                .and_then(|parts| parts.iter().find(|part| part.page == page))
                .map(|part| (part.path.clone(), part.origin))
                .or_else(|| piece.map(|(index, origin)| (vec![index], origin)))
                .unwrap_or((Vec::new(), origins[page]));
            // Its border box origin in the coordinates of the page's content before it was moved
            // onto the page.
            let at = (part.rect.x, part.rect.y - start);
            part.rect.x -= origin.0;
            part.rect.y -= start + origin.1;
            fragments += fragment_count(&part);
            let parent = path
                .iter()
                .try_fold(&mut *content, |fragment, &index| fragment.children.get_mut(index));
            if let Some(parent) = parent {
                let mut path = path;
                path.push(parent.children.len());
                let mut chains = Vec::new();
                crate::positioned::chain_parts(&part, page, &mut path, at, &mut chains);
                for (key, chain_part) in chains {
                    let parts = flow.chains.entry(key).or_default();
                    parts.push(chain_part);
                    parts.sort_by_key(|part| part.page);
                }
                parent.children.push(part);
                touched.push(page);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for page in touched {
            let (Some(content), Some(page_box)) = (
                pages.get_mut(page).and_then(|page| page.fragments.first_mut()),
                page_boxes.get(page),
            ) else {
                continue;
            };
            let before = fragment_count(content);
            layouter.page_number.set((page + 1, 0));
            flow.page = page;
            content.rect.x -= page_box.area.x;
            content.rect.y -= page_box.area.y;
            let viewport = initial_viewport(&page_boxes, page_box);
            layouter.place_absolute(
                content,
                Rect::new(Au::ZERO, Au::ZERO, viewport.0, viewport.1),
                true,
                Some(flow),
            );
            content.rect.x += page_box.area.x;
            content.rect.y += page_box.area.y;
            fragments += fragment_count(content).saturating_sub(before);
        }
    }
    // Content that reaches past the end of its page area shows on the pages after it, which are
    // added where it reaches below the last page, as Chrome adds them.
    if !mode.is_vertical() && kept == pages.len() && fragments < MAX_FRAGMENTS {
        // The parts of each page's content below the end of its page area, from the top of it.
        let last = pages.len().saturating_sub(1);
        let overflows: Vec<Vec<(Au, Au)>> = pages
            .iter()
            .zip(&page_boxes)
            .enumerate()
            .map(|(index, (page, page_box))| {
                let mut parts = Vec::new();
                if let Some(content) = page.fragments.first() {
                    let end = page_box.area.y + page_box.content_height();
                    overflow_parts(content, (Au::ZERO, false), None, end, &mut parts);
                    // A fixed box that lies wholly below the page area of a document of one page
                    // is content of the document there, for which Chrome adds the page its top
                    // falls on (and shows it there), unlike on the last of more pages.
                    if index == 0 && last == 0 {
                        fixed_below(content, Au::ZERO, end, &mut parts);
                    }
                }
                parts
                    .into_iter()
                    .map(|(top, bottom)| (top - page_box.area.y, bottom - page_box.area.y))
                    .collect()
            })
            .collect();
        let mut starts = Vec::with_capacity(pages.len());
        let mut end = Au::ZERO;
        for page_box in &page_boxes {
            starts.push(end);
            end += page_box.content_height();
        }
        // Overflow that ends right at the end of the last page reaches onto a page after it, as
        // in Chrome.
        let reach = starts
            .iter()
            .zip(&overflows)
            .filter_map(|(&start, parts)| parts.iter().map(|&(_, bottom)| start + bottom).max())
            .max()
            .map_or(Au::ZERO, |reach| reach + Au::EPSILON)
            .max(reach_floor);
        while end < reach && pages.len() < MAX_PAGES && fragments < MAX_FRAGMENTS {
            let page_box = shrunk(
                made_boxes.get(
                    rules,
                    root,
                    PageType::of(pages.len(), first_left, false, &groups.last),
                    last_name.as_deref(),
                ),
                shrink,
            );
            if page_box.content_height() <= Au::ZERO {
                break;
            }
            counter = counter.number(&layouter, pages.len(), &page_box.style);
            let page = added_page(&layouter, &page_box, &page_boxes, &statics, shrink);
            fragments += page.fragments.iter().map(fragment_count).sum::<usize>();
            pages.push(page);
            starts.push(end);
            end += page_box.content_height();
            page_boxes.push(page_box);
            marks.push(Marks::default());
        }
        kept = pages.len();
        // Each page shows the parts of the content of the pages before that reach onto it.
        let sorted = starts.len() >= pages.len() && starts.is_sorted();
        // Each part carried onto a page is a copy of what shows there: their number is bounded
        // as that of fragments is, where content of every page reaches over thousands of pages
        // (each page counting the parts that reach onto it, the fragments copied there).
        let mut budget = MAX_FRAGMENTS;
        for (from, parts) in overflows.iter().enumerate() {
            // The pages each part reaches onto, a run of pages from the first below its top to
            // the last above its bottom; the runs joined where they overlap.
            let runs: Vec<(usize, usize)> = parts
                .iter()
                .map(|&(top, bottom)| {
                    let (top, bottom) = (starts[from] + top, starts[from] + bottom);
                    let first = starts
                        .partition_point(|&start| start <= top)
                        .saturating_sub(1)
                        .max(from + 1);
                    let end = if sorted {
                        starts[..pages.len()].partition_point(|&start| start < bottom)
                    } else {
                        (first..pages.len())
                            .take_while(|&to| starts[to] < bottom)
                            .last()
                            .map_or(first, |to| to + 1)
                    };
                    (first, end)
                })
                .filter(|&(first, end)| first < end)
                .collect();
            let onto = reached_pages(&runs, &mut budget);
            let source = page_boxes[from].area;
            for to in onto {
                let (area, distance) = (page_boxes[to].area, starts[to] - starts[from]);
                pages[to].carried.push(Carried {
                    page: from,
                    shift: (area.x - source.x, area.y - source.y - distance),
                    from: source.y + distance,
                    to: source.y + distance + page_boxes[to].content_height(),
                });
            }
        }
    }
    pages.truncate(kept.max(1));
    layouter.page_values.borrow_mut().truncate(pages.len());
    // Absolutely positioned boxes go into the stacking contexts that paint them.
    let owners = layouter.stacking_owners.borrow();
    for content in pages.iter_mut().filter_map(|page| page.fragments.first_mut()) {
        crate::stacking_owner::move_to_owners(content, &owners);
    }
    drop(owners);
    margin_fonts = unloaded_for_margins(tree, fonts, &pages);
    if let Some(margin_fonts) = &margin_fonts {
        layouter.fonts = margin_fonts;
    }
    // Margin boxes are set after pagination, when the page count is known: "page X of Y" needs no
    // second layout pass.
    let total = pages.len();
    let mut entry = Entry::default();
    let values = layouter.page_values.borrow().clone();
    for (index, ((page, page_box), marks)) in pages.iter_mut().zip(&page_boxes).zip(&marks).enumerate() {
        layouter.page_number.set((index + 1, total));
        let marks = &marks.counted((index + 1, total), &values, &tree.counter_styles);
        // The margin boxes are not shrunk: they go in the page box's own page area.
        let page_box = &PageBox {
            area: page_box.area_unshrunk,
            ..page_box.clone()
        };
        let boxes = margin_boxes(&layouter, page_box, marks, &entry);
        fragments += boxes.iter().map(fragment_count).sum::<usize>();
        page.fragments.extend(boxes);
        entry.update(marks);
        if fragments > MAX_FRAGMENTS.saturating_mul(2) {
            // Margin boxes are set once the page count is known; past this, the pages after go
            // without theirs rather than take gigabytes.
            break;
        }
    }
    let numbers = layouter.page_values.take();
    (pages, numbers)
}

/// Puts a footnote area at the end of the page area, `block` long, into the content of the page.
fn place_footnote_area(content: &mut Fragment, area: crate::footnote::Area, block: Au) {
    let mut fragment = area.fragment;
    fragment.rect.x -= content.rect.x;
    fragment.rect.y += block - area.extent - content.rect.y;
    content.children.push(fragment);
}

impl Page {
    /// A page of `page_box` holding `content`, laid out on a page area `1 / shrink` times as large.
    fn new(content: Fragment, page_box: &PageBox, tree: &BoxTree, shrink: f32) -> Self {
        Self {
            width: page_box.width,
            height: page_box.height,
            fragments: vec![content],
            margin: page_box.margin,
            style: Arc::clone(&page_box.style),
            canvas: tree.canvas.clone(),
            bleed: page_box.bleed,
            marks: page_box.marks,
            orientation: page_box.orientation,
            scale: shrink,
            // As tall as it is before it is rounded up to whole pixels: Chrome cuts the content
            // off there, a fraction of a pixel above the end of the page it lays it out on.
            area: Rect {
                height: Au::from_px(page_box.exact.1),
                ..page_box.area_unshrunk
            },
            shift: (page_box.content_x - page_box.area.x).to_px(),
            carried: Vec::new(),
        }
    }
}

/// How far down the content in the flow of a fragment lying `origin` below the top of its page
/// reaches: the bottom of the boxes, lines and images that end on the page, through the boxes
/// that go on to the next one (which reach down to its end), not into positioned boxes.
fn flow_bottom(fragment: &Fragment, origin: Au) -> Au {
    let top = origin + fragment.rect.y;
    match &fragment.kind {
        FragmentKind::Box { style, .. } if style.is_out_of_flow() => Au::ZERO,
        FragmentKind::Box { sides, .. } if sides.bottom => top + fragment.rect.height,
        FragmentKind::Box { .. } | FragmentKind::Anonymous => fragment
            .children
            .iter()
            .map(|child| flow_bottom(child, top))
            .max()
            .unwrap_or(top),
        FragmentKind::Placeholder(_) => Au::ZERO,
        _ => top + fragment.rect.height,
    }
}

/// The pages that the runs of pages `runs` (each `first..end`, those that a part of a page's
/// content reaches onto) cover together, in order, as far as `budget` goes: each page takes as
/// much of it as there are parts reaching onto it.
fn reached_pages(runs: &[(usize, usize)], budget: &mut usize) -> Vec<usize> {
    // Where the number of runs covering a page changes, in order.
    let mut changes: Vec<(usize, isize)> = runs
        .iter()
        .flat_map(|&(first, end)| [(first, 1), (end, -1)])
        .collect();
    changes.sort_unstable();
    let mut onto = Vec::new();
    let mut covering: isize = 0;
    for (index, &(page, change)) in changes.iter().enumerate() {
        covering += change;
        let next = changes.get(index + 1).map_or(page, |&(next, _)| next);
        if covering <= 0 {
            continue;
        }
        let parts = covering.unsigned_abs();
        for to in page..next {
            if *budget < parts {
                *budget = 0;
                return onto;
            }
            *budget -= parts;
            onto.push(to);
        }
    }
    onto
}

/// A page added below the last for positioned boxes or overflow: one of `page_box` that holds
/// only the fixed boxes every page repeats.
fn added_page(
    layouter: &Layouter<'_>,
    page_box: &PageBox,
    page_boxes: &[PageBox],
    statics: &crate::positioned::FixedStatics,
    shrink: f32,
) -> Page {
    let index = page_boxes.len();
    let mut content = Fragment {
        rect: Rect::new(Au::ZERO, Au::ZERO, page_box.content_width(), Au::ZERO),
        kind: FragmentKind::Anonymous,
        children: Children::new(),
    };
    layouter.page_number.set((index + 1, 0));
    let viewport = initial_viewport(page_boxes, page_box);
    layouter.position(
        &mut content,
        Rect::new(Au::ZERO, Au::ZERO, viewport.0, viewport.1),
        Some((index, statics)),
        None,
    );
    content.rect.x += page_box.area.x;
    content.rect.y += page_box.area.y;
    Page::new(content, page_box, layouter.tree, shrink)
}

/// Collects the parts of the content of `fragment` (lying `origin` below the top of the page)
/// that reach below `end`, as far as its scrollable overflow goes: its monolithic content (lines,
/// images, boxes laid out whole, and all boxes inside those, `whole`), cut to the boxes around
/// them that clip their overflow (`clip`), and not fixed boxes, which every page repeats.
fn overflow_parts(
    fragment: &Fragment,
    (origin, whole): (Au, bool),
    clip: Option<(Au, Au)>,
    end: Au,
    parts: &mut Vec<(Au, Au)>,
) {
    let top = origin + fragment.rect.y;
    let bottom = top + fragment.rect.height;
    let mut push = |(top, bottom): (Au, Au)| {
        let (top, bottom) = clip.map_or((top, bottom), |(above, below)| {
            (top.max(above), bottom.min(below))
        });
        if bottom > end && bottom > top {
            parts.push((top, bottom));
        }
    };
    match &fragment.kind {
        // A line that ends right at the end of a page reaches no further, unlike an image (see
        // `reach` in `paginate`): no page is added for it, as in Chrome 157 (WPT css-page
        // monolithic-overflow-024). Nor is one for a box laid out whole (031).
        FragmentKind::Line { .. } => push((top, bottom - Au::EPSILON)),
        FragmentKind::Image(_) => push((top, bottom)),
        FragmentKind::Text(_) | FragmentKind::Placeholder(_) => {}
        FragmentKind::Box { style, .. } if style.position == fagus_style::values::Position::Fixed => {}
        FragmentKind::Box { style, .. } if style.line_clamp.is_some() => push((top, bottom)),
        FragmentKind::Box { .. } | FragmentKind::Anonymous => {
            // A box whose size its content does not change is laid out whole, as large as it
            // says, and the boxes in it as large as they are: their content may reach further
            // still, as in Chrome.
            let whole = whole
                || matches!(&fragment.kind, FragmentKind::Box { style, .. }
                    if style.size_containment().0 && style.size_containment().1);
            // A table cell does not break before its bottom padding and border (Blink prevents
            // that, as it would upset the stretching of rows): where they do not fit below its
            // content, the cell grows past the end of the page, and Chrome adds pages for them.
            let cell = matches!(&fragment.kind, FragmentKind::Box { style, .. }
                if style.display == fagus_style::values::Display::Internal(
                    fagus_style::values::InternalDisplay::TableCell));
            if (whole || cell) && matches!(fragment.kind, FragmentKind::Box { .. }) {
                // (Ending right at the end of a page, a box laid out whole reaches no further, as
                // a line.)
                push((top, if whole { bottom - Au::EPSILON } else { bottom }));
            }
            let clip = match &fragment.kind {
                FragmentKind::Box { style, .. }
                    if style.overflow_y != fagus_style::values::Overflow::Visible =>
                {
                    let (above, below) = clip.unwrap_or((top, bottom));
                    Some((above.max(top), below.min(bottom)))
                }
                _ => clip,
            };
            for child in &fragment.children {
                overflow_parts(child, (top, whole), clip, end, parts);
            }
        }
    }
}

/// Collects the tops of the fixed boxes in `fragment` (lying `origin` below the top of the page)
/// that start at or below `end`, the end of the page area, as parts reaching just past them.
fn fixed_below(fragment: &Fragment, origin: Au, end: Au, parts: &mut Vec<(Au, Au)>) {
    let top = origin + fragment.rect.y;
    match &fragment.kind {
        FragmentKind::Box { style, .. } if style.position == fagus_style::values::Position::Fixed => {
            if top >= end && fragment.rect.height > Au::ZERO {
                parts.push((top, top + Au::EPSILON));
            }
        }
        FragmentKind::Box { .. } | FragmentKind::Anonymous => {
            for child in &fragment.children {
                fixed_below(child, top, end, parts);
            }
        }
        _ => {}
    }
}

/// The viewport of a page with the page box `page_box` among `page_boxes`: that of the first
/// page, as Chrome sizes the initial containing block of fixed boxes by the first page, on pages
/// of any size.
fn initial_viewport(page_boxes: &[PageBox], page_box: &PageBox) -> (Au, Au) {
    page_boxes.first().unwrap_or(page_box).viewport
}

/// A page box whose page area (and viewport) is `1 / shrink` times as large for the content, its
/// own kept for the margin boxes.
fn shrunk(mut page_box: PageBox, shrink: f32) -> PageBox {
    if shrink < 1.0 && shrink > 0.0 {
        let (width, height) = page_box.exact_scaled(1.0 / shrink);
        page_box.area.width = Au::from_px(width.ceil());
        page_box.area.height = Au::from_px(height.ceil());
        page_box.viewport = (Au::from_px(width.floor()), Au::from_px(height.floor()));
    }
    page_box
}

/// An assignment to a named string or of a running element on a page.
struct Assignment<T> {
    name: Arc<str>,
    value: T,
    /// Whether nothing precedes it on the page (for `start`).
    at_start: bool,
}

/// What the content of a page assigns to named strings and running elements, in document order:
/// the values of the named strings as elements set them (`StringValue`), or with the page
/// counters in them counted (`String`).
#[derive(Default)]
struct Marks<S = String> {
    strings: Vec<Assignment<S>>,
    elements: Vec<Assignment<BoxId>>,
}

impl Marks<StringValue> {
    /// Collects the assignments in a page's fragments, before the placeholders of running
    /// elements are cleared. `content_seen` tells whether text or images came before; `logged`
    /// takes where the string log has each assignment to a named string.
    fn collect(
        &mut self,
        tree: &BoxTree,
        fragment: &Fragment,
        content_seen: &mut bool,
        logged: &mut Vec<usize>,
    ) {
        match &fragment.kind {
            // Only the first fragment of an element assigns; a box continued from the previous
            // page did so there.
            FragmentKind::Box {
                node: Some(node),
                sides,
                ..
            } if sides.top && sides.left => {
                let leading = tree.leading_strings.get(node).into_iter().flatten();
                for element in leading.chain(std::iter::once(node)) {
                    let values = tree.strings.get(element).into_iter().flatten();
                    if let Some(&start) = tree.string_starts.get(element) {
                        logged.extend(start..start + values.clone().count());
                    }
                    for (name, value) in values {
                        self.strings.push(Assignment {
                            name: Arc::clone(name),
                            value: value.clone(),
                            at_start: !*content_seen,
                        });
                    }
                }
            }
            FragmentKind::Placeholder(id) => {
                if let Some(name) = &tree.get(*id).style.running {
                    self.elements.push(Assignment {
                        name: Arc::clone(name),
                        value: *id,
                        at_start: !*content_seen,
                    });
                }
                return;
            }
            FragmentKind::Text(_) | FragmentKind::Image(_) => *content_seen = true,
            _ => {}
        }
        for child in &fragment.children {
            self.collect(tree, child, content_seen, logged);
        }
        // Elements with `display: none` after the last box assign where the root element ends.
        if let FragmentKind::Box {
            node: Some(node),
            sides,
            ..
        } = &fragment.kind
            && sides.bottom
            && !tree.trailing_strings.is_empty()
            && tree.root.and_then(|root| tree.get(root).node) == Some(*node)
        {
            let end = tree.string_log.len();
            logged.extend(end - tree.trailing_strings.len()..end);
            for (name, value) in &tree.trailing_strings {
                self.strings.push(Assignment {
                    name: Arc::clone(name),
                    value: value.clone(),
                    at_start: !*content_seen,
                });
            }
        }
    }

    /// The assignments with the page counters in the values of named strings counted for page
    /// `page` (from 1) of `total`, where the values of the page counter are `values`: those of
    /// the page the element that sets the string is on, wherever the string shows (CSS GCPM 3,
    /// §1.1.1).
    fn counted(&self, (page, total): (usize, usize), values: &[i32], styles: &CounterStyles) -> Marks {
        Marks {
            strings: self
                .strings
                .iter()
                .map(|assignment| Assignment {
                    name: Arc::clone(&assignment.name),
                    value: assignment.value.resolve(
                        |counter| crate::tree::page_counter_value(counter, (page, total), values, None),
                        styles,
                    ),
                    at_start: assignment.at_start,
                })
                .collect(),
            elements: self
                .elements
                .iter()
                .map(|assignment| Assignment {
                    name: Arc::clone(&assignment.name),
                    value: assignment.value,
                    at_start: assignment.at_start,
                })
                .collect(),
        }
    }
}

/// The values named strings and running elements have when a page starts: the last ones
/// assigned on the pages before.
#[derive(Default)]
struct Entry {
    strings: FxHashMap<Arc<str>, String>,
    elements: FxHashMap<Arc<str>, BoxId>,
}

impl Entry {
    fn update(&mut self, marks: &Marks) {
        for assignment in &marks.strings {
            self.strings
                .insert(Arc::clone(&assignment.name), assignment.value.clone());
        }
        for assignment in &marks.elements {
            self.elements
                .insert(Arc::clone(&assignment.name), assignment.value);
        }
    }
}

/// The value a margin box shows for `string(name, which)` or `element(name, which)` (CSS GCPM 3,
/// §1.2 and §2.2).
fn pick<'a, T>(
    assignments: &'a [Assignment<T>],
    entry: Option<&'a T>,
    name: &str,
    which: StringWhich,
) -> Option<&'a T> {
    let mut on_page = assignments.iter().filter(|assignment| &*assignment.name == name);
    match which {
        StringWhich::First => on_page.next().map(|assignment| &assignment.value).or(entry),
        StringWhich::Last => on_page.last().map(|assignment| &assignment.value).or(entry),
        StringWhich::Start => match on_page.next() {
            Some(first) if first.at_start => Some(&first.value),
            _ => entry,
        },
        StringWhich::FirstExcept => match on_page.next() {
            Some(_) => None,
            None => entry,
        },
    }
}

/// What a margin box shows on one page.
enum MarginContent {
    /// Text, named strings and page numbers.
    Inline(Vec<InlineItem>),
    /// A running element, laid out as a block.
    Element(BoxId),
}

impl MarginContent {
    fn of(style: &Arc<ComputedStyle>, marks: &Marks, entry: &Entry, tree: &BoxTree) -> Option<Self> {
        let Content::Items(items) = &style.content else {
            return None;
        };
        // A running element takes the whole box: `element()` stands alone in `content` (CSS
        // Generated Content for Paged Media 3), a value that combines it with others is invalid.
        if let Some((name, which)) = items.iter().find_map(|item| match item {
            ContentItem::Element { name, which } => Some((name, *which)),
            _ => None,
        }) {
            return Some(
                match pick(&marks.elements, entry.elements.get(name.as_str()), name, which) {
                    Some(&id) => Self::Element(id),
                    None => Self::Inline(Vec::new()),
                },
            );
        }
        let mut inline = Vec::new();
        let mut text = String::new();
        for item in items {
            match item {
                ContentItem::String(value) => text.push_str(value),
                ContentItem::NamedString { name, which } => {
                    if let Some(value) = pick(&marks.strings, entry.strings.get(name.as_str()), name, *which)
                    {
                        text.push_str(value);
                    }
                }
                ContentItem::Counter { name, style: format }
                    if let Some(counter) = PageCounter::from_name(name) =>
                {
                    if !text.is_empty() {
                        inline.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    inline.push(InlineItem::PageNumber {
                        counter,
                        format: format.clone(),
                        style: Arc::clone(style),
                    });
                }
                // An image is placed in its line like an `<img>`, as in Chrome.
                ContentItem::Image(url) => {
                    if let Some(&id) = tree.margin_images.get(url.trim()) {
                        if !text.is_empty() {
                            inline.push(InlineItem::Text {
                                text: std::mem::take(&mut text),
                                style: Arc::clone(style),
                            });
                        }
                        inline.push(InlineItem::Atomic(id));
                    }
                }
                ContentItem::OpenQuote => text.push_str(&crate::quotes::quote(
                    style.quotes.as_deref(),
                    style.lang.as_deref(),
                    0,
                    true,
                )),
                ContentItem::CloseQuote => text.push_str(&crate::quotes::quote(
                    style.quotes.as_deref(),
                    style.lang.as_deref(),
                    0,
                    false,
                )),
                _ => {}
            }
        }
        if !text.is_empty() {
            inline.push(InlineItem::Text {
                text,
                style: Arc::clone(style),
            });
        }
        Some(Self::Inline(process_white_space(inline)))
    }
}

/// Adds a replaced box to `tree` for each image that the `content` of a page margin box names
/// and that loaded, as for an image in the content of a pseudo-element: which `@page` rules apply
/// is known only page by page, so the images of all margin rules are added.
pub(crate) fn add_margin_images(tree: &mut BoxTree, rules: &RuleSet, images: &crate::image::Images) {
    let root = ComputedStyle::default();
    for rule in rules.page_rules() {
        for margin_rule in &rule.margin_rules {
            let style = style_from_declarations(&[&margin_rule.declarations], &root, A4);
            let Content::Items(items) = &style.content else {
                continue;
            };
            for item in items {
                let ContentItem::Image(url) = item else {
                    continue;
                };
                let url: Arc<str> = Arc::from(url.trim());
                if tree.margin_images.contains_key(&url) {
                    continue;
                }
                let Some(image) = images.get(&url) else {
                    continue;
                };
                let replaced_style = Arc::new(crate::tree::anonymous_style(&style));
                let image = crate::image::Image::zoomed(
                    &crate::image::Image::styled(image, &replaced_style),
                    replaced_style.zoom,
                );
                let id = tree.push(crate::tree::LayoutBox {
                    style: replaced_style,
                    node: None,
                    generated: true,
                    kind: crate::tree::BoxKind::Replaced {
                        image: Some(image),
                        attributes: (None, None),
                    },
                });
                tree.margin_images.insert(url, id);
            }
        }
    }
}

/// A generated margin box: its kind, style and content.
struct MarginBox<'a> {
    kind: PageMarginBox,
    style: &'a Arc<ComputedStyle>,
    content: MarginContent,
}

impl MarginBox<'_> {
    /// The vertical margins, borders and padding.
    fn frame_y(&self, basis: Au) -> Au {
        let style = self.style;
        let length = |value: &LengthPercentageAuto| Au::from_px(value.resolve_or(basis.to_px(), 0.0));
        let padding = |value: &LengthPercentage| Au::from_px(value.resolve(basis.to_px()));
        length(&style.margin.top)
            + length(&style.margin.bottom)
            + Au::from_px(style.border_width.top + style.border_width.bottom)
            + padding(&style.padding.top)
            + padding(&style.padding.bottom)
    }

    /// The horizontal margins, borders and padding.
    fn frame(&self, basis: Au) -> Au {
        let style = self.style;
        let length = |value: &LengthPercentageAuto| Au::from_px(value.resolve_or(basis.to_px(), 0.0));
        let padding = |value: &LengthPercentage| Au::from_px(value.resolve(basis.to_px()));
        length(&style.margin.left)
            + length(&style.margin.right)
            + Au::from_px(style.border_width.left + style.border_width.right)
            + padding(&style.padding.left)
            + padding(&style.padding.right)
    }

    /// The preferred length of the box along its edge of the page, in `edge` (the part of the
    /// margin its edge's boxes share, whose width and height percentages across and down refer
    /// to): along the top and bottom edges (`along`) its widths, along the sides its height as
    /// laid out in the margin's width (as in Blink).
    fn preferred(&self, layouter: &Layouter<'_>, edge: Rect, along: bool) -> Preferred {
        if along {
            let (min, max) = self.intrinsic(layouter, edge);
            return Preferred {
                min,
                max,
                auto: matches!(self.style.width, Size::Auto),
            };
        }
        let frame = self.frame_y(edge.height);
        let auto = !matches!(&self.style.height, Size::Length(_));
        let (min, max) = match (&self.style.height, &self.content) {
            (Size::Length(length), _) => {
                let length = Au::from_px(length.resolve(edge.height.to_px()));
                (length, length)
            }
            // Vertical lines run along the edge: the box's lengths are theirs, as wide ones as
            // horizontal lines along the top and bottom.
            (_, MarginContent::Inline(items)) if self.style.is_vertical() => {
                let outer = layouter.frame.replace(self.style.writing_mode);
                let (min, max) = intrinsic_widths(layouter, items);
                layouter.frame.set(outer);
                (Au::from_px_ceil(min), Au::from_px_ceil(max))
            }
            (_, content) => {
                let content_width = (edge.width - self.frame(edge.width)).clamp_non_negative();
                let length = match content {
                    MarginContent::Inline(items) => {
                        let text_style = margin_text(layouter, self.style, self.kind);
                        layout_lines(layouter, &text_style, items, content_width)
                            .iter()
                            .map(|line| line.height)
                            .sum()
                    }
                    MarginContent::Element(id) => layouter
                        .block_level(*id, Space::unfragmented(content_width), None)
                        .map_or(Au::ZERO, |outcome| {
                            outcome.collapsed_top.resolve()
                                + outcome.fragment.rect.height
                                + outcome.collapsed_bottom.resolve()
                        }),
                };
                (length, length)
            }
        };
        Preferred {
            min: min + frame,
            max: max + frame,
            auto,
        }
    }

    /// The outer min-content and max-content widths, in `edge` (see [`MarginBox::preferred`]).
    fn intrinsic(&self, layouter: &Layouter<'_>, edge: Rect) -> (Au, Au) {
        let frame = self.frame(edge.width);
        if let Size::Length(length) = &self.style.width {
            let width = Au::from_px(length.resolve(edge.width.to_px())) + frame;
            return (width, width);
        }
        // Vertical lines are as long as the box is high: the box is as wide as they stack.
        if let MarginContent::Inline(items) = &self.content
            && self.style.is_vertical()
        {
            let inline = (edge.height - self.frame_y(edge.height)).clamp_non_negative();
            let lines = vertical_margin_lines(layouter, self.style, self.kind, items, inline);
            let width = lines.iter().map(|line| line.height).sum::<Au>() + frame;
            return (width, width);
        }
        let (min, max) = match &self.content {
            MarginContent::Inline(items) => {
                let (min, max) = intrinsic_widths(layouter, items);
                (Au::from_px_ceil(min), Au::from_px_ceil(max))
            }
            MarginContent::Element(id) => layouter.intrinsic(*id),
        };
        (min + frame, max + frame)
    }
}

/// The vertical lines of the content of a margin box of `kind`, `inline` long, laid out in
/// its writing mode.
fn vertical_margin_lines(
    layouter: &Layouter<'_>,
    style: &Arc<ComputedStyle>,
    kind: PageMarginBox,
    items: &[InlineItem],
    inline: Au,
) -> Vec<crate::inline::Line> {
    let text_style = margin_text(layouter, style, kind);
    let outer = layouter.frame.replace(style.writing_mode);
    let lines = layout_lines(layouter, &text_style, items, inline);
    layouter.frame.set(outer);
    lines
}

/// The style of a margin box and its kind, with the style of its text made of them.
pub(crate) type MarginTextStyle = (Arc<ComputedStyle>, PageMarginBox, Arc<ComputedStyle>);

/// [`margin_text_style`] of `style`, made once for each style of a margin box: pages share
/// those (see [`PageBoxes`]), and the text of each page refers to the same style.
fn margin_text(
    layouter: &Layouter<'_>,
    style: &Arc<ComputedStyle>,
    kind: PageMarginBox,
) -> Arc<ComputedStyle> {
    let mut made = layouter.margin_text_styles.borrow_mut();
    if let Some((.., text)) = made
        .iter()
        .find(|(of, made_kind, _)| Arc::ptr_eq(of, style) && *made_kind == kind)
    {
        return Arc::clone(text);
    }
    let text = Arc::new(margin_text_style(style, kind));
    made.push((Arc::clone(style), kind, Arc::clone(&text)));
    text
}

/// The style of the text of a margin box of `kind`: aligned to its side of the page where
/// `text-align` does not say (CSS Paged Media 3, §5.3.1), in vertical lines too (as in Chrome).
fn margin_text_style(style: &ComputedStyle, kind: PageMarginBox) -> ComputedStyle {
    let mut text_style = style.clone();
    if text_style.text_align == TextAlign::Start {
        text_style.text_align = match kind {
            PageMarginBox::TopLeft | PageMarginBox::BottomLeft => TextAlign::Left,
            PageMarginBox::TopRight | PageMarginBox::BottomRight => TextAlign::Right,
            PageMarginBox::TopLeftCorner | PageMarginBox::BottomLeftCorner => TextAlign::Right,
            PageMarginBox::TopRightCorner | PageMarginBox::BottomRightCorner => TextAlign::Left,
            _ => TextAlign::Center,
        };
    }
    text_style
}

/// Lays out the margin boxes of one page.
fn margin_boxes(layouter: &Layouter<'_>, page: &PageBox, marks: &Marks, entry: &Entry) -> Vec<Fragment> {
    let boxes: Vec<MarginBox<'_>> = page
        .margin_boxes
        .iter()
        .filter_map(|(kind, style)| {
            Some(MarginBox {
                kind: *kind,
                style,
                content: MarginContent::of(style, marks, entry, layouter.tree)?,
            })
        })
        .collect();
    // The margins as Blink places the boxes in them: the page border box goes at the top and
    // left margins rounded to whole pixels and is as large as the rest rounded up, and no
    // margin is larger than the room that leaves (`margins.Intersect`), so that a top margin of
    // 20.3 pixels is 20 and one of 75.59 stays as it is.
    let style = &page.style;
    let frame =
        |border: f32, padding: &LengthPercentage| Au::from_px(border + padding.resolve(page.width.to_px()));
    let area = page.area;
    let snapped = |margin: Au| margin.min(Au::from_px(margin.to_px().round()));
    let top = snapped(page.margin[0]);
    let bottom =
        (page.height - (area.y + area.height) - frame(style.border_width.bottom, &style.padding.bottom))
            .max(Au::ZERO)
            .min(page.margin[2]);
    // Right to left, the boxes are placed from the right: the right margin is snapped, and the
    // left one is what the content leaves.
    let (left, right) = if page.rtl {
        (
            (page.content_x - frame(style.border_width.left, &style.padding.left))
                .max(Au::ZERO)
                .min(page.margin[3]),
            snapped(page.margin[1]),
        )
    } else {
        (
            snapped(page.margin[3]),
            (page.width - (area.x + area.width) - frame(style.border_width.right, &style.padding.right))
                .max(Au::ZERO)
                .min(page.margin[1]),
        )
    };
    let width = page.width - left - right;
    let height = page.height - top - bottom;
    let find = |kind: PageMarginBox| boxes.iter().find(|margin_box| margin_box.kind == kind);
    let mut fragments = Vec::new();
    // The three boxes along each edge share its length (CSS Paged Media 3, §5.3.2), the ones
    // along the left and right edges by their heights.
    for (row, rect) in [
        (
            [
                PageMarginBox::TopLeft,
                PageMarginBox::TopCenter,
                PageMarginBox::TopRight,
            ],
            Rect::new(left, Au::ZERO, width, top),
        ),
        (
            [
                PageMarginBox::BottomLeft,
                PageMarginBox::BottomCenter,
                PageMarginBox::BottomRight,
            ],
            Rect::new(left, page.height - bottom, width, bottom),
        ),
        (
            [
                PageMarginBox::LeftTop,
                PageMarginBox::LeftMiddle,
                PageMarginBox::LeftBottom,
            ],
            Rect::new(Au::ZERO, top, left, height),
        ),
        (
            [
                PageMarginBox::RightTop,
                PageMarginBox::RightMiddle,
                PageMarginBox::RightBottom,
            ],
            Rect::new(page.width - right, top, right, height),
        ),
    ] {
        let along = matches!(row[0], PageMarginBox::TopLeft | PageMarginBox::BottomLeft);
        let row = row.map(find);
        let sizes = edge_sizes(
            if along { rect.width } else { rect.height },
            row.map(|margin_box| {
                margin_box.map(|margin_box| {
                    in_margin_box(layouter, margin_box.style, || {
                        margin_box.preferred(layouter, rect, along)
                    })
                })
            }),
        );
        for (index, (margin_box, length)) in row.into_iter().zip(sizes).enumerate() {
            let Some(margin_box) = margin_box else {
                continue;
            };
            let slot = Slot { length, index, along };
            fragments.push((
                margin_box.kind,
                in_margin_box(layouter, margin_box.style, || {
                    lay_out_margin_box(layouter, margin_box, rect, Some(slot))
                }),
            ));
        }
    }
    for margin_box in &boxes {
        let rect = match margin_box.kind {
            PageMarginBox::TopLeftCorner => Rect::new(Au::ZERO, Au::ZERO, left, top),
            PageMarginBox::TopRightCorner => Rect::new(page.width - right, Au::ZERO, right, top),
            PageMarginBox::BottomLeftCorner => Rect::new(Au::ZERO, page.height - bottom, left, bottom),
            PageMarginBox::BottomRightCorner => {
                Rect::new(page.width - right, page.height - bottom, right, bottom)
            }
            _ => continue,
        };
        fragments.push((
            margin_box.kind,
            in_margin_box(layouter, margin_box.style, || {
                lay_out_margin_box(layouter, margin_box, rect, None)
            }),
        ));
    }
    // They paint clockwise from the top left corner (CSS Paged Media 3, §5.4), as in Chrome.
    fragments.sort_by_key(|(kind, _)| paint_order(*kind));
    fragments.into_iter().map(|(_, fragment)| fragment).collect()
}

/// How far the content of a margin box goes from the start of the box along its block axis
/// (down the box, or across it from the side where vertical lines start) where it leaves
/// `room` there, by the box's `vertical-align`, as in Chrome: `middle` centers it, `bottom` puts
/// it at the end, anything else at the start, which is where content that overflows the box
/// stays (the user agent's rules say which a box has by default).
fn aligned(style: &ComputedStyle, room: Au) -> Au {
    let room = room.clamp_non_negative();
    match style.vertical_align {
        VerticalAlign::Middle => room / 2,
        VerticalAlign::Bottom | VerticalAlign::TextBottom => room,
        _ => Au::ZERO,
    }
}

/// Where a margin box comes in the order the margin boxes paint in: clockwise from the top left
/// corner, the boxes of each edge in its direction.
fn paint_order(kind: PageMarginBox) -> u8 {
    match kind {
        PageMarginBox::TopLeftCorner => 0,
        PageMarginBox::TopLeft => 1,
        PageMarginBox::TopCenter => 2,
        PageMarginBox::TopRight => 3,
        PageMarginBox::TopRightCorner => 4,
        PageMarginBox::RightTop => 5,
        PageMarginBox::RightMiddle => 6,
        PageMarginBox::RightBottom => 7,
        PageMarginBox::BottomRightCorner => 8,
        PageMarginBox::BottomRight => 9,
        PageMarginBox::BottomCenter => 10,
        PageMarginBox::BottomLeft => 11,
        PageMarginBox::BottomLeftCorner => 12,
        PageMarginBox::LeftBottom => 13,
        PageMarginBox::LeftMiddle => 14,
        PageMarginBox::LeftTop => 15,
    }
}

/// The preferred length of a margin box along its edge of the page: its outer min-content and
/// max-content lengths, and whether its size along the edge is `auto` (Blink's
/// `PreferredSizeInfo`). A box that is not generated has none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Preferred {
    min: Au,
    max: Au,
    auto: bool,
}

impl Preferred {
    fn doubled(self) -> Self {
        Self {
            min: self.min * 2,
            max: self.max * 2,
            ..self
        }
    }
}

/// The lengths of the three margin boxes along an edge `available` long, as Blink resolves them
/// (`CalculateEdgeMarginBoxSizes`, CSS Paged Media 3, §5.3.2): auto-sized boxes share what the
/// others leave by their content, and an auto-sized center box stays centered, the boxes beside
/// it taking half of the rest each.
fn edge_sizes(available: Au, boxes: [Option<Preferred>; 3]) -> [Au; 3] {
    let mut preferred = boxes.map(Option::unwrap_or_default);
    let mut sizes = preferred.map(|size| size.max);
    // Auto-sized boxes without content get equal shares.
    if boxes.iter().flatten().any(|size| size.auto)
        && boxes
            .iter()
            .flatten()
            .filter(|size| size.auto)
            .all(|size| size.max == Au::ZERO)
    {
        for size in &mut preferred {
            if size.auto {
                *size = Preferred {
                    min: Au::from_px(1.0),
                    max: Au::from_px(1.0),
                    auto: true,
                };
            }
        }
    }
    let [start, center, end] = preferred;
    if boxes[1].is_some() {
        if center.auto {
            let for_start = resolve_two([center, Preferred::default(), start.doubled()], available).0;
            let for_end = resolve_two([center, Preferred::default(), end.doubled()], available).0;
            sizes[1] = for_start.min(for_end);
        }
        let side = available - sizes[1];
        if start.auto {
            sizes[0] = side / 2;
        }
        if end.auto {
            sizes[2] = side - side / 2;
        }
    } else {
        (sizes[0], sizes[2]) = resolve_two(preferred, available);
    }
    sizes.map(Au::clamp_non_negative)
}

/// The lengths of the first and the last of `boxes` in `available` (Blink's
/// `ResolveTwoEdgeMarginBoxLengths`; the middle one has a fixed length): space beyond the
/// max-content lengths of the auto-sized ones goes in proportion to them; less space shrinks
/// them toward their min-content lengths, and below those in proportion to them.
fn resolve_two(boxes: [Preferred; 3], available: Au) -> (Au, Au) {
    let mut flexible = available;
    let (mut auto_min, mut auto_max) = (Au::ZERO, Au::ZERO);
    for size in boxes {
        if size.auto {
            auto_min += size.min;
            auto_max += size.max;
        } else {
            flexible -= size.max;
        }
    }
    let (space, unflexed, factors) = if flexible > auto_max {
        (
            flexible - auto_max,
            boxes.map(|size| size.max),
            boxes.map(|size| size.max),
        )
    } else {
        let space = flexible - auto_min;
        let factors = if space > Au::ZERO {
            boxes.map(|size| size.max - size.min)
        } else {
            boxes.map(|size| size.min)
        };
        (space, boxes.map(|size| size.min), factors)
    };
    let [first, _, second] = boxes;
    let (mut first_size, mut second_size) = (unflexed[0], unflexed[2]);
    if first.auto {
        if second.auto {
            let total = factors[0] + factors[2];
            if total > Au::ZERO {
                // Layout unit arithmetic: the product in units, then the quotient.
                let product = i64::from(space.0) * i64::from(factors[0].0) / 64;
                first_size += Au(i32::try_from(product * 64 / i64::from(total.0)).unwrap_or(0));
            }
        } else {
            first_size = available - second_size;
        }
    }
    if second.auto {
        second_size = available - first_size;
    }
    (first_size, second_size)
}

/// Where a margin box along an edge of the page goes: its outer length along the edge, its place
/// there (0 at the start, 1 in the center, 2 at the end), and whether the edge runs across the
/// page (the top and bottom edges).
#[derive(Clone, Copy)]
struct Slot {
    length: Au,
    index: usize,
    along: bool,
}

/// The margins at the start and end of a margin box along one axis, where that axis meets an
/// edge of the page, `available` long: `auto` margins take the room the box leaves (none where
/// there is none), and a box that does not fill the room is pushed away from the page's edge by
/// the margin there (Blink's `ResolveMarginsForPageMarginBox`).
fn page_edge_margins(
    lengths: (&LengthPercentageAuto, &LengthPercentageAuto),
    (start, end): (Au, Au),
    size: Au,
    available: Au,
    at_start: bool,
) -> (Au, Au) {
    let room = (available - size - start - end).clamp_non_negative();
    let (mut start, mut end) = match (lengths.0.is_auto(), lengths.1.is_auto()) {
        (true, true) => (start + room / 2, end + room - room / 2),
        (true, false) => (start + room, end),
        (false, true) => (start, end + room),
        (false, false) => (start, end),
    };
    let inequality = available - (size + start + end);
    if at_start {
        start += inequality;
    } else {
        end += inequality;
    }
    (start, end)
}

/// Lays out a margin box in `edge`, the part of the page margin of its edge (or its corner), as
/// Blink does (`PageContainerLayoutAlgorithm`): percentages of the edge's width across and its
/// height down; along the edge (`slot`, for boxes that are not corners), the box is as long as
/// its share of the edge, placed at its start, center or end; across it, and both ways in a
/// corner, it is as large as its `width` and `height` say, else it fills the room, and its
/// margins place it (see [`page_edge_margins`]).
fn lay_out_margin_box(
    layouter: &Layouter<'_>,
    margin_box: &MarginBox<'_>,
    edge: Rect,
    slot: Option<Slot>,
) -> Fragment {
    let style = margin_box.style;
    let px = |value: f32| Au::from_px(value);
    let (basis_x, basis_y) = (edge.width.to_px(), edge.height.to_px());
    let mut margin = [
        px(style.margin.top.resolve_or(basis_y, 0.0)),
        px(style.margin.right.resolve_or(basis_x, 0.0)),
        px(style.margin.bottom.resolve_or(basis_y, 0.0)),
        px(style.margin.left.resolve_or(basis_x, 0.0)),
    ];
    let inset = [
        px(style.border_width.top) + px(style.padding.top.resolve(basis_y)),
        px(style.border_width.right) + px(style.padding.right.resolve(basis_x)),
        px(style.border_width.bottom) + px(style.padding.bottom.resolve(basis_y)),
        px(style.border_width.left) + px(style.padding.left.resolve(basis_x)),
    ];
    let content_box = style.box_sizing == fagus_style::values::BoxSizing::ContentBox;
    let (frame_x, frame_y) = (inset[1] + inset[3], inset[0] + inset[2]);
    let given = |size: &Size, basis: f32, frame: Au| match size {
        Size::Length(length) => Some(px(length.resolve(basis)) + if content_box { frame } else { Au::ZERO }),
        _ => None,
    };
    // Sizes other than lengths and `auto` (`fit-content` and the like) fit the content.
    let fits = |size: &Size| !matches!(size, Size::Auto | Size::Stretch | Size::Length(_));
    let width = match slot {
        Some(Slot {
            length, along: true, ..
        }) => length - margin[1] - margin[3],
        _ => given(&style.width, basis_x, frame_x).unwrap_or_else(|| {
            let room = edge.width - margin[1] - margin[3];
            if fits(&style.width) {
                let (min, max) = margin_box.intrinsic(layouter, edge);
                (max.min(edge.width).max(min) - margin[1] - margin[3]).min(room)
            } else {
                room
            }
        }),
    }
    .clamp_non_negative()
    .max(frame_x);
    let height = match slot {
        Some(Slot {
            length, along: false, ..
        }) => Some(length - margin[0] - margin[2]),
        _ => given(&style.height, basis_y, frame_y).or_else(|| {
            // Vertical lines are as long as a height that fits the content says, of the room
            // across the edge (or in the corner).
            let MarginContent::Inline(items) = &margin_box.content else {
                return None;
            };
            if !style.is_vertical() || !fits(&style.height) {
                return None;
            }
            let room = (edge.height - margin[0] - margin[2] - frame_y).clamp_non_negative();
            let outer = layouter.frame.replace(style.writing_mode);
            let (min, max) = intrinsic_widths(layouter, items);
            layouter.frame.set(outer);
            let (min, max) = (Au::from_px_ceil(min), Au::from_px_ceil(max));
            let inline = match style.height {
                Size::MinContent => min,
                Size::MaxContent => max,
                _ => max.min(room).max(min),
            };
            Some(inline + frame_y)
        }),
    }
    .unwrap_or(edge.height - margin[0] - margin[2])
    .clamp_non_negative()
    .max(frame_y);
    let content_width = (width - frame_x).clamp_non_negative();
    let mut content_height = (height - frame_y).clamp_non_negative();

    let mut children = Vec::new();
    let used_height = match &margin_box.content {
        // Vertical lines, as long as the box is high, stack from its middle across it.
        MarginContent::Inline(items) if style.is_vertical() => {
            let mode = style.writing_mode;
            let lines = vertical_margin_lines(layouter, style, margin_box.kind, items, content_height);
            let extent = lines.iter().map(|line| line.height).sum::<Au>();
            let mut y = aligned(style, content_width - extent);
            for line in lines {
                let mut fragment = line.fragment;
                fragment.rect.y = y;
                y += line.height;
                crate::writing::to_physical(
                    &mut fragment,
                    mode,
                    (content_height, content_width),
                    &layouter.tree.flows,
                );
                fragment.rect.x += inset[3];
                fragment.rect.y += inset[0];
                children.push(fragment);
            }
            content_height
        }
        MarginContent::Inline(items) => {
            let text_style = margin_text(layouter, style, margin_box.kind);
            let lines = layout_lines(layouter, &text_style, items, content_width);
            let mut y = Au::ZERO;
            for line in lines {
                let mut fragment = line.fragment;
                fragment.rect.x = inset[3];
                fragment.rect.y = inset[0] + y;
                y += line.height;
                children.push(fragment);
            }
            y
        }
        // A running element of a vertical document is laid out in its lines, which run down the
        // box, and centered across it.
        MarginContent::Element(id) if layouter.writing_mode().is_vertical() => {
            let mode = layouter.writing_mode();
            if let Some(outcome) = layouter.block_level(*id, Space::unfragmented(content_height), None) {
                let mut fragment = outcome.fragment;
                let extent = outcome.collapsed_top.resolve()
                    + fragment.rect.height
                    + outcome.collapsed_bottom.resolve();
                fragment.rect.y += outcome.collapsed_top.resolve() + aligned(style, content_width - extent);
                let element_area = Rect::new(Au::ZERO, Au::ZERO, content_height, content_width);
                layouter.prepare_page(
                    &mut fragment,
                    element_area,
                    (0, None),
                    &mut crate::positioned::FixedStatics::default(),
                );
                layouter.position(&mut fragment, element_area, None, None);
                crate::writing::to_physical(
                    &mut fragment,
                    mode,
                    (content_height, content_width),
                    &layouter.tree.flows,
                );
                fragment.rect.x += inset[3];
                fragment.rect.y += inset[0];
                children.push(fragment);
            }
            content_height
        }
        MarginContent::Element(id) => {
            match layouter.block_level(*id, Space::unfragmented(content_width), None) {
                Some(outcome) => {
                    let margin_top = outcome.collapsed_top.resolve();
                    let mut fragment = outcome.fragment;
                    let used = margin_top + fragment.rect.height + outcome.collapsed_bottom.resolve();
                    let element_area = Rect::new(Au::ZERO, Au::ZERO, content_width, used);
                    layouter.prepare_page(
                        &mut fragment,
                        element_area,
                        (0, None),
                        &mut crate::positioned::FixedStatics::default(),
                    );
                    layouter.position(&mut fragment, element_area, None, None);
                    fragment.rect.x += inset[3];
                    fragment.rect.y = inset[0] + margin_top;
                    children.push(fragment);
                    used
                }
                None => Au::ZERO,
            }
        }
    };
    // A height that fits the content is as tall as it.
    let height = if slot.is_none_or(|slot| slot.along) && fits(&style.height) {
        content_height = used_height;
        used_height + frame_y
    } else {
        height
    };
    // The margins across the edge (both ways in a corner) place the box there; along the edge,
    // its share of it does.
    let (across_x, across_y, top, left) = match (slot, margin_box.kind) {
        (Some(slot), kind) => (
            !slot.along,
            slot.along,
            matches!(
                kind,
                PageMarginBox::TopLeft | PageMarginBox::TopCenter | PageMarginBox::TopRight
            ),
            matches!(
                kind,
                PageMarginBox::LeftTop | PageMarginBox::LeftMiddle | PageMarginBox::LeftBottom
            ),
        ),
        (None, kind) => (
            true,
            true,
            matches!(kind, PageMarginBox::TopLeftCorner | PageMarginBox::TopRightCorner),
            matches!(
                kind,
                PageMarginBox::TopLeftCorner | PageMarginBox::BottomLeftCorner
            ),
        ),
    };
    if across_y {
        (margin[0], margin[2]) = page_edge_margins(
            (&style.margin.top, &style.margin.bottom),
            (margin[0], margin[2]),
            height,
            edge.height,
            top,
        );
    }
    if across_x {
        (margin[3], margin[1]) = page_edge_margins(
            (&style.margin.left, &style.margin.right),
            (margin[3], margin[1]),
            width,
            edge.width,
            left,
        );
    }
    let along_offset = |available: Au, outer: Au| match slot.map(|slot| slot.index) {
        Some(1) => (available - outer) / 2,
        Some(2) => available - outer,
        _ => Au::ZERO,
    };
    let x = if across_x {
        Au::ZERO
    } else {
        along_offset(edge.width, width + margin[1] + margin[3])
    };
    let y = if across_y {
        Au::ZERO
    } else {
        along_offset(edge.height, height + margin[0] + margin[2])
    };
    let border_box = Rect::new(edge.x + x + margin[3], edge.y + y + margin[0], width, height);
    // Lines across the page are aligned down the box (vertical ones across it, see above).
    let offset = aligned(style, content_height - used_height);
    for child in &mut children {
        child.rect.y += offset;
    }
    Fragment {
        rect: border_box,
        kind: FragmentKind::Box {
            style: Arc::clone(style),
            node: None,
            sides: VisibleSides::ALL,
        },
        children: children.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(value: f32) -> Au {
        Au::from_px(value)
    }

    #[test]
    fn counts_the_parts_reaching_onto_each_page_against_the_budget() {
        // Two parts reach onto pages 2 and 3, one onto 1 and 4 to 5.
        let runs = [(1, 4), (2, 6)];
        let mut budget = 100;
        assert_eq!(reached_pages(&runs, &mut budget), [1, 2, 3, 4, 5]);
        assert_eq!(budget, 100 - 7);
        // The pages go as far as the budget does.
        let mut budget = 4;
        assert_eq!(reached_pages(&runs, &mut budget), [1, 2]);
        assert_eq!(budget, 0);
        let mut budget = 0;
        assert_eq!(reached_pages(&runs, &mut budget), Vec::<usize>::new());
    }

    /// The page box of the first page with the user agent's rules and `css`.
    fn first_page(css: &str) -> PageBox {
        use fagus_css::{MediaEnvironment, Origin, Stylesheet};
        let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(css, Origin::Author, "style");
        let rules = RuleSet::new(&[&ua, &author], &MediaEnvironment::default());
        page_box(&rules, &ComputedStyle::default(), 0, None, &[])
    }

    /// The size of the first page with `@page { size: <size> }` after `100mm 150mm`, in
    /// millimetres to a hundredth.
    fn size_of(size: &str) -> (f32, f32) {
        let page = first_page(&format!("@page {{ size: 100mm 150mm }} @page {{ size: {size} }}"));
        let mm = |au: Au| (au.to_px() / MM * 100.0).round() / 100.0;
        (mm(page.width), mm(page.height))
    }

    #[test]
    fn zooms_the_page_margins_by_the_zoom_of_the_root() {
        use fagus_css::{MediaEnvironment, Origin, Stylesheet};
        let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "@page { size: 400px 300px; margin: 20px } @page :first { @top-left { content: 'x'; margin-left: 2px } }",
            Origin::Author,
            "style",
        );
        let rules = RuleSet::new(&[&ua, &author], &MediaEnvironment::default());
        let root = ComputedStyle::from(fagus_style::ComputedValues {
            zoom: 1.5,
            font_size: 24.0,
            ..fagus_style::ComputedValues::default()
        });
        // The page context inherits the root's zoom, as in Chrome: its margins (and its margin
        // boxes) are zoomed, its size is not.
        let page = page_box(&rules, &root, 0, None, &[]);
        assert_eq!((page.width, page.height), (px(400.0), px(300.0)));
        assert_eq!(page.margin, [px(30.0); 4]);
        assert_eq!(page.viewport, (px(340.0), px(240.0)));
        let (_, margin_box) = page.margin_boxes.first().expect("a margin box");
        assert_eq!(
            margin_box.margin.left,
            LengthPercentageAuto::Length(LengthPercentage::Px(3.0))
        );
    }

    #[test]
    fn parses_page_sizes_as_chrome() {
        assert_eq!(size_of("80mm 200mm"), (80.0, 200.0));
        assert_eq!(size_of("80mm"), (80.0, 80.0));
        assert_eq!(size_of("4in 6in"), (101.6, 152.4));
        assert_eq!(size_of("400Q 600Q"), (100.0, 150.0));
        // Font-relative lengths are relative to 16 pixels.
        assert_eq!(size_of("20em 30rem"), (84.67, 127.0));
        assert_eq!(size_of("landscape A5"), (210.0, 148.0));
        assert_eq!(size_of("JIS-B5 portrait"), (182.0, 257.0));
        assert_eq!(size_of("letter landscape"), (279.4, 215.9));
        assert_eq!(size_of("landscape"), (297.0, 210.0));
        assert_eq!(size_of("auto"), (210.0, 297.0));
        // Invalid values leave the size as it was: `auto` with a length, sizes Chrome does not
        // know (A6), lengths with an orientation, three lengths, repeated keywords and negative
        // lengths.
        for invalid in [
            "80mm auto",
            "A6",
            "A6 landscape",
            "80mm 200mm landscape",
            "1in 2in 3in",
            "A4 A4",
            "landscape landscape",
            "auto landscape",
            "-5mm 10mm",
            "80mm 10%",
        ] {
            assert_eq!(size_of(invalid), (100.0, 150.0), "{invalid}");
        }
    }

    #[test]
    fn evaluates_math_functions_and_viewport_units_in_page_sizes() {
        assert_eq!(size_of("calc(50mm + 30mm) calc(2 * 100mm)"), (80.0, 200.0));
        assert_eq!(size_of("min(80mm, 9cm) max(1in, 120mm)"), (80.0, 120.0));
        assert_eq!(size_of("clamp(10mm, 300mm, 80mm) 200mm"), (80.0, 200.0));
        assert_eq!(size_of("calc(5em) 150mm"), (21.17, 150.0));
        // Viewport units are relative to the default page.
        assert_eq!(size_of("50vw 10vh"), (105.0, 29.7));
        assert_eq!(size_of("10vmin 10vmax"), (21.0, 29.7));
        // Percentages are invalid, as is a function with an orientation.
        assert_eq!(size_of("calc(50% + 10mm) 100mm"), (100.0, 150.0));
        assert_eq!(size_of("calc(80mm) landscape"), (100.0, 150.0));
    }

    #[test]
    fn resolves_page_margins_against_the_page_and_the_default_page() {
        let margins = |css: &str| {
            first_page(&format!("@page {{ size: 400px 300px; {css} }}"))
                .margin
                .map(Au::to_px)
        };
        // Percentages across against the page's width, down against its height, as in Chrome.
        assert_eq!(margins("margin: 10%"), [30.0, 40.0, 30.0, 40.0]);
        assert_eq!(margins("margin: calc(5% + 10px)"), [25.0, 30.0, 25.0, 30.0]);
        // Viewport units against the default page (A4, 793.7 by 1122.5 pixels): the page area
        // lies at the margins rounded to whole pixels.
        let page = first_page("@page { size: 400px 300px; margin: 10vh 5vw }");
        assert_eq!((page.area.x.to_px(), page.area.y.to_px()), (40.0, 112.0));
    }

    #[test]
    fn prints_pages_without_room_for_content_on_the_default_page() {
        // Less than a point left across or down, with the page rounded to whole points: the
        // default page and margins instead, as Chrome prints it.
        let default = first_page("");
        for css in [
            "@page { size: 0 0; margin: 0 }",
            "@page { size: 0.6px 0.6px; margin: 0 }",
            "@page { size: 10mm; margin: 4.9mm }",
            "@page { size: 50mm; margin: 0 0 49.8mm }",
        ] {
            let page = first_page(css);
            assert_eq!(
                (page.width, page.height, page.margin),
                (default.width, default.height, default.margin),
                "{css}"
            );
        }
        // A pixel is three quarters of a point, a whole one rounded.
        let page = first_page("@page { size: 1px; margin: 0 }");
        assert_eq!((page.width, page.margin[0]), (Au::from_px(1.0), Au::ZERO));
        let page = first_page("@page { size: 10mm; margin: 4mm }");
        assert_eq!(page.width, Au::from_px(page_size_rounded(10.0 * MM)));
    }

    /// A length rounded to layout units.
    fn page_size_rounded(px: f32) -> f32 {
        (px * 64.0).round() / 64.0
    }

    #[test]
    fn scales_the_page_and_its_margins_before_cutting_them_to_layout_units() {
        // A4 with margins of 18mm, laid out 1.0036 times as large for content wider than the
        // page: 796.55 and 68.28 pixels, cut to layout units, leave 660.016 pixels (where
        // scaling the page area leaves 659.999), as Chrome lays out the page.
        let page = first_page("@page { size: A4; margin: 18mm }");
        assert_eq!(page.exact_scaled(1.0).0, page.exact.0);
        let (width, _) = page.exact_scaled(1.003_585_6);
        assert!((width - (660.0 + 1.0 / 64.0)).abs() < 1e-3, "{width}");
    }

    #[test]
    fn rounds_the_page_size_to_layout_units() {
        // JIS-B5 is 971.338 pixels tall: 971.34375 in layout units (728.51 points, which Chrome
        // prints as 729), not cut to 971.328125 (728.496 points).
        let page = first_page("@page { size: JIS-B5 }");
        assert_eq!(page.height.to_px(), 971.343_75);
    }

    #[test]
    fn shares_the_edge_by_content() {
        let auto = |min: f32, max: f32| {
            Some(Preferred {
                min: px(min),
                max: px(max),
                auto: true,
            })
        };
        // With room for both, the rest goes in proportion to the max-content widths.
        assert_eq!(
            edge_sizes(px(400.0), [auto(50.0, 100.0), None, auto(50.0, 100.0)]),
            [px(200.0), Au::ZERO, px(200.0)]
        );
        assert_eq!(
            edge_sizes(px(400.0), [auto(10.0, 300.0), None, auto(10.0, 100.0)]),
            [px(300.0), Au::ZERO, px(100.0)]
        );
        // A box on its own takes all of it, a center box too.
        assert_eq!(
            edge_sizes(px(400.0), [auto(10.0, 100.0), None, None]),
            [px(400.0), Au::ZERO, Au::ZERO]
        );
        assert_eq!(
            edge_sizes(px(240.0), [None, auto(12.0, 12.0), None])[1],
            px(240.0)
        );
        // With too little room, the boxes shrink toward their min-content widths.
        assert_eq!(
            edge_sizes(px(300.0), [auto(100.0, 300.0), None, auto(100.0, 100.0)]),
            [px(200.0), Au::ZERO, px(100.0)]
        );
        // A box of fixed size keeps it; a center box is as wide as it can be with twice the side
        // box beside it.
        let fixed = Some(Preferred {
            min: px(50.0),
            max: px(50.0),
            auto: false,
        });
        assert_eq!(
            edge_sizes(px(400.0), [fixed, None, auto(10.0, 100.0)]),
            [px(50.0), Au::ZERO, px(350.0)]
        );
        assert_eq!(
            edge_sizes(px(400.0), [auto(20.0, 60.0), auto(50.0, 100.0), None]),
            [px(109.093_75), px(181.8125), Au::ZERO]
        );
    }

    /// The pages of `html`, whose `<style>` elements hold its rules, with the test fonts.
    fn render(html: &str) -> Vec<Page> {
        use fagus_css::{MediaEnvironment, Origin, Stylesheet};
        let document = fagus_dom::Document::parse_html(html);
        let css: String = document
            .descendants(fagus_dom::NodeId::DOCUMENT)
            .filter(|&node| document.element(node).is_some_and(|data| data.is("style")))
            .map(|node| document.text_content(node))
            .collect();
        let ua = Stylesheet::parse(fagus_style::USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(&css, Origin::Author, "style");
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = fagus_style::compute_styles(&document, &rules, (environment.width, environment.height));
        let mut fonts = FontDb::new();
        fonts
            .add_font_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fonts"))
            .expect("test fonts");
        crate::layout(&document, &styled, &rules, &fonts, &crate::Images::default())
    }

    #[test]
    fn sizes_the_page_by_its_width_height_and_auto_margins() {
        // Over-constrained, the page takes the size of the page box and its margins, not `size`.
        let page = first_page("@page { size: 500px; margin: 50px; width: 200px; height: 300px }");
        assert_eq!((page.width, page.height), (px(300.0), px(400.0)));
        assert_eq!(page.margin, [px(50.0); 4]);
        assert_eq!((page.area.width, page.area.height), (px(200.0), px(300.0)));
        // Auto margins take the room the page box leaves, negative where there is none.
        let page = first_page("@page { size: 320px 112px; width: 192px; height: 48px; margin: auto }");
        assert_eq!((page.width, page.height), (px(320.0), px(112.0)));
        assert_eq!(page.margin, [px(32.0), px(64.0), px(32.0), px(64.0)]);
        let page = first_page("@page { size: 300px; width: 340px; height: 340px; margin: auto }");
        assert_eq!(page.margin, [px(-20.0); 4]);
        // Percentages of the padding refer to the page's height down and its width across.
        let page = first_page("@page { size: 400px 800px; margin: 0; padding: 5% 20% 15% 40% }");
        assert_eq!(page.insets.map(f32::round), [40.0, 80.0, 120.0, 160.0]);
    }

    /// The margin boxes of the first page of `html` and the rectangles of their descendants
    /// (in the box) that have no children of their own.
    fn margin_box_leaves(html: &str) -> Vec<(Rect, Vec<Rect>)> {
        fn leaves(fragment: &Fragment, (x, y): (Au, Au), out: &mut Vec<Rect>) {
            for child in &fragment.children {
                let at = (x + child.rect.x, y + child.rect.y);
                if child.children.is_empty() {
                    out.push(Rect::new(at.0, at.1, child.rect.width, child.rect.height));
                } else {
                    leaves(child, at, out);
                }
            }
        }
        render(html)[0]
            .fragments
            .iter()
            .skip(1)
            .map(|fragment| {
                let mut out = Vec::new();
                leaves(fragment, (Au::ZERO, Au::ZERO), &mut out);
                (fragment.rect, out)
            })
            .collect()
    }

    #[test]
    fn aligns_margin_box_content_along_its_block_axis_as_chrome() {
        let boxes = margin_box_leaves(
            "<style>@page { size: 500px 400px; margin: 100px;
               @top-left-corner { content: 'x'; writing-mode: vertical-rl; vertical-align: bottom }
               @top-right-corner { content: 'x'; writing-mode: vertical-lr; vertical-align: top }
               @bottom-center { content: 'a\\A b\\A c\\A d\\A e\\A f\\A g'; white-space: pre;
                 vertical-align: bottom }
               @left-middle { content: 'x' } }</style>x",
        );
        let [
            (corner, left),
            (right_corner, right),
            (bottom, lines),
            (side, middle),
        ] = &boxes[..]
        else {
            panic!("four boxes: {boxes:?}");
        };
        // Vertical lines: the end of the block axis of `vertical-rl` is the left, the start of
        // `vertical-lr` too.
        assert_eq!(corner.width, px(100.0));
        assert!(left.iter().all(|leaf| leaf.x < px(30.0)), "{left:?}");
        assert_eq!(right_corner.width, px(100.0));
        assert!(right.iter().all(|leaf| leaf.x < px(30.0)), "{right:?}");
        // Lines taller than the box start at its top, whatever the alignment.
        assert!(lines.iter().map(|leaf| leaf.y).min() < Some(px(5.0)), "{lines:?}");
        assert!(lines.iter().map(|leaf| leaf.y + leaf.height).max() > Some(bottom.height));
        // By default the boxes along the sides are centered on their middle.
        let center = middle[0].y + middle[0].height / 2;
        assert!(
            (center - side.height / 2).0.abs() < px(2.0).0,
            "{middle:?} in {side:?}"
        );
    }

    #[test]
    fn shares_the_sides_between_vertical_margin_boxes_by_their_content() {
        let boxes = margin_box_leaves(&format!(
            "<style>@page {{ size: 500px 400px; margin: 100px;
               @right-top {{ content: '{}'; writing-mode: vertical-rl }}
               @right-bottom {{ content: 'mmmm'; writing-mode: vertical-rl }}
               @bottom-left {{ content: 'mm'; writing-mode: vertical-rl; height: min-content;
                 margin-top: auto }} }}</style>x",
            "m ".repeat(40)
        ));
        let [(top, _), (bottom, word), (bottom_left, short)] = &boxes[..] else {
            panic!("three boxes: {boxes:?}");
        };
        // The side's 200 pixels are less than the content's max-content lengths but more than
        // its min-content ones: the box whose lengths are the same keeps them.
        assert_eq!(top.height + bottom.height, px(200.0));
        let length = word.iter().map(|leaf| leaf.height).max().unwrap_or_default();
        assert!(
            (bottom.height - length).0.abs() < px(1.0).0,
            "{bottom:?}, {word:?}"
        );
        // A min-content height along the bottom is the length of the lines, at the bottom of
        // the margin.
        let length = short.iter().map(|leaf| leaf.height).max().unwrap_or_default();
        assert!(
            (bottom_left.height - length).0.abs() < px(1.0).0,
            "{bottom_left:?}, {short:?}"
        );
        assert_eq!(bottom_left.y + bottom_left.height, px(400.0));
    }

    #[test]
    fn adds_pages_for_what_overflows_boxes_contained_in_size() {
        let pages = |inner: &str| {
            render(&format!(
                "<style>@page {{ size: 300px 200px; margin: 0 }} body {{ margin: 0 }}</style>
                 <div style='contain: size; height: 420px'>{inner}</div>"
            ))
            .len()
        };
        assert_eq!(pages(""), 3);
        // The boxes inside reach further, onto pages of their own, as in Chrome.
        assert_eq!(pages("<div style='height: 820px'>x</div>"), 5);
    }

    #[test]
    fn lays_out_absolutely_positioned_boxes_in_columns() {
        fn widths(fragment: &Fragment, out: &mut Vec<(Au, Au)>) {
            out.push((fragment.rect.width, fragment.rect.height));
            for child in &fragment.children {
                widths(child, out);
            }
        }
        let pages = render(
            "<style>@page { size: 300px 200px; margin: 0 } body { margin: 0 }</style>
             <div style='position: absolute; columns: 4; gap: 0; column-fill: auto; width: 100px;
               height: 100px'><div style='height: 400px'></div></div>x",
        );
        let mut sizes = Vec::new();
        widths(&pages[0].fragments[0], &mut sizes);
        // Four columns as tall as the box, each a quarter of it wide, with a part of the
        // content in each, none of it in one.
        assert_eq!(
            sizes
                .iter()
                .filter(|&&size| size == (px(25.0), px(100.0)))
                .count(),
            8,
            "{sizes:?}"
        );
        assert!(sizes.iter().all(|&(_, height)| height < px(400.0)), "{sizes:?}");
    }

    #[test]
    fn places_margin_boxes_by_their_margins_toward_the_page_area() {
        let auto = LengthPercentageAuto::Auto;
        let zero = LengthPercentageAuto::ZERO;
        // Auto margins center a box; a box that leaves room is pushed away from the page's edge.
        assert_eq!(
            page_edge_margins((&auto, &auto), (Au::ZERO, Au::ZERO), px(30.0), px(100.0), true),
            (px(35.0), px(35.0))
        );
        assert_eq!(
            page_edge_margins((&zero, &zero), (Au::ZERO, Au::ZERO), px(30.0), px(100.0), true),
            (px(70.0), Au::ZERO)
        );
        assert_eq!(
            page_edge_margins((&zero, &zero), (Au::ZERO, Au::ZERO), px(30.0), px(100.0), false),
            (Au::ZERO, px(70.0))
        );
        let pages = render(
            "<style>@page { size: 500px 400px; margin: 100px;
               @top-left-corner { content: ''; width: 20px; height: 30px; margin: auto }
               @top-center { content: ''; width: 50px; height: 20px }
               @bottom-left { content: ''; height: 20px }
               @right-top { content: ''; width: 20px; height: 40px } }</style>x",
        );
        let rects: Vec<Rect> = pages[0]
            .fragments
            .iter()
            .skip(1)
            .map(|fragment| fragment.rect)
            .collect();
        assert_eq!(
            rects,
            [
                // A corner centered by its auto margins.
                Rect::new(px(40.0), px(35.0), px(20.0), px(30.0)),
                // Along the top edge as wide as it is, centered, at the bottom of the margin.
                Rect::new(px(225.0), px(80.0), px(50.0), px(20.0)),
                // Along the right edge at the left of the margin, as tall as it is.
                Rect::new(px(400.0), px(100.0), px(20.0), px(40.0)),
                // Along the bottom edge at its top, as wide as its share of it.
                Rect::new(px(100.0), px(300.0), px(300.0), px(20.0)),
            ]
        );
    }

    #[test]
    fn breaks_between_flex_items_on_pages_of_other_names() {
        let html = |direction: &str| {
            render(&format!(
                "<div>a</div><div style='display: flex; flex-direction: {direction}'>
                   <div style='page: b'>b</div><div style='page: c'>c</div></div><div>d</div>"
            ))
            .len()
        };
        assert_eq!(html("column"), 4);
        // The items of a row share a page, which takes the name of the first.
        assert_eq!(html("row"), 3);
        // Names deeper inside an item break the column too.
        let pages = render(
            "<div>a</div><div style='display: flex; flex-direction: column'><div><div>
               <div style='page: a'>b</div><div style='page: b'>c</div></div></div></div><div>d</div>",
        );
        assert_eq!(pages.len(), 4);
    }

    #[test]
    fn breaks_absolutely_positioned_boxes_at_forced_breaks_and_page_names() {
        let pages = render(
            "<style>@page { size: 300px }</style>
             <div style='position: absolute'>Page 1<div style='break-before: page'>Page 2</div></div>",
        );
        assert_eq!(pages.len(), 2);
        let pages = render(
            "<div style='position: absolute'><div style='page: a'>a</div><div style='page: b'>b</div></div>",
        );
        assert_eq!(pages.len(), 2);
    }

    #[test]
    fn adds_pages_for_boxes_positioned_in_boxes_on_later_pages() {
        let pages = render(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0 }</style>Five pages.
             <div style='position: absolute; top: 300px'>Second page
               <div style='position: absolute; top: 300px; height: 900px'>Third page</div></div>",
        );
        assert_eq!(pages.len(), 5);
        // Inside a box positioned on its placeholder's page as well.
        let pages = render(
            "<style>@page { size: 400px 300px; margin: 0 } body { margin: 0 }</style>
             <div style='position: absolute'>Three pages.
               <div style='position: absolute; top: 0; height: 900px'></div></div>",
        );
        assert_eq!(pages.len(), 3);
        // A box that clips its overflow adds none.
        let pages = render(
            "<style>@page { size: 400px 300px; margin: 0 }</style>
             <div style='position: absolute; overflow: clip; width: 10px; height: 10px'>
               <div style='position: absolute; height: 1000px'></div></div>",
        );
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn ignores_a_forced_break_after_nothing_but_positioned_boxes() {
        let pages = render(
            "<style>body { margin: 0 }</style><div style='position: fixed'>On both pages.</div>
             <div style='break-before: page; height: 10px'></div><div style='break-before: page; height: 10px'></div>",
        );
        assert_eq!(pages.len(), 2);
    }

    #[test]
    fn places_boxes_on_added_pages_of_their_own_heights() {
        // A positioned box below the last page goes on the page its top falls on with the pages
        // added as tall as the last one, and breaks where that page, which has its own height
        // (`:left` margins), ends: as in Chrome, on the fifth page here, whole.
        let html = |top: &str| {
            render(&format!(
                "<style>@page {{ size: 400px 200px; margin: 0 }} @page :left {{ margin: 50px }}
                   body {{ margin: 0 }}</style><div style='height: 300px'></div>
                 <div style='position: absolute; top: {top}; height: 200px'></div>"
            ))
            .len()
        };
        assert_eq!(html("500px"), 5);
        // On the fourth (a left page, 100 pixels tall), it breaks onto the fifth.
        assert_eq!(html("400px"), 5);
        // On the third (a right page, 200 pixels tall), it fits.
        assert_eq!(html("300px"), 3);
    }
}
