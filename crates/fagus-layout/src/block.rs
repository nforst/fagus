//! Block-level layout with fragmentation.
//!
//! Every layout function takes the space left on the current page and an optional break token
//! (where the previous page stopped) and returns a fragment plus, if the box did not fit, a break
//! token for the next page (ADR 2).

use std::cell::RefCell;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use fagus_style::values::{
    BoxSizing, BreakBetween, ContentAlignment, FlexDirection, FlexWrap, ItemAlignment, LengthPercentage,
    LengthPercentageAuto, MaxSize, Size, WritingMode,
};
use fagus_style::{ComputedStyle, ComputedValues};
use fagus_text::FontDb;

use crate::float::{FloatToken, FlowLimit};
use crate::fragment::{Children, Fragment, FragmentKind, VisibleSides};
use crate::inline::{intrinsic_widths_in, lines_in_flow};
use crate::tree::{BoxId, BoxKind, BoxTree};
use crate::units::{Au, Rect};

/// Where a broken box continues on the next page.
#[derive(Clone, Debug, PartialEq)]
pub enum BreakToken {
    /// A block container continues at child `index`, inside that child at `inner`. The root of
    /// a formatting context has the `floats` in it that continue too. The content of children
    /// that ended on a page before but overflow them continues at `overflow` (by child index)
    /// at the top of the page.
    Block {
        index: usize,
        inner: Option<Box<BreakToken>>,
        consumed: Consumed,
        floats: Vec<FloatToken>,
        overflow: Vec<(usize, BreakToken)>,
    },
    /// Inline content continues with the line that starts at piece `piece`, `offset` bytes into
    /// its text where a word broke inside (line breaking starts again there, beside the floats of
    /// the new page).
    Lines {
        piece: usize,
        offset: usize,
        consumed: Consumed,
        floats: Vec<FloatToken>,
    },
    /// A table continues at body row `row` (counted over all row groups); if that row (with the
    /// rows that cells starting in it span) was broken inside, its cells continue at `cells`
    /// (one per cell of the rows, `None` for cells that are done or have not started) and the
    /// next page starts `offset` below the top of the row, else `cells` is empty. `started` tells
    /// whether the table box began on the pages before, not only its captions. `overflow` is how
    /// far monolithic content of the rows on the page before (an image taller than the page)
    /// reaches into the next one: the rows go on below it there, as in Chrome, while a repeated
    /// header keeps its place at the top. `covered` tells that such content covered the page
    /// before: the rest of the broken cells goes on at the top of the next page, or, past the
    /// last rows (no cells left), the end of the table (its spacing and bottom border and
    /// padding) does, drawn below the overflow.
    Table {
        row: usize,
        cells: Vec<Option<BreakToken>>,
        offset: Au,
        started: bool,
        overflow: Au,
        covered: bool,
    },
    /// A box whose content is done continues with `rest` of its height (a box taller than its
    /// content, broken at the end of a page).
    Rest { rest: Au },
    /// A flex or grid container continues at line `line` (a flex line, or a group of grid rows
    /// that no item spans out of). If that line broke inside, `inside` holds a token per item of
    /// the line (`None` for items that are done or have not started) and the place in the line
    /// at which the next page starts. `overflow` is how much taller than its content the
    /// container is on the next page: what the monolithic content of its items that go on
    /// reached past the end of the page before, beyond the rest of its rows (see
    /// `grid_item_overflow`).
    Container {
        line: usize,
        inside: Option<(Vec<Option<BreakToken>>, Au)>,
        consumed: Consumed,
        overflow: Au,
    },
    /// A column flex container with a fixed or minimum height continues each of its flex lines
    /// where `lines` tells (see [`crate::flex_column`]).
    Columns {
        lines: Vec<crate::flex_column::ColumnResume>,
        consumed: Consumed,
    },
    /// A table whose captions above it break: they go on with caption `index` (among all of
    /// the table's captions), inside it at `inner` if it broke inside, and the table follows
    /// them as if it started on this page.
    Captions {
        index: usize,
        inner: Option<Box<BreakToken>>,
    },
    /// A box that ended on a page before, whose content overflows it and continues at `inner`
    /// on this page: at the top, without the box and beside the content that follows the box,
    /// which it does not move (in a parallel flow, as Chrome lays out overflowing content).
    Overflow { inner: Box<BreakToken> },
    /// A flex item that has not started: it did not fit on the page before beside the items of
    /// its line that stayed there, and starts at the top of this one (Blink's break token
    /// before a box).
    Before,
    /// A table cell that ended on a page before beside a cell of its rows broken by a forced
    /// break, its part stretched to the end of the rows' part there: it has no part on the
    /// pages after, as in Chrome.
    Ended,
}

impl BreakToken {
    /// A block container that continues at child `index`; the height it consumed is set once its
    /// part on the page is done.
    pub(crate) fn at(index: usize, inner: Option<Box<BreakToken>>) -> Self {
        Self::Block {
            index,
            inner,
            consumed: Consumed::default(),
            floats: Vec::new(),
            overflow: Vec::new(),
        }
    }

    /// The floats of a formatting context that continue at the token.
    pub(crate) fn floats(&self) -> &[FloatToken] {
        match self {
            Self::Block { floats, .. } | Self::Lines { floats, .. } => floats,
            Self::Overflow { inner } => inner.floats(),
            Self::Table { .. }
            | Self::Captions { .. }
            | Self::Rest { .. }
            | Self::Container { .. }
            | Self::Columns { .. }
            | Self::Before
            | Self::Ended => &[],
        }
    }

    /// Whether the token breaks a box in the empty space below its content (the box is taller
    /// than its content), rather than between pieces of content.
    pub(crate) fn splits_empty_space(&self) -> bool {
        match self {
            Self::Rest { .. } => true,
            Self::Block {
                inner: Some(inner), ..
            } => inner.splits_empty_space(),
            _ => false,
        }
    }

    /// The height of the box's content box on the pages before.
    pub(crate) fn consumed(&self) -> Au {
        match self {
            Self::Block { consumed, .. }
            | Self::Lines { consumed, .. }
            | Self::Container { consumed, .. }
            | Self::Columns { consumed, .. } => consumed.0,
            Self::Table { .. }
            | Self::Captions { .. }
            | Self::Rest { .. }
            | Self::Overflow { .. }
            | Self::Before
            | Self::Ended => Au::ZERO,
        }
    }

    /// Records the height of the box's content box on this page and the ones before.
    pub(crate) fn set_consumed(&mut self, height: Au) {
        if let Self::Block { consumed, .. }
        | Self::Lines { consumed, .. }
        | Self::Container { consumed, .. }
        | Self::Columns { consumed, .. } = self
        {
            *consumed = Consumed(height);
        }
    }
}

/// The height of a broken box's content box on the pages before a break, stretched parts
/// included: the box's height, minimum and maximum height hold for all of its parts together, so
/// they leave its last part what remains (CSS Fragmentation 3, §5.3). Tokens are equal when they
/// continue at the same place, whatever the pages before took.
#[derive(Clone, Copy, Debug, Default)]
pub struct Consumed(Au);

impl PartialEq for Consumed {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// What block layout does when a box's bottom border and padding do not fit below its content
/// at the end of a page.
#[derive(Clone, Copy)]
pub(crate) enum Ending {
    /// Tell where the content ends, to lay out the box again.
    Retry,
    /// Break the content before this position.
    Before(Au),
    /// Move the border and padding to the next page on their own.
    Alone,
}

/// The sizes a block container is given rather than finding them itself.
#[derive(Clone, Copy, Debug)]
struct Sizes {
    /// The width of its content box (flex and grid items, table cells).
    width: Option<Au>,
    /// The height of its border box (stretched items).
    height: Option<Au>,
    /// Whether its height is fixed at that of its content (see [`Layouter::content_sized`]).
    content: bool,
    /// Whether the height it is given does not grow with its content where it breaks (see
    /// [`Layouter::fixed_item_height`]).
    fixed: bool,
    /// Whether its own height gives way to its content's (see [`Layouter::indefinite_basis`]).
    indefinite: bool,
}

/// The space a box is laid out in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Space {
    /// Width of the containing block.
    pub containing_width: Au,
    /// Height of the containing block if it does not depend on the content, for percentages.
    pub containing_height: Option<Au>,
    /// Space left until the end of the page; `None` without fragmentation.
    pub remaining: Option<Au>,
    /// Whether nothing precedes the box on the page. A box at the top of a page is always placed,
    /// even if it does not fit, so that layout makes progress.
    pub at_top: bool,
    /// Whether, at the top of a page, nothing lies above the box there either: no margin,
    /// border or padding of it or of the boxes around it. Monolithic content (an image, a
    /// line) that does not fit moves to the next page unless it starts at the very top of the
    /// page, as in Chrome (Blink's `refuse_break_before` in `MovePastBreakpoint`), so the box
    /// around it keeps only its top border and padding on this page. Only counts `at_top`
    /// (see [`Layouter::at_page_top`]).
    pub flush: bool,
    /// Where the left edge of the containing block's content box lies in the block formatting
    /// context, for lines and boxes beside floats.
    pub bfc_left: Au,
    /// Where the top of the box's border box lies in the block formatting context.
    pub bfc_top: Au,
    /// Whether the containing block is right to left: a box whose width and margins leave room
    /// then keeps its right margin.
    pub rtl: bool,
    /// Whether `stretch` in `height`, `min-height` and `max-height` leaves out the box's top and
    /// bottom margins: those at an edge of a parent without border or padding there, which is
    /// no new formatting context, as in Chrome (where they may collapse through the parent).
    pub stretch_skips_margins: (bool, bool),
}

impl Space {
    pub(crate) fn unfragmented(containing_width: Au) -> Self {
        Self {
            containing_width,
            containing_height: None,
            remaining: None,
            at_top: true,
            flush: true,
            bfc_left: Au::ZERO,
            bfc_top: Au::ZERO,
            rtl: false,
            stretch_skips_margins: (false, false),
        }
    }

    /// `model` without the margins that `stretch` heights leave out, for sizing the box.
    pub(crate) fn stretch_model(&self, model: &BoxModel) -> BoxModel {
        let mut model = *model;
        if self.stretch_skips_margins.0 {
            model.margin[TOP] = Au::ZERO;
        }
        if self.stretch_skips_margins.1 {
            model.margin[BOTTOM] = Au::ZERO;
        }
        model
    }
}

/// Keeps a block formatting context open while its root is laid out.
struct Context<'l, 'a>(&'l Layouter<'a>);

impl<'l, 'a> Context<'l, 'a> {
    fn open(layouter: &'l Layouter<'a>) -> Self {
        layouter.push_context();
        Self(layouter)
    }
}

impl Drop for Context<'_, '_> {
    fn drop(&mut self) {
        self.0.pop_context();
    }
}

/// Sets where content of the current formatting context ends on the page while a box is laid
/// out.
struct FlowScope<'l, 'a> {
    layouter: &'l Layouter<'a>,
    outer: Option<FlowLimit>,
}

impl<'l, 'a> FlowScope<'l, 'a> {
    fn open(layouter: &'l Layouter<'a>, limit: Option<FlowLimit>) -> Self {
        Self {
            layouter,
            outer: layouter.set_flow_limit(limit),
        }
    }
}

impl Drop for FlowScope<'_, '_> {
    fn drop(&mut self) {
        self.layouter.set_flow_limit(self.outer);
    }
}

/// Sets the height that percentage heights of replaced elements refer to in a table cell of a
/// fixed height, while a block container is laid out (see `Layouter::cell_replaced_height`).
struct CellReplacedScope<'l, 'a> {
    layouter: &'l Layouter<'a>,
    outer: Option<Au>,
}

impl<'l, 'a> CellReplacedScope<'l, 'a> {
    /// Opens the scope of a block container with `style`, `model` and whether it is anonymous:
    /// a cell of a fixed height sets its content height, an anonymous box keeps its parent's,
    /// and any other box ends it.
    fn open(layouter: &'l Layouter<'a>, style: &ComputedStyle, model: &BoxModel, anonymous: bool) -> Self {
        let height = if style.display
            == fagus_style::values::Display::Internal(fagus_style::values::InternalDisplay::TableCell)
        {
            match &style.height {
                Size::Length(length) => length.fixed().map(|px| {
                    let frame =
                        model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
                    let height = Au::from_px(px);
                    if style.box_sizing == BoxSizing::BorderBox {
                        (height - frame).clamp_non_negative()
                    } else {
                        height
                    }
                }),
                _ => None,
            }
        } else if anonymous {
            layouter.cell_replaced_height.get()
        } else {
            None
        };
        Self {
            layouter,
            outer: layouter.cell_replaced_height.replace(height),
        }
    }
}

impl Drop for CellReplacedScope<'_, '_> {
    fn drop(&mut self) {
        self.layouter.cell_replaced_height.set(self.outer);
    }
}

/// The room below the end before which the content of a box has to end (see [`Ending::Before`])
/// to the end of the page, added to that of the boxes around it while the box lays out its
/// content (see [`Layouter::early_slack`]).
struct SlackScope<'l, 'a> {
    layouter: &'l Layouter<'a>,
    outer: Au,
}

impl<'l, 'a> SlackScope<'l, 'a> {
    /// Adds `slack` to the room of the boxes around.
    fn open(layouter: &'l Layouter<'a>, slack: Au) -> Self {
        let outer = layouter.early_slack.get();
        layouter.early_slack.set(outer + slack);
        Self { layouter, outer }
    }

    /// Sets the room to `slack`, for a child laid out with all of the room of its parent.
    fn set(layouter: &'l Layouter<'a>, slack: Au) -> Self {
        let outer = layouter.early_slack.replace(slack);
        Self { layouter, outer }
    }
}

impl Drop for SlackScope<'_, '_> {
    fn drop(&mut self) {
        self.layouter.early_slack.set(self.outer);
    }
}

/// The list item whose content a block container lays out, while it is laid out: Chrome places
/// an outside marker beside the list item's border box (Blink's
/// `ListMarker::InlineMarginsForOutside`, relative to the list item), whatever padding, border or
/// nested blocks stand between it and the line that holds the marker.
struct ListItemScope<'l, 'a> {
    layouter: &'l Layouter<'a>,
    outer: Option<(Au, Au)>,
    /// What the layouter knew of the list item around before: whether there was one, and
    /// whether its marker was laid out in a formatting context inside it.
    outer_item: (bool, bool),
    list_item: bool,
}

impl<'l, 'a> ListItemScope<'l, 'a> {
    /// Opens the scope of a box with `style` whose border box spans `edges` in the coordinates
    /// of its formatting context, which it establishes itself if `root`.
    fn open(layouter: &'l Layouter<'a>, style: &ComputedStyle, edges: (Au, Au), root: bool) -> Option<Self> {
        let list_item = matches!(
            style.display,
            fagus_style::values::Display::Normal { list_item: true, .. }
        );
        if !list_item && !root {
            return None;
        }
        // Lines inside a new formatting context have coordinates of their own: an outside
        // marker there is placed once the list item is laid out (`place_nested_marker`).
        let inner = list_item.then_some(edges);
        let outer_item = (layouter.in_list_item.get(), layouter.unplaced_marker.get());
        if list_item {
            layouter.in_list_item.set(true);
            layouter.unplaced_marker.set(false);
        }
        Some(Self {
            layouter,
            outer: layouter.list_item_edges.replace(inner),
            outer_item,
            list_item,
        })
    }
}

impl Drop for ListItemScope<'_, '_> {
    fn drop(&mut self) {
        self.layouter.list_item_edges.set(self.outer);
        if self.list_item {
            self.layouter.in_list_item.set(self.outer_item.0);
            self.layouter.unplaced_marker.set(self.outer_item.1);
        }
    }
}

impl Layouter<'_> {
    /// Lays out a box with `style` by `layout` where it lays out its lines in formatting
    /// contexts of their own (columns): if it is a list item, its outside marker is placed
    /// beside its border box afterwards.
    pub(crate) fn list_item_in_contexts(
        &self,
        style: &ComputedStyle,
        layout: impl FnOnce() -> Option<Outcome>,
    ) -> Option<Outcome> {
        let scope = ListItemScope::open(self, style, (Au::ZERO, Au::ZERO), false);
        let mut outcome = layout();
        if scope.is_some()
            && self.unplaced_marker.get()
            && !style.is_vertical()
            && let Some(outcome) = &mut outcome
        {
            let width = outcome.fragment.rect.width;
            place_nested_marker(&mut outcome.fragment.children, Au::ZERO, width);
        }
        outcome
    }
}

/// Moves the outside marker of a list item `width` wide, found in the first line that holds one
/// in `children` (whose border boxes lie `offset` from the item's), from beside that line to
/// beside the item: `start` from the item's border box (its end in right-to-left text) instead of
/// from the line. Lines of list items inside are left alone. Returns whether it found the marker.
fn place_nested_marker(children: &mut [Fragment], offset: Au, width: Au) -> bool {
    for child in children {
        let at = offset + child.rect.x;
        match &child.kind {
            FragmentKind::Box { style, .. }
                if matches!(
                    style.display,
                    fagus_style::values::Display::Normal { list_item: true, .. }
                ) => {}
            FragmentKind::Line { .. } => {
                let line_width = child.rect.width;
                let mut found = false;
                for run in &mut child.children {
                    if let FragmentKind::Text(text) = &run.kind
                        && text.marker
                    {
                        if text.style.is_rtl() {
                            run.rect.x += width - line_width - at;
                        } else {
                            run.rect.x -= at;
                        }
                        found = true;
                    }
                }
                if found {
                    return true;
                }
            }
            _ => {
                if place_nested_marker(&mut child.children, at, width) {
                    return true;
                }
            }
        }
    }
    false
}

/// What block layout had reached before it placed a child.
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools, reason = "independent states of the layout")]
struct Placement {
    index: usize,
    children: usize,
    y: Au,
    previous: Margin,
    leading: bool,
    collapsed_top: Margin,
    floats: usize,
    placed: bool,
    /// The child's `break-before` and `break-after` avoid a break.
    avoid_before: bool,
    avoid_after: bool,
    /// The space the child was laid out in, where it started on the page, and the end of the
    /// room for content there.
    space: Option<(Space, Au)>,
}

/// The result of laying out a box.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub fragment: Fragment,
    pub token: Option<BreakToken>,
    /// The box's own bottom margin, for layouts that do not collapse margins (flex, grid).
    pub margin_bottom: Au,
    /// The margins above and below the box in block flow, collapsed with those of its first and
    /// last children where they adjoin (CSS 2.1 §8.3.1).
    pub collapsed_top: Margin,
    pub collapsed_bottom: Margin,
    /// Whether the box is empty and its top and bottom margins collapse through it.
    pub through: bool,
    /// The `break-before` and `break-after` that layout finds the box passes on to its parent,
    /// besides those its first and last children pass on (see `BoxTree::break_before`): those
    /// of all items of the first and last lines of a flex or grid container, as in Chrome,
    /// passed on through the first and last children of blocks.
    pub passed_breaks: (BreakBetween, BreakBetween),
}

impl Outcome {
    /// The outcome of a box whose margins do not collapse with its children's.
    pub(crate) fn new(
        fragment: Fragment,
        token: Option<BreakToken>,
        margin_top: Au,
        margin_bottom: Au,
    ) -> Self {
        Self {
            fragment,
            token,
            margin_bottom,
            collapsed_top: Margin::of(margin_top),
            collapsed_bottom: Margin::of(margin_bottom),
            through: false,
            passed_breaks: (BreakBetween::Auto, BreakBetween::Auto),
        }
    }
}

/// Adjoining margins collapse to the largest positive one plus the most negative one
/// (CSS 2.1 §8.3.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Margin {
    positive: Au,
    negative: Au,
}

impl Margin {
    pub fn of(value: Au) -> Self {
        if value >= Au::ZERO {
            Self {
                positive: value,
                negative: Au::ZERO,
            }
        } else {
            Self {
                positive: Au::ZERO,
                negative: value,
            }
        }
    }

    #[must_use]
    pub fn adjoin(self, other: Self) -> Self {
        Self {
            positive: self.positive.max(other.positive),
            negative: self.negative.min(other.negative),
        }
    }

    pub fn resolve(self) -> Au {
        self.positive + self.negative
    }
}

/// Resolved margins, borders and padding of a box.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BoxModel {
    pub(crate) margin: [Au; 4],
    pub(crate) border: [Au; 4],
    pub(crate) padding: [Au; 4],
    pub(crate) content_width: Au,
}

pub(crate) const TOP: usize = 0;
pub(crate) const RIGHT: usize = 1;
pub(crate) const BOTTOM: usize = 2;
pub(crate) const LEFT: usize = 3;

/// What a table layout is cached by: the table, the width of its containing block, whether it
/// fills that width and whether its own heights give way (see `Layouter::table_fills`,
/// `Layouter::table_unsized`), how much its rows grow (see `Layouter::table_grow`)
/// and the height of its containing block, for a percentage height, and whether its cells are
/// aligned on the baseline of their rows (see `Layouter::table`), and the width of the containing
/// block where the table started, whose rows it keeps on later pages (see `Layouter::table`).
pub(crate) type TableKey = (BoxId, Au, (bool, bool), Au, Option<Au>, bool, Option<Au>);

pub struct Layouter<'a> {
    pub tree: &'a BoxTree,
    pub fonts: &'a FontDb,
    /// Intrinsic widths per box: they do not depend on the page, so tables that span many pages
    /// measure their cells once.
    pub(crate) intrinsic_cache: RefCell<FxHashMap<BoxId, (Au, Au)>>,
    /// The boxes whose intrinsic widths depend on the page (see
    /// [`Layouter::forget_page_widths`]).
    pub(crate) page_widths: RefCell<crate::memo::PageWidths>,
    /// Min-content and max-content widths of content boxes, whatever their `width` (see
    /// [`Layouter::content_box_widths`]).
    pub(crate) content_cache: RefCell<FxHashMap<BoxId, (Au, Au)>>,
    /// Tables laid out per width; every page of a table takes its rows from the same layout.
    pub(crate) table_cache: RefCell<FxHashMap<TableKey, std::rc::Rc<crate::table::Laid>>>,
    /// The styles made for cells with collapsed borders and for the pieces of those borders.
    pub(crate) collapsed_styles: RefCell<crate::table::CollapsedStyles>,
    /// Whether the table laid out next is a flex or grid item whose container gives it its
    /// width (stretched or flexed): an auto-width table fills its containing block then, as in
    /// Chrome, and does not shrink to its content.
    pub(crate) table_fills: std::cell::Cell<bool>,
    /// How much taller than their content the body rows of the table laid out next grow
    /// together: a flex or grid item stretched (or sized) taller than the table gives its rows
    /// that room, as Chrome does with a fixed height (see `Layouter::item`).
    pub(crate) table_grow: std::cell::Cell<Au>,
    /// Whether the table laid out next is a flex or grid item whose container gives it its
    /// height: its own `height`, `min-height` and `max-height` give way then, and it is as tall
    /// as its rows and captions (grown by `table_grow`), as in Chrome, where a table takes a
    /// forced block size as that of its captions and rows.
    pub(crate) table_unsized: std::cell::Cell<bool>,
    /// Whether the flex container laid out next has the height its own container gave it
    /// (stretched or flexed), which is definite for the percentage heights of its items (CSS
    /// Flexbox 1, §9.8), as in Chrome.
    pub(crate) given_height: std::cell::Cell<bool>,
    /// Whether the absolutely positioned box laid out next is aligned in a grid area by the grid
    /// (see `Layouter::absolute_in_grid_area`), not at its static position by its own alignment.
    pub(crate) grid_aligned: std::cell::Cell<bool>,
    /// How many grids are in a first pass that only finds the sizes of their rows for a second
    /// (see `Layouter::grid`): grids inside one size their tracks once.
    pub(crate) provisional_grids: std::cell::Cell<u32>,
    /// How many grid items are being laid out only to find their heights, before they are laid
    /// out again stretched to their areas (see `Layouter::grid_pass`): grids inside them do not
    /// lay out their own such items again.
    pub(crate) measured_items: std::cell::Cell<u32>,
    /// The number of the page being laid out and the page count (0 while it is not known), for
    /// `counter(page)` and `counter(pages)` in generated content.
    pub(crate) page_number: std::cell::Cell<(usize, usize)>,
    /// The value of the page counter on each page numbered so far, from the first (see
    /// `page::PageNumbering`); where a page has none, `counter(page)` is its number.
    pub(crate) page_values: RefCell<Vec<i32>>,
    /// The value of the page counter in the margin box being laid out, where it resets, sets or
    /// increments the counter of its own.
    pub(crate) margin_page_value: std::cell::Cell<Option<i32>>,
    /// Where an `@page` rule resets the `footnote` counter for the page being laid out (or the
    /// page the footnote being laid out is called on): the number of the first footnote called
    /// on the page (from 0, in document order) and the value the counter is reset to. The
    /// footnotes called there count from that value on.
    pub(crate) footnote_numbering: std::cell::Cell<Option<(u32, i32)>>,
    /// The style of the footnote areas of the page being laid out (`@footnote`), which the
    /// footnote areas of columns take too (see `footnote::Layouter::column_footnotes`).
    pub(crate) footnote_area: RefCell<Option<Arc<ComputedStyle>>>,
    /// Whether the document has footnotes with `float-reference: column`, which go into
    /// footnote areas of the columns their calls are in.
    pub(crate) column_footnotes: bool,
    /// Where `string()` in generated content takes its value from in the string log, while the
    /// rows of a table's header or footer are laid out for one page; `None` elsewhere.
    pub(crate) string_position: std::cell::Cell<Option<usize>>,
    /// The definite content height of the block container whose lines are being laid out, which
    /// percentage heights of replaced elements in them refer to.
    pub(crate) inline_containing_height: std::cell::Cell<Option<Au>>,
    /// The definite content height of the box whose min-content and max-content widths are
    /// being found, if its height is fixed: percentage heights of replaced elements in it (in
    /// anonymous boxes too, not in other boxes) refer to it, as in layout, so that their widths
    /// follow from it through their ratio.
    intrinsic_height: std::cell::Cell<Option<Au>>,
    /// A box whose min-content and max-content widths are being found at a content height its
    /// container gives it (a grid item in rows of a known size), and that height: it replaces
    /// the box's own definite height, and the widths found so are not cached.
    pub(crate) height_for_widths: std::cell::Cell<Option<(BoxId, Option<Au>)>>,
    /// The width the percentage paddings of the cells of the table being laid out refer to:
    /// its columns with the spacing between them, as in Blink (see `Layouter::cell_basis_for`).
    pub(crate) cell_basis: std::cell::Cell<Option<Au>>,
    /// The box (by the address of its style) that avoids floats and is laid out in the room
    /// beside them, and the width of its containing block, which its percentage width still
    /// refers to, as in Blink (the percentage resolution size of `LayoutNewFormattingContext`).
    beside_floats_basis: std::cell::Cell<Option<(usize, Au)>>,
    /// The content height of the table cell of a fixed height whose content (in anonymous boxes
    /// and lines, not in other boxes) is being laid out: percentage heights of replaced elements
    /// there refer to it on every layout of the cell, whatever the height of its row (Blink's
    /// `CalculateReplacedChildPercentageSize`), so that they keep their size.
    pub(crate) cell_replaced_height: std::cell::Cell<Option<Au>>,
    /// Whether the table cell being measured is in a table of a specified height, where a scroll
    /// container with a percentage height in it counts as having none (Blink's
    /// `IsRestrictedBlockSizeTableCell`).
    pub(crate) restricted_cell: std::cell::Cell<bool>,
    /// The absolutely positioned children of flex containers, with where their static positions
    /// align in the container (see [`FlexStatic`]).
    pub(crate) flex_statics: RefCell<FxHashMap<BoxId, FlexStatic>>,
    /// The absolutely positioned children of block containers that `justify-self` moves off
    /// the start of the line their static position is on: how wide that line is, and how much
    /// of the room beside the box goes before it (see [`crate::block_justify::static_fraction`]).
    pub(crate) block_statics: RefCell<FxHashMap<BoxId, (Au, f32)>>,
    /// The left and right edges of the border box of the list item whose content is being laid
    /// out, in the coordinates of the block formatting context of its lines: outside markers
    /// stand beside its border box (see [`ListItemScope`]). `None` outside list items, and
    /// inside a formatting context that a list item's descendant establishes.
    pub(crate) list_item_edges: std::cell::Cell<Option<(Au, Au)>>,
    /// Whether the content being laid out is inside a list item.
    pub(crate) in_list_item: std::cell::Cell<bool>,
    /// Whether the outside marker of the list item being laid out was laid out in a formatting
    /// context inside it, beside the line that holds it rather than beside the item.
    pub(crate) unplaced_marker: std::cell::Cell<bool>,
    /// The floats of the block formatting contexts being laid out, innermost last.
    pub(crate) contexts: RefCell<Vec<crate::float::Exclusions>>,
    /// How many columns the box being laid out is in (columns in columns count twice): inside
    /// one, `break-before: column` and `break-after: column` break.
    pub(crate) columns: std::cell::Cell<u32>,
    /// The content box of the multi-column container whose columns are being laid out: its
    /// spanners end a row of columns.
    pub(crate) spanning: std::cell::Cell<Option<BoxId>>,
    /// Whether the column being laid out continues the content right below a spanner, where
    /// margins are not truncated as they are after a break.
    pub(crate) below_spanner: std::cell::Cell<bool>,
    /// Whether the column being laid out takes a break that leaves fewer lines than `orphans`
    /// asks for, or else one that `break-before` and `break-after` avoid, rather than none at
    /// all: the first column of a row below a spanner, where Chrome accepts any break
    /// (`kBreakAppealLastResort`) when there is no better one, the orphans first.
    pub(crate) last_resort_column: std::cell::Cell<bool>,
    /// Whether the content of columns is being measured in a single column, where it does not
    /// break before or after boxes.
    pub(crate) measuring: std::cell::Cell<bool>,
    /// Whether the page or column being laid out starts after a forced break, where margins
    /// are kept (CSS Fragmentation 3, §5.2), and whether it ends with one.
    pub(crate) after_forced_break: std::cell::Cell<bool>,
    pub(crate) forced_break: std::cell::Cell<bool>,
    /// Whether monolithic content at the top of a page stays there even below a margin, border
    /// or padding: where rows of a table that go on from the page before would get no further
    /// otherwise (see [`Space::flush`]).
    pub(crate) lenient_top: std::cell::Cell<bool>,
    /// How far monolithic content of the pages before reaches into the page being laid out,
    /// which the content of the page starts below (see `page::paginate`).
    pub(crate) page_overflow: std::cell::Cell<Au>,
    /// Whether the content being laid out has to end before the bottom border and padding of a
    /// box around it (see [`Layouter::ending_on_page`]) rather than at the end of the page: the
    /// spacing below a table's last rows does not shrink to fit then, as in Chrome.
    pub(crate) ending_early: std::cell::Cell<bool>,
    /// How far below where the content being laid out has to end (see [`Ending::Before`]) the
    /// page ends: a box broken inside that content still reaches down to the end of the page,
    /// as in Chrome (Blink's early break leaves the fragment the room of the fragmentainer).
    pub(crate) early_slack: std::cell::Cell<Au>,
    /// How many block-level boxes have been placed in block flow so far, and how many had been
    /// when the box whose content ends early (see [`Layouter::ending_early`]) started: where
    /// more have, a paragraph in it is preceded by a break between boxes inside it.
    pub(crate) blocks_placed: std::cell::Cell<(u64, u64)>,
    /// Whether the table laid out next is an item of a column flex container, which breaks
    /// before its bottom border and padding where they do not fit (see `Layouter::table`).
    pub(crate) table_frame_breaks: std::cell::Cell<bool>,
    /// Whether the content being laid out has to end before the bottom border and padding of a
    /// block around it, where its last line may go to the next page alone, whatever `orphans`
    /// asks for, as in Chrome; not in flex items.
    pub(crate) last_line_alone: std::cell::Cell<bool>,
    /// Whether the block container laid out next is an item of a column flex container, whose
    /// height is fixed at that of its content, as in Chrome: where it breaks, the bottom margin
    /// of its last child, which the break truncates, leaves room for a break before its bottom
    /// border and padding.
    pub(crate) content_sized: std::cell::Cell<bool>,
    /// Whether the block container laid out next is an item of a column flex container whose
    /// height is its content's, with a percentage or content-based flex basis, which is its
    /// content's height there: its own height gives way to that, and percentage heights of its
    /// children are `auto`, also where it is laid out again at its flexed height, as in Chrome
    /// (Blink's `is_initial_block_size_indefinite`).
    pub(crate) indefinite_basis: std::cell::Cell<bool>,
    /// Whether the block container laid out next is a flex item that keeps the height the
    /// container gives it where it breaks, as in Chrome (Blink's
    /// `MinBlockSizeShouldEncompassIntrinsicSize` is false for it): its content overflows it
    /// and goes on past its end, as that of a box of a fixed height does.
    pub(crate) fixed_item_height: std::cell::Cell<bool>,
    /// The height of the column being laid out, if it is known (see `Layouter::multicol`).
    pub(crate) column_height: std::cell::Cell<Option<Au>>,
    /// The least amount of room that content which did not fit at the end of a page or column
    /// needed beyond it, since it was last taken: how much taller columns being balanced must
    /// grow for more of the content to fit in them (Blink's minimal space shortage).
    pub(crate) space_shortage: std::cell::Cell<Option<Au>>,
    /// How many rows of columns being balanced take the space shortage of their content at the
    /// moment (see `Layouter::row`).
    pub(crate) shortage_taken: std::cell::Cell<u32>,
    /// Whether the document has absolutely positioned or fixed boxes, found when first needed.
    pub(crate) has_out_of_flow: std::cell::OnceCell<bool>,
    /// Whether absolutely positioned boxes are being placed during layout (in columns, see
    /// `Layouter::position_in_columns`): the relative offsets inside them are then left to the
    /// page, which applies them to everything on it.
    pub(crate) relative_later: std::cell::Cell<bool>,
    /// The absolutely positioned boxes painted by a stacking context inside their containing
    /// block, by key, with that stacking context's key (see [`crate::stacking_owner`]).
    pub(crate) stacking_owners: RefCell<FxHashMap<crate::positioned::ChainKey, crate::positioned::ChainKey>>,
    /// The multi-column container being laid out, innermost, and whether it lies in the
    /// columns of another (see `Layouter::in_multicol`).
    pub(crate) multicol_container: std::cell::Cell<Option<(BoxId, bool)>>,
    /// Whether the items of a grid row being laid out break inside even where they avoid it,
    /// as where the break before the row is avoided too.
    pub(crate) breaks_avoided_items: std::cell::Cell<bool>,
    /// Whether the block laid out next may take a poor break inside, every break before it on
    /// the page being avoided: between lines where that leaves fewer lines than `orphans` and
    /// `widows` ask for, or inside the items of a grid row that avoid breaks inside where the
    /// break before the row is avoided too.
    pub(crate) poor_lines: std::cell::Cell<bool>,
    /// Whether the block laid out next breaks inside boxes that avoid breaks inside, as it does
    /// where every break before it on the page is avoided too and it does not fit otherwise:
    /// both breaks violate an `avoid`, and the later one is taken, as in Chrome (Blink ranks
    /// both `kBreakAppealViolatingBreakAvoid` and takes the latest break of the best appeal).
    pub(crate) breaks_avoided_inside: std::cell::Cell<bool>,
    /// The width of the containing block of each table where it starts.
    pub(crate) table_widths: RefCell<FxHashMap<BoxId, Au>>,
    /// Whether the header and the footer of each table repeat, as decided where the table
    /// starts.
    pub(crate) table_repeats: RefCell<FxHashMap<BoxId, (crate::table::Repeating, crate::table::Repeating)>>,
    /// How many more lines the box with `line-clamp` being laid out shows; `None` outside one.
    pub(crate) line_budget: std::cell::Cell<Option<usize>>,
    /// Whether content follows the last line that the `line-clamp` being laid out shows, which
    /// then ends with an ellipsis even where it ends its paragraph.
    pub(crate) clamp_follows: std::cell::Cell<bool>,
    /// The tracks grids pass down to their subgrids.
    pub(crate) subgrids: RefCell<crate::subgrid::Subgrids>,
    /// The margin above the block container about to be laid out, collapsed with those it
    /// adjoins, into which the emphasis marks and ruby annotations of its first line may reach.
    pub(crate) margin_above: std::cell::Cell<Au>,
    /// The room the last line of the block container laid out last leaves under its text,
    /// where it has annotations: the next block's annotations may reach into it.
    pub(crate) room_below: std::cell::Cell<Au>,
    /// Whether the block container about to be laid out has the first and the last formatted
    /// line of a box with `text-box-trim`, to trim them.
    pub(crate) trim_pending: std::cell::Cell<(bool, bool)>,
    /// Whether lines at the start and end of a column are trimmed (`text-box-trim` of the
    /// multi-column container), as in Blink: the first line of a box resumed in a column, and
    /// a line that fits at the end of a column only trimmed.
    pub(crate) fragmentainer_trim: std::cell::Cell<(bool, bool)>,
    /// The writing mode of the boxes being laid out, in whose coordinates their fragments are.
    pub(crate) frame: std::cell::Cell<WritingMode>,
    /// The width and height of the initial containing block (the page area), which gives
    /// orthogonal flows their room.
    pub(crate) initial_containing_block: std::cell::Cell<(Au, Au)>,
    /// The definite height of the flex container whose items are being laid out, which gives
    /// orthogonal items their room.
    pub(crate) cross_room: std::cell::Cell<Option<Au>>,
    /// The out-of-flow boxes whose static position is in a right-to-left block container: they
    /// start at its right edge, whatever their own direction (CSS 2.1, §10.3.7).
    static_rtl: rustc_hash::FxHashSet<BoxId>,
    /// The out-of-flow boxes whose containing block is right to left: where both of their
    /// inline insets are given and their margins too, the right one wins (CSS 2.1, §10.3.7).
    rtl_containers: rustc_hash::FxHashSet<BoxId>,
    /// The inline-level out-of-flow boxes in lines, and whether the bidi level of their place
    /// there is right to left: they start at their static position's right edge then, and at
    /// its left edge in a left-to-right run of a right-to-left line, as in Chrome (Blink's
    /// `LogicalLineItem::container_direction`).
    inline_static_rtl: RefCell<FxHashMap<BoxId, bool>>,
    /// The tracks of the grid containers that are containing blocks of absolutely positioned
    /// boxes, by element, as their last layout found them (see [`crate::grid_positioned`]).
    pub(crate) grid_tracks: RefCell<FxHashMap<fagus_dom::NodeId, crate::grid_positioned::GridTracks>>,
    /// What the faces say about their fullwidth punctuation, for `text-spacing-trim`.
    pub(crate) han_fonts: crate::inline::spacing::FontCache,
    /// The shaped content of the paragraph laid out last, for its next page.
    pub(crate) shaped: crate::inline::ShapedCache,
    /// The tables inside a table cell, flex or grid container, found when first needed.
    pub(crate) nested_tables: std::cell::OnceCell<rustc_hash::FxHashSet<BoxId>>,
    /// How much taller than laid out whole the rows of grids broken inside them grew, by grid
    /// and row (its line): Chrome leaves out of the row gaps that cross the end of a page only
    /// what the grid laid out whole has there, without the growth of the rows above.
    pub(crate) grid_growth: RefCell<FxHashMap<(BoxId, usize), Au>>,
    /// How many boxes that Chrome lays out without fragmentation the content being laid out is
    /// in (see `crate::unfragmented`).
    pub(crate) unfragmented: std::cell::Cell<u32>,
    /// Whether the table laid out next is an inline table, laid out as an atomic inline, whose
    /// cells Chrome aligns on the baseline of their rows (see `Layouter::laid_table`).
    pub(crate) inline_table: std::cell::Cell<bool>,
    /// The layouts of table cells broken on the page being laid out (see [`crate::memo`]).
    pub(crate) cell_memo: RefCell<crate::memo::CellMemo>,
    /// The layouts of flex and grid containers broken across pages and of fixed boxes (see
    /// [`crate::memo`]).
    pub(crate) pages_memo: RefCell<crate::memo::PagesMemo>,
    /// The styles of the text of margin boxes made so far, which the pages that share the
    /// styles of their margin boxes share too.
    pub(crate) margin_text_styles: RefCell<Vec<crate::page::MarginTextStyle>>,
    /// Per block, flex or grid container looked at so far, whether a box below it forces a
    /// page break and whether one forces a column break (see [`Layouter::forces_breaks_below`]).
    forced_below: RefCell<FxHashMap<BoxId, (bool, bool)>>,
    /// Per box looked at so far, whether an in-flow child of it has a percentage height (see
    /// [`Layouter::has_percentage_heights`]).
    pub(crate) percentage_heights: RefCell<FxHashMap<BoxId, bool>>,
    /// The layouts of containers inside the item being laid out (see [`crate::memo`]).
    pub(crate) item_memo: RefCell<crate::memo::ItemMemo>,
    /// The layouts of atomic inlines (see [`crate::memo`]).
    pub(crate) atomic_memo: RefCell<crate::memo::AtomicMemo>,
    /// Per block container looked at so far, whether a forced page break or a change of page
    /// name lies between the block-level boxes in it, and whether a forced column break (see
    /// [`Layouter::breaks_inside`]).
    pub(crate) breaks_below: RefCell<FxHashMap<BoxId, (bool, bool)>>,
}

/// The out-of-flow boxes of `tree` whose parent is right to left, and those whose containing
/// block is (that of a fixed box being the initial one unless a transform or containment makes
/// an ancestor its containing block).
fn rtl_static_positions(tree: &BoxTree) -> (rustc_hash::FxHashSet<BoxId>, rustc_hash::FxHashSet<BoxId>) {
    let mut found = rustc_hash::FxHashSet::default();
    let mut containers = rustc_hash::FxHashSet::default();
    let initial = tree.root.is_some_and(|root| tree.get(root).style.is_rtl());
    // Each box with whether the containing blocks of the absolutely positioned and the fixed
    // boxes inside are right to left.
    let mut stack: Vec<(BoxId, bool, bool)> = tree
        .root
        .into_iter()
        .chain(tree.fixed.iter().copied())
        .map(|id| (id, initial, initial))
        .collect();
    let mut seen = rustc_hash::FxHashSet::default();
    while let Some((id, absolute, fixed)) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let style = &tree.get(id).style;
        let rtl = style.is_rtl();
        let absolute = if style.contains_absolute() { rtl } else { absolute };
        let fixed = if style.has_transform() || style.contains_out_of_flow() {
            rtl
        } else {
            fixed
        };
        for child in tree.children(id) {
            let child_style = &tree.get(child).style;
            if child_style.is_out_of_flow() {
                if rtl {
                    found.insert(child);
                }
                let container = if child_style.position == fagus_style::values::Position::Fixed {
                    fixed
                } else {
                    absolute
                };
                if container {
                    containers.insert(child);
                }
            }
            stack.push((child, absolute, fixed));
        }
    }
    (found, containers)
}

impl<'a> Layouter<'a> {
    pub fn new(tree: &'a BoxTree, fonts: &'a FontDb) -> Self {
        let (static_rtl, rtl_containers) = rtl_static_positions(tree);
        Self {
            tree,
            fonts,
            intrinsic_cache: RefCell::default(),
            page_widths: RefCell::default(),
            content_cache: RefCell::default(),
            table_cache: RefCell::default(),
            collapsed_styles: RefCell::default(),
            table_fills: std::cell::Cell::new(false),
            table_grow: std::cell::Cell::new(Au::ZERO),
            table_unsized: std::cell::Cell::new(false),
            given_height: std::cell::Cell::new(false),
            grid_aligned: std::cell::Cell::new(false),
            page_number: std::cell::Cell::new((1, 0)),
            page_values: RefCell::default(),
            margin_page_value: std::cell::Cell::new(None),
            footnote_numbering: std::cell::Cell::new(None),
            footnote_area: RefCell::default(),
            column_footnotes: tree
                .footnotes
                .keys()
                .any(|&id| tree.get(id).style.float_reference == fagus_style::values::FloatReference::Column),
            string_position: std::cell::Cell::new(None),
            inline_containing_height: std::cell::Cell::new(None),
            intrinsic_height: std::cell::Cell::new(None),
            height_for_widths: std::cell::Cell::new(None),
            cell_basis: std::cell::Cell::new(None),
            beside_floats_basis: std::cell::Cell::new(None),
            cell_replaced_height: std::cell::Cell::new(None),
            restricted_cell: std::cell::Cell::new(false),
            flex_statics: RefCell::default(),
            block_statics: RefCell::default(),
            list_item_edges: std::cell::Cell::new(None),
            in_list_item: std::cell::Cell::new(false),
            unplaced_marker: std::cell::Cell::new(false),
            contexts: RefCell::default(),
            columns: std::cell::Cell::new(0),
            spanning: std::cell::Cell::new(None),
            below_spanner: std::cell::Cell::new(false),
            last_resort_column: std::cell::Cell::new(false),
            measuring: std::cell::Cell::new(false),
            after_forced_break: std::cell::Cell::new(false),
            forced_break: std::cell::Cell::new(false),
            lenient_top: std::cell::Cell::new(false),
            page_overflow: std::cell::Cell::new(Au::ZERO),
            ending_early: std::cell::Cell::new(false),
            early_slack: std::cell::Cell::new(Au::ZERO),
            blocks_placed: std::cell::Cell::new((0, 0)),
            table_frame_breaks: std::cell::Cell::new(false),
            last_line_alone: std::cell::Cell::new(false),
            content_sized: std::cell::Cell::new(false),
            indefinite_basis: std::cell::Cell::new(false),
            fixed_item_height: std::cell::Cell::new(false),
            column_height: std::cell::Cell::new(None),
            space_shortage: std::cell::Cell::new(None),
            shortage_taken: std::cell::Cell::new(0),
            has_out_of_flow: std::cell::OnceCell::new(),
            relative_later: std::cell::Cell::new(false),
            stacking_owners: RefCell::default(),
            multicol_container: std::cell::Cell::new(None),
            breaks_avoided_items: std::cell::Cell::new(false),
            poor_lines: std::cell::Cell::new(false),
            breaks_avoided_inside: std::cell::Cell::new(false),
            table_repeats: RefCell::default(),
            table_widths: RefCell::default(),
            line_budget: std::cell::Cell::new(None),
            clamp_follows: std::cell::Cell::new(false),
            subgrids: RefCell::default(),
            margin_above: std::cell::Cell::new(Au::ZERO),
            room_below: std::cell::Cell::new(Au::ZERO),
            trim_pending: std::cell::Cell::new((false, false)),
            fragmentainer_trim: std::cell::Cell::new((false, false)),
            frame: std::cell::Cell::new(
                tree.root
                    .map(|root| tree.get(root).style.writing_mode)
                    .unwrap_or_default(),
            ),
            // A Letter page until pagination sets the page area.
            initial_containing_block: std::cell::Cell::new((Au::from_px(816.0), Au::from_px(1056.0))),
            cross_room: std::cell::Cell::new(None),
            static_rtl,
            rtl_containers,
            inline_static_rtl: RefCell::default(),
            grid_tracks: RefCell::default(),
            han_fonts: crate::inline::spacing::FontCache::default(),
            shaped: crate::inline::ShapedCache::default(),
            nested_tables: std::cell::OnceCell::new(),
            grid_growth: RefCell::default(),
            unfragmented: std::cell::Cell::new(0),
            inline_table: std::cell::Cell::new(false),
            provisional_grids: std::cell::Cell::new(0),
            measured_items: std::cell::Cell::new(0),
            cell_memo: RefCell::default(),
            pages_memo: RefCell::default(),
            margin_text_styles: RefCell::default(),
            forced_below: RefCell::default(),
            percentage_heights: RefCell::default(),
            item_memo: RefCell::default(),
            atomic_memo: RefCell::default(),
            breaks_below: RefCell::default(),
        }
    }

    /// Lays out a block-level box. `None` means the box does not fit in the remaining space and
    /// has to move to the next page.
    pub fn block_level(&self, id: BoxId, space: Space, token: Option<&BreakToken>) -> Option<Outcome> {
        fagus_dom::budget::check_time();
        let style = &self.tree.get(id).style;
        let _whole = self.enter_box(style);
        // The lines of a box with a formatting context of its own do not count for the
        // `line-clamp` of a box around it (see `crate::clamp`).
        let _paused = crate::clamp::BudgetPause::of(&self.line_budget, style);
        // Boxes with size containment or `line-clamp` are monolithic, as in Chrome: they move to
        // the next page whole, like boxes that avoid breaks inside.
        let monolithic =
            (style.size_containment().0 && style.size_containment().1) || style.line_clamp.is_some();
        // Forced breaks inside a box that avoids breaks break it all the same, as in Chrome.
        if (self.avoids_breaks_inside(style.break_inside) && !self.forced_breaks_inside(id) || monolithic)
            && token.is_none()
            && space.remaining.is_some()
        {
            let floats = self.float_count();
            let outcome = self.block_level(
                id,
                Space {
                    remaining: None,
                    ..space
                },
                None,
            )?;
            let margin_top = self.vertical_margin(style, space.containing_width, true);
            // Content that overflows a box that avoids breaks inside it would break across pages
            // with it, which counts as a break inside the box, as in Chrome.
            let extent = if monolithic {
                outcome.fragment.rect.height
            } else {
                crate::parallel::flow_extent(&outcome.fragment)
            };
            let fits = space
                .remaining
                .is_none_or(|remaining| extent + margin_top <= remaining);
            let shortage = space
                .remaining
                .map_or(Au::ZERO, |remaining| extent + margin_top - remaining);
            if fits || monolithic {
                if !fits && !self.at_page_top(&space) {
                    self.note_shortage(shortage);
                    return None;
                }
                return Some(outcome);
            }
            // A box that avoids breaks inside but is too tall for the page moves to the next one,
            // and is broken there if it is still too tall, as in Chrome.
            if !space.at_top {
                self.note_shortage(shortage);
                return None;
            }
            self.truncate_floats(floats);
        }
        match &self.tree.get(id).kind {
            BoxKind::Table { .. } => self.table(id, space, token),
            // Replaced elements cannot break; they move to the next page if they do not fit.
            BoxKind::Replaced { .. } => {
                let containing_height = self.cell_replaced_height.get().or(space.containing_height);
                let mut outcome = self.replaced(id, space.containing_width, None, None, containing_height);
                // In a right-to-left containing block it keeps its right margin.
                let style = &self.tree.get(id).style;
                if space.rtl && !style.margin.left.is_auto() && !style.margin.right.is_auto() {
                    let right =
                        Au::from_px(style.margin.right.resolve_or(space.containing_width.to_px(), 0.0));
                    outcome.fragment.rect.x = space.containing_width - outcome.fragment.rect.width - right;
                }
                if let Some(remaining) = space.remaining
                    && outcome.fragment.rect.height + outcome.collapsed_top.resolve() > remaining
                    && !self.at_page_top(&space)
                {
                    self.note_shortage(
                        outcome.fragment.rect.height + outcome.collapsed_top.resolve() - remaining,
                    );
                    return None;
                }
                Some(outcome)
            }
            BoxKind::Flex { .. } | BoxKind::Grid { .. } => {
                // A block-level field or select shrinks to fit, as in Chrome.
                let shrink = self.tree.shrinks_to_fit.contains(&id);
                let width = self.sized_width(id, space.containing_width, space.containing_width, shrink);
                self.flex_or_grid(id, space, token, width)
            }
            _ if style.is_multicol() => {
                let width = self.sized_width(id, space.containing_width, space.containing_width, false);
                self.in_multicol(id, || self.multicol_with(id, space, token, width))
            }
            _ => self.block_container(id, space, token),
        }
    }

    /// Whether a forced break lies between the rows of the bodies of table `id`, or between
    /// block-level boxes in box `id` (see [`Layouter::breaks_inside`]).
    fn forced_breaks_inside(&self, id: BoxId) -> bool {
        let BoxKind::Table { bodies, .. } = &self.tree.get(id).kind else {
            return self.breaks_inside(id);
        };
        let rows: Vec<(BoxId, BoxId)> = bodies
            .iter()
            .flat_map(|&group| match &self.tree.get(group).kind {
                BoxKind::RowGroup { rows } => rows.iter().map(|&row| (row, group)).collect(),
                _ => Vec::new(),
            })
            .collect();
        rows.windows(2)
            .any(|pair| self.forces_break(self.tree.break_between_rows(pair[0], pair[1])))
    }

    /// Notes that content needed `shortage` more room than the page or column had left for it
    /// (see [`Layouter::space_shortage`]).
    pub(crate) fn note_shortage(&self, shortage: Au) {
        if shortage > Au::ZERO {
            let least = self
                .space_shortage
                .get()
                .map_or(shortage, |least| least.min(shortage));
            self.space_shortage.set(Some(least));
        }
    }

    /// Where an empty line of a block container with `style` whose content box starts at
    /// `origin` in the formatting context and is `width` wide puts its start: the static
    /// position of an absolutely positioned box that was inline-level among block-level boxes,
    /// which Chrome places in such a line (beside the floats, aligned by `text-align`). In
    /// context coordinates; right to left, the box ends there.
    fn inline_static_x(&self, style: &ComputedStyle, origin: (Au, Au), width: Au) -> Au {
        use fagus_style::values::TextAlign;
        let (left, right) = self.line_range(origin.0, origin.0 + width, origin.1, Au::EPSILON);
        let rtl = style.is_rtl();
        match style.text_align {
            TextAlign::Left => left,
            TextAlign::Right => right,
            TextAlign::Center => left + (right - left) / 2,
            TextAlign::End if !rtl => right,
            TextAlign::Start | TextAlign::Justify if rtl => right,
            TextAlign::Start | TextAlign::Justify | TextAlign::End => left,
        }
    }

    /// Notes the bidi level of the place of out-of-flow box `id` in a line: whether it is right
    /// to left. Only boxes that were inline-level take their direction from it.
    pub(crate) fn note_inline_static(&self, id: BoxId, rtl: bool) {
        if self.tree.get(id).style.originally_inline {
            self.inline_static_rtl.borrow_mut().insert(id, rtl);
        }
    }

    /// Whether a box in `space` starts at the very top of the page, where monolithic content
    /// that does not fit stays rather than moving to the next page (see [`Space::flush`]).
    pub(crate) fn at_page_top(&self, space: &Space) -> bool {
        space.at_top && (space.flush || self.lenient_top.get())
    }

    /// Whether a block-level box inside block, flex or grid container `id` forces a break before
    /// or after it.
    fn forces_breaks_below(&self, id: BoxId) -> bool {
        if self.measuring.get() {
            return false;
        }
        let (page, column) = self.forced_below(id);
        page || (column && self.columns.get() > 0)
    }

    /// Whether a block-level box inside block, flex or grid container `id` has a `break-before`
    /// or `break-after` that forces a page break, and whether one has `column`: found once per
    /// container, as containers broken across pages ask on every page.
    fn forced_below(&self, id: BoxId) -> (bool, bool) {
        if let Some(&found) = self.forced_below.borrow().get(&id) {
            return found;
        }
        let (BoxKind::Block { children } | BoxKind::Flex { children } | BoxKind::Grid { children }) =
            &self.tree.get(id).kind
        else {
            return (false, false);
        };
        let mut found = (false, false);
        for &child in children {
            for value in [self.tree.break_before(child), self.tree.break_after(child)] {
                found.0 |= value.is_forced_page_break();
                found.1 |= value == BreakBetween::Column;
            }
            let below = self.forced_below(child);
            found = (found.0 || below.0, found.1 || below.1);
        }
        self.forced_below.borrow_mut().insert(id, found);
        found
    }

    /// Whether a value of `break-before` or `break-after` asks not to break here.
    pub(crate) fn avoids_break(&self, value: BreakBetween) -> bool {
        match value {
            BreakBetween::Avoid => true,
            BreakBetween::AvoidPage => self.columns.get() == 0,
            BreakBetween::AvoidColumn => self.columns.get() > 0,
            _ => false,
        }
    }

    /// Whether a value of `break-inside` asks not to break inside the box here: `avoid-page`
    /// avoids breaks between pages only and `avoid-column` between columns only (inside a
    /// column, as in Blink, whose content is fragmented into columns there, not pages).
    pub(crate) fn avoids_breaks_inside(&self, value: fagus_style::values::BreakInside) -> bool {
        use fagus_style::values::BreakInside;
        if self.breaks_avoided_inside.get() {
            return false;
        }
        match value {
            BreakInside::Avoid => true,
            BreakInside::AvoidPage => self.columns.get() == 0,
            BreakInside::AvoidColumn => self.columns.get() > 0,
            BreakInside::Auto => false,
        }
    }

    /// Whether a value of `break-before` or `break-after` forces a break here: one between
    /// pages, or one between columns inside a column.
    pub(crate) fn forces_break(&self, value: BreakBetween) -> bool {
        !self.measuring.get()
            && (value.is_forced_page_break() || (value == BreakBetween::Column && self.columns.get() > 0))
    }

    /// Whether a table header or footer of rows `heights` apart by `spacing` is short enough to
    /// repeat on every page: no taller than a quarter of the page area, as in Chrome (Blink's
    /// `TableLayoutAlgorithm`). Inside columns, any height is.
    pub(crate) fn repeatable_height(&self, heights: impl Iterator<Item = Au>, spacing: Au) -> bool {
        let (count, total) = heights.fold((0, Au::ZERO), |(count, total), height| {
            (count + 1, total + height)
        });
        let height = total + spacing * (count - 1).max(0);
        // In columns, the fragmentainer is the column; where its height is not known yet (while
        // the columns are balanced), nothing repeats, as in Chrome.
        let fragmentainer = if self.columns.get() > 0 {
            match self.column_height.get() {
                Some(column) => column,
                None => return false,
            }
        } else {
            crate::writing::physical_size(self.initial_containing_block.get(), self.writing_mode()).1
        };
        height <= fragmentainer / 4
    }

    pub(crate) fn box_model(
        &self,
        style: &ComputedStyle,
        containing_width: Au,
        width_override: Option<Au>,
    ) -> BoxModel {
        let basis = containing_width.to_px();
        let resolve = |length: &LengthPercentage| Au::from_px(length.resolve(basis));
        // A table cell's percentage paddings refer to the width of its table's columns.
        let padding_basis = match self.cell_basis.get() {
            Some(width)
                if style.display
                    == fagus_style::values::Display::Internal(
                        fagus_style::values::InternalDisplay::TableCell,
                    ) =>
            {
                width.to_px()
            }
            _ => basis,
        };
        let pad = |length: &LengthPercentage| Au::from_px(length.resolve(padding_basis));
        let border = [
            Au::from_px(style.border_width.top),
            Au::from_px(style.border_width.right),
            Au::from_px(style.border_width.bottom),
            Au::from_px(style.border_width.left),
        ];
        let padding = [
            pad(&style.padding.top),
            pad(&style.padding.right),
            pad(&style.padding.bottom),
            pad(&style.padding.left),
        ];
        let margin_or_zero = |margin: &LengthPercentageAuto| match margin {
            LengthPercentageAuto::Auto => Au::ZERO,
            LengthPercentageAuto::Length(length) => resolve(length),
        };
        let mut margin = [
            margin_or_zero(&style.margin.top),
            margin_or_zero(&style.margin.right),
            margin_or_zero(&style.margin.bottom),
            margin_or_zero(&style.margin.left),
        ];
        let frame = border[LEFT] + border[RIGHT] + padding[LEFT] + padding[RIGHT];
        let to_content = |width: Au| {
            if style.box_sizing == BoxSizing::BorderBox {
                (width - frame).clamp_non_negative()
            } else {
                width
            }
        };
        let width_basis = match self.beside_floats_basis.get() {
            Some((address, width)) if address == std::ptr::from_ref(style) as usize => width.to_px(),
            _ => basis,
        };
        let specified = width_override.or_else(|| match (&style.width, &style.height, style.aspect_ratio) {
            (Size::Length(length), _, _) => Some(to_content(Au::from_px(length.resolve(width_basis)))),
            // The margin box fills the containing block, also where `auto` would shrink.
            (Size::Stretch, _, _) => {
                Some((containing_width - margin[LEFT] - margin[RIGHT] - frame).clamp_non_negative())
            }
            // An auto width follows a fixed height through the aspect ratio (CSS Sizing 4, §5); the
            // ratio applies to the box that `box-sizing` names, like the height.
            (Size::Auto, Size::Length(LengthPercentage::Px(height)), Some(ratio)) if ratio > 0.0 => {
                Some(to_content(Au::from_px(height * ratio)))
            }
            _ => None,
        });
        let mut content_width = specified.unwrap_or_else(|| {
            (containing_width - margin[LEFT] - margin[RIGHT] - frame).clamp_non_negative()
        });
        // min-width and max-width.
        let border_box = |width: Au| {
            if style.box_sizing == BoxSizing::BorderBox {
                (width - frame).clamp_non_negative()
            } else {
                width
            }
        };
        // A table cell given its column's width fills it whatever its `max-width` says, as in
        // Chrome (which caps only what the cell asks of the column).
        let cell_width = width_override.is_some()
            && style.display
                == fagus_style::values::Display::Internal(fagus_style::values::InternalDisplay::TableCell);
        if let MaxSize::Length(max) = &style.max_width
            && !cell_width
        {
            content_width = content_width.min(border_box(resolve(max)));
        }
        // A fixed maximum height limits an automatic width through the aspect ratio, as in
        // Chrome (CSS Sizing 4, §5.2: transferred size suggestions).
        let transferred = match (specified, &style.height, &style.max_height, style.aspect_ratio) {
            (None, Size::Auto, MaxSize::Length(LengthPercentage::Px(max)), Some(ratio)) if ratio > 0.0 => {
                Some(to_content(Au::from_px(max * ratio)))
            }
            _ => None,
        };
        if let Some(max) = transferred {
            content_width = content_width.min(max);
        }
        if let Size::Length(min) = &style.min_width {
            content_width = content_width.max(border_box(resolve(min)));
        }
        // Auto margins center or push the box when its width is known.
        if specified.is_some() || transferred.is_some() || !matches!(style.max_width, MaxSize::None) {
            let free = containing_width - content_width - frame - margin[LEFT] - margin[RIGHT];
            match (style.margin.left.is_auto(), style.margin.right.is_auto()) {
                (true, true) => {
                    margin[LEFT] = (free / 2).clamp_non_negative();
                    margin[RIGHT] = margin[LEFT];
                }
                (true, false) => margin[LEFT] = free.clamp_non_negative(),
                (false, true) => margin[RIGHT] = free.clamp_non_negative(),
                (false, false) => {}
            }
        }
        BoxModel {
            margin,
            border,
            padding,
            content_width,
        }
    }

    /// The box model of a block-level box in the normal flow of `space`: over-constrained in a
    /// right-to-left containing block, it keeps its right margin and its left margin takes the
    /// rest (CSS 2.1, §10.3.3).
    pub(crate) fn flow_model(
        &self,
        style: &ComputedStyle,
        space: &Space,
        width_override: Option<Au>,
    ) -> BoxModel {
        let mut model = self.box_model(style, space.containing_width, width_override);
        if space.rtl && !style.margin.left.is_auto() && !style.margin.right.is_auto() {
            let border_box = model.content_width
                + model.border[LEFT]
                + model.border[RIGHT]
                + model.padding[LEFT]
                + model.padding[RIGHT];
            model.margin[LEFT] = space.containing_width - border_box - model.margin[RIGHT];
        }
        model
    }

    /// In quirks mode, the border-box height that the root element or `body` fills: the height
    /// of its containing block less its margins (Quirks Mode, §3.6 and §3.7, as Blink's
    /// `CalculateDefaultBlockSize`), if it is in the normal flow with an automatic height.
    fn quirky_fill(&self, id: BoxId, model: &BoxModel, space: &Space) -> Option<Au> {
        let (root, body) = self.tree.quirky?;
        let layout_box = self.tree.get(id);
        let node = layout_box.node?;
        let style = &layout_box.style;
        if node != root && Some(node) != body
            || style.is_floating()
            || style.is_out_of_flow()
            || !matches!(style.height, Size::Auto)
            || !style.display.is_block_level()
        {
            return None;
        }
        let frame = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        let available = space.containing_height?;
        Some(
            (available - model.margin[TOP] - model.margin[BOTTOM])
                .clamp_non_negative()
                .max(frame),
        )
    }

    /// Whether a block container passes the height of its containing block on to the percentage
    /// heights inside it: in quirks mode, where its own height is automatic, unless it is a
    /// table cell or absolutely positioned (Quirks Mode, §3.5).
    pub(crate) fn passes_percentages_on(&self, style: &ComputedStyle) -> bool {
        self.tree.quirky.is_some()
            && matches!(style.height, Size::Auto)
            && !style.is_out_of_flow()
            && style.display
                != fagus_style::values::Display::Internal(fagus_style::values::InternalDisplay::TableCell)
    }

    /// The used height of a box with a specified `height`, or `content` for `auto`.
    pub(crate) fn used_height(
        style: &ComputedStyle,
        model: &BoxModel,
        content: Au,
        containing_height: Option<Au>,
    ) -> Au {
        let empty = Au::from_px(style.contain_intrinsic_size.1.unwrap_or(0.0));
        Self::used_height_contained(style, model, content, containing_height, empty)
    }

    /// [`Layouter::used_height`] of a box whose content is `empty` tall where size containment
    /// lets it count as empty: a grid keeps the rows of its template (see
    /// [`Layouter::contained_grid_height`]).
    pub(crate) fn used_height_contained(
        style: &ComputedStyle,
        model: &BoxModel,
        content: Au,
        containing_height: Option<Au>,
        empty: Au,
    ) -> Au {
        // A table cell is as tall as its content: its `height` sets the height of its row, in
        // which the content is then aligned (CSS 2.1, §17.5.3).
        if style.display
            == fagus_style::values::Display::Internal(fagus_style::values::InternalDisplay::TableCell)
        {
            return content;
        }
        let frame = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        // Size containment: the content counts as empty (CSS Containment 2, §3.1).
        let content = if style.size_containment().1 {
            empty
        } else {
            content
        };
        let to_content = |height: Au| {
            if style.box_sizing == BoxSizing::BorderBox {
                (height - frame).clamp_non_negative()
            } else {
                height
            }
        };
        // A length, or a percentage of a containing block height that does not depend on the
        // content; other percentages are `auto` (CSS 2.1, §10.5).
        let resolve = |length: &LengthPercentage| match length.fixed() {
            Some(px) => Some(to_content(Au::from_px(px))),
            None => containing_height
                .map(|containing| to_content(Au::from_px(length.resolve(containing.to_px())))),
        };
        let mut height = match &style.height {
            Size::Length(length) if let Some(height) = resolve(length) => height,
            // The margin box fills a containing block of a definite height.
            Size::Stretch if let Some(containing) = containing_height => {
                let margins = model.margin[TOP] + model.margin[BOTTOM];
                (containing - margins - frame).clamp_non_negative()
            }
            // An auto height follows the width through the aspect ratio, but never cuts the
            // content (the automatic minimum size, CSS Sizing 4, §5.1).
            _ => match style.aspect_ratio {
                Some(ratio) if ratio > 0.0 => {
                    // The ratio is of the content box for `auto && <ratio>`.
                    let preferred = if crate::ratio::ratio_sizing(style) == BoxSizing::BorderBox {
                        let frame_x = model.border[LEFT]
                            + model.border[RIGHT]
                            + model.padding[LEFT]
                            + model.padding[RIGHT];
                        to_content(Au::from_px((model.content_width + frame_x).to_px() / ratio))
                    } else {
                        Au::from_px(model.content_width.to_px() / ratio)
                    };
                    // A scroll container has no automatic minimum size, nor has a box with a
                    // `min-height` of its own.
                    if crate::sizing::is_scroll_container(style) || !matches!(style.min_height, Size::Auto) {
                        preferred
                    } else {
                        preferred.max(content)
                    }
                }
                _ => content,
            },
        };
        // `calc-size()` of the height its basis gives.
        height = crate::sizing::calculated(style, 1, height, frame, containing_height.unwrap_or(Au::ZERO));
        // The maximum first, so that the minimum wins over it (CSS 2.1, §10.7). Content-based
        // keywords are the content's height in the block axis (CSS Box Sizing 3, §3.1);
        // `stretch` fills a containing block of a definite height.
        let stretched = containing_height.map(|containing| {
            (containing - model.margin[TOP] - model.margin[BOTTOM] - frame).clamp_non_negative()
        });
        // Either through the calculations of a `calc-size()`, whose `auto` minimum is nothing.
        let calculated = |index: usize, size: Au| {
            crate::sizing::calculated(style, index, size, frame, containing_height.unwrap_or(Au::ZERO))
        };
        let max = match &style.max_height {
            MaxSize::Length(max) => resolve(max),
            MaxSize::MinContent | MaxSize::MaxContent | MaxSize::FitContent(_) => Some(content),
            MaxSize::Stretch => stretched,
            MaxSize::None => None,
        };
        if let Some(max) = max {
            height = height.min(calculated(5, max));
        }
        let min = match &style.min_height {
            Size::Length(min) => resolve(min),
            Size::MinContent | Size::MaxContent | Size::FitContent(_) => Some(content),
            Size::Stretch => stretched,
            Size::Auto => crate::sizing::calc_size(style, 3).map(|_| Au::ZERO),
        };
        if let Some(min) = min {
            height = height.max(calculated(3, min));
        }
        height
    }

    /// The content height of a box if it does not depend on its content: a fixed height, a
    /// percentage of a definite containing block height, or the border-box height the box is
    /// stretched to. Percentage heights of the box's children refer to it.
    pub(crate) fn definite_height(
        style: &ComputedStyle,
        model: &BoxModel,
        containing_height: Option<Au>,
        height_override: Option<Au>,
    ) -> Option<Au> {
        if let Some(stretched) = height_override {
            let frame = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
            return Some((stretched - frame).clamp_non_negative());
        }
        // A table cell's height is its row's, which its content's percentages refer to only
        // where the table lays it out again at that height.
        if style.display
            == fagus_style::values::Display::Internal(fagus_style::values::InternalDisplay::TableCell)
        {
            return None;
        }
        let definite = match &style.height {
            Size::Length(length) => length.fixed().is_some() || containing_height.is_some(),
            Size::Stretch => containing_height.is_some(),
            // An automatic height that follows the (definite) width through a preferred aspect
            // ratio is definite too (CSS Sizing 4, §5.1): flex and grid containers align their
            // items in it, as in Chrome.
            Size::Auto => style.aspect_ratio.is_some_and(|ratio| ratio > 0.0),
            _ => false,
        };
        definite.then(|| Self::used_height(style, model, Au::ZERO, containing_height))
    }

    fn block_container(&self, id: BoxId, space: Space, token: Option<&BreakToken>) -> Option<Outcome> {
        let style = &self.tree.get(id).style;
        let root = self.tree.root == Some(id) || establishes_formatting_context(style);
        self.block_container_with(id, space, token, None, None, root)
    }

    /// Lays out a block container, optionally with a fixed content width (flex and grid items,
    /// table cells) and a fixed border-box height (stretched items). `root` tells whether the box
    /// establishes a block formatting context, whose children's margins stay inside it.
    pub(crate) fn block_container_with(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        width_override: Option<Au>,
        height_override: Option<Au>,
        root: bool,
    ) -> Option<Outcome> {
        // Sizing keywords decide the width; a block-level button shrinks to fit, as in Chrome.
        let width_override = width_override.or_else(|| {
            let shrink = self.tree.shrinks_to_fit.contains(&id);
            let room = (space.containing_width, space.containing_width);
            self.sized_width_in(id, room, shrink, space.containing_height)
                .or_else(|| self.block_ratio_width(id, space.containing_width, space.containing_height))
        });
        // A box with `line-clamp` ends after as many lines of its content, its descendants' too;
        // the lines after them overflow it (see `crate::clamp`).
        let clamp = self.tree.get(id).style.line_clamp.map(|lines| lines as usize);
        let outer = clamp.map(|lines| {
            (
                self.line_budget.replace(Some(lines)),
                self.clamp_follows.replace(false),
            )
        });
        let sizes = Sizes {
            width: width_override,
            height: height_override,
            content: self.content_sized.replace(false),
            fixed: self.fixed_item_height.replace(false),
            indefinite: self.indefinite_basis.replace(false),
        };
        let layout = || {
            self.ending_on_page(space, true, |ending| {
                self.block_container_in(id, space, token, sizes, root, ending)
            })
        };
        let mut outcome = layout();
        if let (Some(lines), Some(laid)) = (clamp, &outcome)
            && crate::clamp::count_lines(&laid.fragment) > lines
        {
            // Content follows the last line shown: laid out again for its ellipsis.
            self.line_budget.set(Some(lines));
            self.clamp_follows.set(true);
            outcome = layout();
            // A box of a definite height keeps it, as in Chrome.
            let sized = match &self.tree.get(id).style.height {
                Size::Length(length) => length.fixed().is_some() || space.containing_height.is_some(),
                _ => false,
            };
            if let Some(laid) = &mut outcome
                && height_override.is_none()
                && !sized
            {
                crate::clamp::end_after_line(
                    self.tree.get(id),
                    &mut laid.fragment,
                    lines,
                    space.containing_width,
                );
            }
        }
        if let Some((budget, follows)) = outer {
            self.line_budget.set(budget);
            self.clamp_follows.set(follows);
        }
        if let Some(legend) = self.tree.fieldsets.get(&id)
            && let Some(outcome) = &mut outcome
        {
            crate::fieldset::cut_legend_gap(&mut outcome.fragment, *legend);
        }
        outcome
    }

    /// Lays out a box with `layout`, which tells where the box's content ends (`Err`) when its
    /// bottom border and padding do not fit below it on the page. The box breaks before the last
    /// of its content instead, which moves to the next page along with them. Without a
    /// breakpoint for that (the empty space of a box taller than its content is none), the box
    /// moves to the next page as a whole, or if it starts the page, they move on their own (CSS
    /// Fragmentation 3, §4.4).
    /// In block flow (`block_flow`), the last line of the content may then go on alone, whatever
    /// `orphans` asks for (see [`Layouter::last_line_alone`]).
    pub(crate) fn ending_on_page(
        &self,
        space: Space,
        block_flow: bool,
        layout: impl Fn(Ending) -> Option<Result<Outcome, Au>>,
    ) -> Option<Outcome> {
        let floats = self.float_count();
        let end = match layout(Ending::Retry)? {
            Ok(outcome) => return Some(outcome),
            Err(end) => end,
        };
        self.truncate_floats(floats);
        let outer = self.ending_early.replace(true);
        let (count, outer_start) = self.blocks_placed.get();
        self.blocks_placed.set((count, count));
        let alone = self
            .last_line_alone
            .replace(block_flow || self.last_line_alone.get());
        let earlier = layout(Ending::Before(end - Au::EPSILON));
        self.ending_early.set(outer);
        self.blocks_placed.set((self.blocks_placed.get().0, outer_start));
        self.last_line_alone.set(alone);
        let earlier = earlier?.ok()?;
        // A break before the first of the content leaves none of it on the page, which is no
        // breakpoint (only one between siblings is, as Chrome has it): the box moves as a whole,
        // or the border and padding go on alone.
        let before_first = matches!(
            earlier.token,
            Some(BreakToken::Block {
                index: 0,
                inner: None,
                ..
            })
        );
        if !before_first && !earlier.token.as_ref().is_some_and(BreakToken::splits_empty_space) {
            return Some(earlier);
        }
        if !space.at_top {
            return None;
        }
        self.truncate_floats(floats);
        layout(Ending::Alone)?.ok()
    }

    /// Lays out a block container. `Err` tells where its content ends when the box's bottom
    /// border and padding do not fit below it and `ending` asks for that.
    fn block_container_in(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        sizes: Sizes,
        root: bool,
        ending: Ending,
    ) -> Option<Result<Outcome, Au>> {
        let Sizes {
            width: width_override,
            height: height_override,
            content: content_sized,
            fixed: fixed_height,
            indefinite: indefinite_basis,
        } = sizes;
        if let Some(inner) = self.tree.flows.content_of(id) {
            return self
                .flow_root(id, inner, space, width_override, height_override)
                .map(Ok);
        }
        let layout_box = self.tree.get(id);
        // An item of a column flex container whose height follows its content, with a flex basis
        // of its content's height: its own height gives way to that, as Chrome lets it grow with
        // its content where it breaks (see `Layouter::content_basis_height`).
        let basis_style;
        let style = if height_override.is_none()
            && (indefinite_basis
                || content_sized
                    && matches!(
                        layout_box.style.flex_basis,
                        Size::MinContent | Size::MaxContent | Size::FitContent(_)
                    ))
            && matches!(layout_box.style.height, Size::Length(_))
        {
            basis_style = Arc::new(ComputedStyle::from(ComputedValues {
                height: Size::Auto,
                ..ComputedValues::clone(&layout_box.style)
            }));
            &basis_style
        } else {
            &layout_box.style
        };
        let model = self.flow_model(style, &space, width_override);
        let _cell_replaced = CellReplacedScope::open(self, style, &model, layout_box.node.is_none());
        let continuing = token.is_some();
        // A box that ended on a page before, whose overflowing content goes on: the content
        // continues at the top of the page, without the box.
        let (token, ended) = match token {
            Some(BreakToken::Overflow { inner }) => (Some(&**inner), true),
            _ => (token, false),
        };
        let margin_above = self.margin_above.replace(Au::ZERO);
        // `text-box-trim` of this box, or of an ancestor whose first or last line it holds.
        let trim = {
            let pending = self.trim_pending.replace((false, false));
            (
                (style.text_box_trim.start || pending.0) && !continuing,
                style.text_box_trim.end || pending.1,
            )
        };
        // A box that clones its decorations has its border and padding on every page it spans,
        // and its content ends above them (`box-decoration-break: clone`).
        let clones = style.clones_decorations() && !ended;
        let top = if continuing && !clones {
            Au::ZERO
        } else {
            model.border[TOP] + model.padding[TOP]
        };
        let bottom_frame = model.padding[BOTTOM] + model.border[BOTTOM];
        let x0 = model.border[LEFT] + model.padding[LEFT];
        let limit = space
            .remaining
            .map(|remaining| remaining - if continuing { Au::ZERO } else { model.margin[TOP] });
        // Where content may end on this page.
        let content_limit = match ending {
            Ending::Before(end) => limit.map(|limit| limit.min(end)),
            Ending::Retry | Ending::Alone => limit,
        }
        .map(|limit| if clones { limit - bottom_frame } else { limit });
        // Where the content of the child that goes on from the page before may end: the content
        // does not end earlier inside it for the box's bottom border and padding, as Chrome finds
        // no breakpoint inside a resumed child (Blink's `MovePastBreakpoint` leaves it out of
        // the early breaks): where it reaches them, they go on alone (see `ending_on_page`).
        let resumed_limit = limit.map(|limit| if clones { limit - bottom_frame } else { limit });
        // The room the boxes inside have below the end of the content, to the end of the page.
        let outer_slack = self.early_slack.get();
        let _slack = SlackScope::open(
            self,
            match (ending, limit) {
                (Ending::Before(end), Some(limit)) => (limit - end).clamp_non_negative(),
                _ => Au::ZERO,
            },
        );
        let definite = Self::definite_height(
            style,
            &space.stretch_model(&model),
            space.containing_height,
            height_override,
        );
        // In quirks mode the root and `body` fill the page area, and percentage heights inside
        // them refer to that; boxes of an automatic height pass the height of their containing
        // block on to percentages inside them (Quirks Mode, §3.5 to §3.7).
        let frame_y = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        let fill = if token.is_none() && height_override.is_none() {
            self.quirky_fill(id, &model, &space)
        } else {
            None
        };
        // A box of a fixed height that fits on the page keeps its content there even where that
        // does not fit (images, lines taller than the room left), as in Chrome: the content
        // overflows the box and the page.
        let holds_content = token.is_none()
            && height_override.is_none()
            && definite.is_some_and(|height| limit.is_some_and(|limit| top + height + bottom_frame <= limit));
        // An item laid out at its flexed height with an indefinite flex basis leaves percentages
        // inside indefinite (Blink's `is_initial_block_size_indefinite`).
        let percent_basis = definite
            .filter(|_| !(indefinite_basis && height_override.is_some()))
            .or_else(|| fill.map(|fill| (fill - frame_y).clamp_non_negative()))
            .or_else(|| {
                // Anonymous block boxes are ignored when resolving percentages (CSS 2.1,
                // §9.2.1.1): those of their content refer to the box around them.
                let anonymous = layout_box.node.is_none() && matches!(style.height, Size::Auto);
                space
                    .containing_height
                    .filter(|_| anonymous || self.passes_percentages_on(style))
            });
        // A table cell, which is as tall as its row, breaks after its content as it is (its
        // row decides where it breaks).
        let cell_height = height_override.filter(|_| {
            style.display
                == fagus_style::values::Display::Internal(fagus_style::values::InternalDisplay::TableCell)
        });
        let width = model.content_width
            + model.border[LEFT]
            + model.border[RIGHT]
            + model.padding[LEFT]
            + model.padding[RIGHT];
        // The rest of a box whose content was placed on earlier pages.
        if let Some(BreakToken::Rest { rest }) = token {
            // A box that clones its decorations has its border and padding around the rest too.
            let frames = if clones { top + bottom_frame } else { Au::ZERO };
            // At the top of a page or column without room for any of the rest inside the
            // repeated decorations, all of it stays there, overflowing, so that layout progresses.
            let placed = limit.map_or(*rest, |limit| {
                let room = limit - frames;
                if room <= Au::ZERO && clones && space.at_top {
                    *rest
                } else {
                    (*rest).min(room.max(Au::ZERO))
                }
            });
            let broken = placed < *rest;
            let height = placed + frames;
            return Some(Ok(Outcome::new(
                Fragment {
                    rect: Rect::new(model.margin[LEFT], Au::ZERO, width, height),
                    kind: layout_box.fragment_kind(VisibleSides {
                        top: clones,
                        bottom: !broken || clones,
                        ..VisibleSides::ALL
                    }),
                    children: Children::new(),
                },
                broken.then(|| BreakToken::Rest { rest: *rest - placed }),
                Au::ZERO,
                if broken { Au::ZERO } else { model.margin[BOTTOM] },
            )));
        }
        let mut children: Vec<Fragment> = Vec::new();
        let mut y = top;
        let mut next_token = None;
        // Margins collapse through the top and bottom edges without border, padding or a new
        // formatting context (CSS 2.1 §8.3.1).
        let top_adjoins = !root && !continuing && top == Au::ZERO;
        // `stretch` heights of the children leave out their margins at edges without border or
        // padding, whatever this box's height, as in Chrome.
        let stretch_skips_margins = (
            !root && model.border[TOP] + model.padding[TOP] == Au::ZERO,
            !root && bottom_frame == Au::ZERO,
        );
        // With a minimum height too: where that makes the box taller than its content, the
        // last child's margin stays inside it (see `raised` below).
        // A height that behaves as `auto` (a content-based keyword, a percentage or `stretch` of
        // an indefinite height) lets the margins collapse as `auto` does (CSS Box Sizing 3, §3.1).
        let auto_height = match &style.height {
            Size::Auto | Size::MinContent | Size::MaxContent | Size::FitContent(_) => true,
            Size::Length(length) => length.fixed().is_none() && space.containing_height.is_none(),
            Size::Stretch => space.containing_height.is_none(),
        };
        let bottom_adjoins = !root && bottom_frame == Au::ZERO && auto_height;
        let mut collapsed_top = Margin::of(if continuing { Au::ZERO } else { model.margin[TOP] });
        // The margin below the last child placed so far, not yet added to `y`.
        let mut previous = Margin::default();
        // Whether `previous` holds the margins of an empty box with clearance, which do not
        // collapse with our bottom margin (CSS 2.1 §8.3.1).
        let mut previous_cleared = false;
        // Whether nothing but empty boxes has been placed yet.
        let mut leading = true;
        // Where the empty boxes placed after the last box that is not empty are among the
        // children, which `margin-trim: block-end` moves up to it.
        let mut trailing_through: Vec<usize> = Vec::new();
        // Where the border box lies in its block formatting context; a box that establishes one is
        // its origin.
        let _context = root.then(|| Context::open(self));
        let (bfc_x, bfc_y) = if root {
            (Au::ZERO, Au::ZERO)
        } else {
            (space.bfc_left + model.margin[LEFT], space.bfc_top)
        };
        let _list_item = ListItemScope::open(self, style, (bfc_x, bfc_x + width), root);
        // Whether this is a list item whose children may yet move down below its outside marker.
        let mut marker_pending = !continuing
            && !style.is_vertical()
            && matches!(
                style.display,
                fagus_style::values::Display::Normal { list_item: true, .. }
            );
        // Floats in the content break where it has to end on the page; one at the very top of
        // the page (nothing, not even a margin, above it there) stays even if it does not fit.
        let _limit = FlowScope::open(
            self,
            content_limit.map(|limit| FlowLimit {
                limit: bfc_y + limit,
                top: self.at_page_top(&space).then_some(bfc_y + top),
            }),
        );
        // The floats that continue from the page before come first, at the top.
        if root && let Some(token) = token {
            children.extend(self.resume_floats(token.floats(), top, space.at_top));
        }
        let resumed_break = root && self.floats_continue();
        // Where the inline content ends, if all of it is laid out on the page.
        let mut lines_end = (usize::MAX, 0);
        // The breaks that layout finds the first and last children pass on.
        let mut passed_breaks = (BreakBetween::Auto, BreakBetween::Auto);
        // The children whose boxes end on this page while their content goes on.
        let mut overflow: Vec<(usize, BreakToken)> = Vec::new();

        match &layout_box.kind {
            BoxKind::Inline { items } => {
                let start = match token {
                    Some(BreakToken::Lines { piece, offset, .. }) => (*piece, *offset),
                    _ => (0, 0),
                };
                let floats_before = self.float_count();
                let flow = (bfc_x + x0, bfc_y + top);
                let first_line = self.tree.first_line(id).map(|style| &**style);
                // Emphasis marks and ruby annotations may reach into the top margin of a box
                // without a top border, and into its padding.
                let room = if start == (0, 0) {
                    let margin = if model.border[TOP] == Au::ZERO && !root {
                        margin_above.max(Au::ZERO)
                    } else {
                        Au::ZERO
                    };
                    (margin + model.padding[TOP]).to_px()
                } else {
                    0.0
                };
                // In a column, the first line of a paragraph resumed from the column before is
                // trimmed too, unless the box trims its own lines (see `fragmentainer_trim`).
                let column_trim = self.fragmentainer_trim.get();
                let column_trim = (
                    column_trim.0 && !style.text_box_trim.start,
                    column_trim.1 && !style.text_box_trim.end,
                );
                let trims_first = if start == (0, 0) { trim.0 } else { column_trim.0 };
                let spacer = crate::annotation::Spacer::new(room, style.font_size, trims_first);
                let outer_height = self.inline_containing_height.replace(percent_basis);
                let mut lines = lines_in_flow(
                    self,
                    (style, first_line),
                    (items, Some(id)),
                    model.content_width,
                    (start, Some(spacer)),
                    flow,
                );
                self.inline_containing_height.set(outer_height);
                let room_below = crate::annotation::end_lines(
                    self.fonts,
                    &mut lines,
                    style.font_size,
                    model.padding[BOTTOM].to_px(),
                );
                self.room_below.set(Au::from_px(room_below.unwrap_or_default()));
                crate::text_box::trim_lines(
                    self.fonts,
                    (style, first_line.filter(|_| start == (0, 0))),
                    &mut lines,
                    (trim.0 && start == (0, 0), trim.1),
                );
                if column_trim.0 && start != (0, 0) {
                    crate::text_box::trim_lines(self.fonts, (style, None), &mut lines, (true, false));
                }
                if let Some(remaining) = self.line_budget.get() {
                    self.line_budget.set(Some(remaining.saturating_sub(lines.len())));
                }
                // The lines that fit on the page, with the floats placed beside them.
                let mut end = 0;
                // A formatting context whose floats break breaks with them, and its bottom
                // border and padding need no room on this page.
                let mut floats_break = resumed_break;
                // Whether only the bottom border and padding keep the last line from fitting.
                let mut cut_by_frame = false;
                for index in 0..lines.len() {
                    let line = &lines[index];
                    let floats_bottom = top + line.floats_bottom;
                    floats_break |= root && content_limit.is_some_and(|limit| floats_bottom >= limit);
                    // Below the last line the box ends with its padding and border, unless it is
                    // taller than its content, sized or stretched (its padding and border then break
                    // onto the next page, as in Chrome); at a break they are sliced off
                    // (`box-decoration-break: slice`), unless they are cloned, and the content
                    // ends above them anyway.
                    let taller = cell_height.is_some()
                        || definite
                            .is_some_and(|height| line.top + line.height + line.clearance_after < height);
                    let frame = if end + 1 == lines.len() && !clones && !floats_break && !taller {
                        bottom_frame
                    } else {
                        Au::ZERO
                    };
                    // Only a formatting context holds its floats above its bottom border.
                    let bottom = top + line.top + line.height + line.clearance_after + line.annotations_under;
                    let without_frame = bottom.max(floats_bottom);
                    let bottom = if root {
                        bottom.max(floats_bottom) + frame
                    } else {
                        (bottom + frame).max(floats_bottom)
                    };
                    // A first line at the top of the page stays even if it does not fit, unless
                    // something lies above it there (other than its annotations), as in Chrome
                    // (see `Space::flush`; WPT css-break ruby-003).
                    if let Some(limit) = content_limit
                        && bottom > limit
                        && (end > 0
                            || !self.at_page_top(&space)
                            || top + line.top - line.annotations_over > Au::ZERO && !continuing)
                    {
                        let cut = if column_trim.1 {
                            crate::text_box::end_trim(self.fonts, style, line)
                        } else {
                            Au::ZERO
                        };
                        if cut > Au::ZERO && bottom - cut <= limit {
                            // It fits at the end of the column only trimmed, as in Blink.
                            lines[index].height -= cut;
                            end += 1;
                            break;
                        }
                        self.note_shortage(bottom - limit);
                        cut_by_frame = frame > Au::ZERO && without_frame <= limit;
                        break;
                    }
                    end += 1;
                }

                if end < lines.len() {
                    // Orphans and widows (CSS Fragmentation 3, §3.4).
                    let widows = style.widows as usize;
                    let orphans = style.orphans as usize;
                    let first = start == (0, 0);
                    let resume = start;
                    // Lines without content and height (an empty inline after a forced break)
                    // are no widows, as in Chrome.
                    let after = lines[end..].iter().filter(|line| line.height > Au::ZERO).count();
                    // Where only the box's own bottom border and padding push the last line to
                    // the next page, the box breaks after the last line that leaves as many
                    // lines before and after it as `orphans` and `widows` ask for, or if none
                    // does, before the last line, which goes alone, as in Chrome; where those of
                    // a block around them push it, lines move along for the widows, but the
                    // orphans may be fewer than asked for.
                    let own_frame = end + 1 == lines.len() && cut_by_frame;
                    let last_alone = own_frame || end + 1 == lines.len() && self.last_line_alone.get();
                    if own_frame
                        && let Some(kept) = (1..=end)
                            .rev()
                            .find(|&kept| (!first || kept >= orphans) && lines.len() - kept >= widows)
                    {
                        end = kept;
                    }
                    if !own_frame && after < widows && end > widows - after {
                        // Lines move to the next page for the widows as far as the orphans
                        // allow, those of the part of the paragraph on this page, as in Chrome
                        // (also where the paragraph started on a page before).
                        end = (end - (widows - after)).max(orphans).min(end);
                    } else if !own_frame && !first && after < widows && !space.at_top {
                        // The rest of a paragraph too short to leave the widows a break needs
                        // moves on whole, as in Chrome (in columns being balanced, they grow).
                        end = 0;
                    }
                    // Where the content of a box around has to end before its bottom border and
                    // padding, a break that leaves fewer lines than `orphans` or `widows` ask
                    // for is a worse one than a break between boxes before the paragraph inside
                    // that box, where there is one, as in Chrome.
                    let (count, start) = self.blocks_placed.get();
                    let violates = (first && end < orphans) || lines.len() - end < widows;
                    let box_before = self.ending_early.get() && count > start && !own_frame;
                    // The first column of a row below a spanner takes such a break where it
                    // has no better one (see `Layouter::last_resort_column`).
                    let last_resort = (self.last_resort_column.get() || self.poor_lines.get()) && end > 0;
                    if (first && end < orphans && !last_alone || end == 0 || violates && box_before)
                        && !space.at_top
                        && !last_resort
                    {
                        self.truncate_floats(floats_before);
                        return None;
                    }
                    if end == 0 {
                        // The first line moves to the next page, below the top border and
                        // padding of the box, which stay here.
                        self.truncate_floats(floats_before);
                        next_token = Some(BreakToken::Lines {
                            piece: resume.0,
                            offset: resume.1,
                            consumed: Consumed::default(),
                            floats: Vec::new(),
                        });
                    } else if end < lines.len() {
                        let (piece, offset) = lines[end - 1].end;
                        next_token = Some(BreakToken::Lines {
                            piece,
                            offset,
                            consumed: Consumed::default(),
                            floats: Vec::new(),
                        });
                        // The floats of the lines that move on are placed again on the next page.
                        self.truncate_floats(lines[end - 1].float_count);
                    }
                }
                for line in lines.into_iter().take(end) {
                    lines_end = line.end;
                    let mut fragment = line.fragment;
                    fragment.rect.x += x0;
                    fragment.rect.y = top + line.top;
                    y = top + line.top + line.height + line.clearance_after;
                    // Absolutely positioned boxes that were block-level start at the start edge
                    // of the line, whatever its `text-align`; where they follow content, they
                    // start where the next line would, as in Chrome.
                    let start = if style.is_rtl() {
                        x0 + model.content_width
                    } else {
                        x0
                    };
                    let (moved, kept) = self.block_level_statics(&mut fragment, start, style.is_rtl());
                    for id in kept {
                        self.note_block_static(style, id, model.content_width);
                    }
                    for id in moved {
                        self.note_block_static(style, id, model.content_width);
                        children.push(placeholder(id, start, y));
                    }
                    leading &= line.phantom;
                    children.push(fragment);
                    for mut float in line.floats {
                        float.rect.x += x0;
                        float.rect.y += top;
                        children.push(float);
                    }
                }
            }
            BoxKind::Block { children: boxes } => {
                let (start, inner, overflowing) = match token {
                    Some(BreakToken::Block {
                        index,
                        inner,
                        overflow,
                        ..
                    }) => (*index, inner.as_deref(), overflow.as_slice()),
                    _ => (0, None, &[][..]),
                };
                // The content of children that ended on a page before and overflows them comes
                // first, at the top, in parallel to the content that follows them, as in Chrome.
                for (index, child_token) in overflowing {
                    let Some(&child) = boxes.get(*index) else {
                        continue;
                    };
                    let child_space = Space {
                        containing_width: model.content_width,
                        containing_height: percent_basis,
                        remaining: content_limit.map(|limit| limit - top),
                        at_top: true,
                        flush: true,
                        bfc_left: bfc_x + x0,
                        bfc_top: bfc_y + top,
                        rtl: style.is_rtl(),
                        stretch_skips_margins,
                    };
                    if let Some(outcome) = self.block_level(child, child_space, Some(child_token)) {
                        let mut fragment = outcome.fragment;
                        fragment.rect.x += x0;
                        fragment.rect.y = top;
                        children.push(fragment);
                        overflow.extend(outcome.token.map(|rest| (*index, rest)));
                    }
                }
                // Whether a child has been placed on this page.
                let mut placed = false;
                // The in-flow child placed last on this page.
                let mut last_child: Option<BoxId> = None;
                // The room under the last line of the child before, for the annotations of the next.
                let mut room_below = Au::ZERO;
                // The first and last in-flow block containers, which hold the lines to trim
                // (found from either end: all children were walked for each page).
                let container = |child: &BoxId| {
                    let child_box = self.tree.get(*child);
                    !child_box.style.is_out_of_flow()
                        && !child_box.style.is_floating()
                        && matches!(child_box.kind, BoxKind::Block { .. } | BoxKind::Inline { .. })
                };
                let first_container = boxes.iter().position(container);
                let last_container = boxes.iter().rposition(container);
                // Not past padding or a border between this box and the line (CSS Inline 3,
                // §5.1: then there is no effect).
                let framed = |index: usize, start: bool| {
                    let child_style = &self.tree.get(boxes[index]).style;
                    let (border, padding) = if start {
                        (child_style.border_width.top, &child_style.padding.top)
                    } else {
                        (child_style.border_width.bottom, &child_style.padding.bottom)
                    };
                    border > 0.0 || padding.resolve(model.content_width.to_px()) != 0.0
                };
                let trimmed = (
                    first_container.filter(|&index| trim.0 && !framed(index, true)),
                    last_container.filter(|&index| trim.1 && !framed(index, false)),
                );
                // The state before each in-flow child placed on this page, to move children to
                // the next page again when the break would separate boxes that avoid it.
                let mut history: Vec<Placement> = Vec::new();
                for (index, &child) in boxes.iter().enumerate().skip(start) {
                    let child_box = self.tree.get(child);
                    let child_style = &child_box.style;
                    if child_style.is_out_of_flow() {
                        // The static position: where the box would start if it were in flow, on
                        // the right in right-to-left text. One that was inline-level starts
                        // where a line there would put it, beside the floats and as aligned.
                        let static_y = y + previous.resolve();
                        let start = if child_style.originally_inline {
                            self.inline_static_x(style, (bfc_x + x0, bfc_y + static_y), model.content_width)
                                - bfc_x
                        } else if style.is_rtl() {
                            x0 + model.content_width
                        } else {
                            x0
                        };
                        if !child_style.originally_inline {
                            self.note_block_static(style, child, model.content_width);
                        }
                        children.push(placeholder(child, start, static_y));
                        continue;
                    }
                    if child_style.is_floating() {
                        // A float among blocks starts where the next block would. One that
                        // does not fit breaks, or moves to the next page while the content
                        // goes on beside it.
                        // Its percentage heights refer to the height of this box, where that
                        // is definite, as in the lines of the box.
                        let outer_height = self.inline_containing_height.replace(percent_basis);
                        let float = self.atomic(child, model.content_width);
                        let float_top = bfc_y + y + previous.resolve();
                        let float_top = self
                            .clearance(child_style.clear)
                            .map_or(float_top, |clear| clear.max(float_top));
                        let placed_float = self.place_float_box(
                            child,
                            (float.fragment, (float.width, float.height)),
                            child_style.float,
                            float_top,
                            (bfc_x + x0, bfc_x + x0 + model.content_width),
                        );
                        self.inline_containing_height.set(outer_height);
                        if let Some((rect, mut fragment)) = placed_float {
                            fragment.rect.x += rect.x - bfc_x;
                            fragment.rect.y += rect.y - bfc_y;
                            children.push(fragment);
                            placed = true;
                        }
                        continue;
                    }
                    let child_token = if index == start { inner } else { None };
                    // A spanner ends the row of columns it is in: the multi-column container
                    // places it below them, across all of them (CSS Multi-column Layout 1, §6).
                    if self
                        .spanning
                        .get()
                        .is_some_and(|flow| self.tree.spanned_flow(child) == Some(flow))
                    {
                        next_token = Some(BreakToken::at(index, child_token.cloned().map(Box::new)));
                        // Unlike margins at a break, the margin above a spanner stays in the
                        // columns, when the spanner is a child of their content box.
                        if self.spanning.get() == Some(id) {
                            y += previous.resolve();
                            previous = Margin::default();
                            previous_cleared = false;
                        }
                        break;
                    }
                    // A different page name forces a break (CSS Paged Media 3, §9.2).
                    let page_changes = last_child.is_some_and(|last| self.page_name_changes(last, child));
                    // Breaks fall between in-flow siblings: absolutely positioned boxes before a
                    // child do not separate it from the start of the page (Chrome ignores a forced
                    // break before the first in-flow box of a document after a fixed box).
                    let after_sibling = boxes[start..index]
                        .iter()
                        .any(|&before| !self.tree.get(before).style.is_out_of_flow());
                    if after_sibling
                        && (self.forces_break(self.tree.break_before(child)) || page_changes)
                        && limit.is_some()
                    {
                        next_token = Some(BreakToken::at(index, None));
                        self.forced_break.set(true);
                        break;
                    }
                    // Margins at the top of a page after an unforced break are truncated.
                    let truncated = child_token.is_some()
                        || (continuing
                            && !placed
                            && !self.below_spanner.get()
                            && !self.after_forced_break.get());
                    let own_margin_top = Au::from_px(match &child_style.margin.top {
                        LengthPercentageAuto::Length(length) => length.resolve(model.content_width.to_px()),
                        LengthPercentageAuto::Auto => 0.0,
                    });
                    // Where the border box starts, with the margins of its first descendants
                    // that collapse with its own.
                    let border_top = y + if leading && top_adjoins {
                        Au::ZERO
                    } else if truncated {
                        previous.resolve()
                    } else {
                        previous
                            .adjoin(self.leading_margin(child, model.content_width, (bfc_y + y, previous)))
                            .resolve()
                    };
                    // Clearance moves the border box below the floats `clear` names; boxes that
                    // must not overlap floats (new formatting contexts, tables, images) go beside
                    // them, or below them if there is no room.
                    let mut cleared = self
                        .clearance(child_style.clear)
                        .map(|bottom| bottom - bfc_y)
                        .filter(|&bottom| bottom > border_top);
                    let avoids_floats = self.has_floats()
                        && (establishes_formatting_context(child_style)
                            || self.tree.flows.content_of(child).is_some()
                            || matches!(
                                child_box.kind,
                                BoxKind::Table { .. }
                                    | BoxKind::Flex { .. }
                                    | BoxKind::Grid { .. }
                                    | BoxKind::Replaced { .. }
                            ));
                    let (mut shift, mut child_width, retry) = if avoids_floats {
                        // A box of a fixed height must have the room all down its border box,
                        // as in Chrome; the height of others is not known yet.
                        let fixed_height = match &child_style.height {
                            Size::Length(length) => length.fixed().map(|height| {
                                let child_model = self.box_model(child_style, model.content_width, None);
                                let frame = child_model.border[TOP]
                                    + child_model.border[BOTTOM]
                                    + child_model.padding[TOP]
                                    + child_model.padding[BOTTOM];
                                match child_style.box_sizing {
                                    BoxSizing::BorderBox => Au::from_px(height).max(frame),
                                    BoxSizing::ContentBox => Au::from_px(height.max(0.0)) + frame,
                                }
                            }),
                            _ => None,
                        };
                        let mut height = fixed_height.unwrap_or(Au::EPSILON).max(Au::EPSILON);
                        let left_edge = bfc_x + x0;
                        // It goes below the initial letters before it too, as in Blink
                        // (`ClearanceOffsetIncludingInitialLetter`).
                        let start = self
                            .letter_clearance(fagus_style::values::Clear::Both)
                            .map_or(bfc_y + cleared.unwrap_or(border_top), |bottom| {
                                bottom.max(bfc_y + cleared.unwrap_or(border_top))
                            });
                        let mut placed = self.beside_floats(
                            child,
                            model.content_width,
                            (left_edge, start),
                            height,
                            style.is_rtl(),
                        );
                        // A table must have the room all down its border box too, as in Chrome
                        // (Blink lays it out in a layout opportunity and moves on to the next
                        // where it is taller): it is laid out in the room found to know its
                        // height, and placed again over that.
                        if fixed_height.is_none() && matches!(child_box.kind, BoxKind::Table { .. }) {
                            for _ in 0..3 {
                                let table_height = self.table_height(child, placed.2);
                                if table_height <= height {
                                    break;
                                }
                                height = table_height;
                                placed = self.beside_floats(
                                    child,
                                    model.content_width,
                                    (left_edge, start),
                                    height,
                                    style.is_rtl(),
                                );
                            }
                        }
                        let (top_edge, shift, child_width) = placed;
                        if top_edge - bfc_y > cleared.unwrap_or(border_top) {
                            cleared = Some(top_edge - bfc_y);
                        }
                        // Where its height is not known yet, the box may turn out too tall
                        // for the room found (see below).
                        let retry = (fixed_height.is_none()
                            && !matches!(child_box.kind, BoxKind::Table { .. }))
                        .then_some((left_edge, start, placed));
                        (shift, child_width, retry)
                    } else {
                        (Au::ZERO, model.content_width, None)
                    };
                    // While a table cell of a fixed height, or in a table of a specified one, is
                    // measured (not yet given its row's height), a scroll container in it with
                    // a percentage height counts as having none: its content scrolls in
                    // whatever height the row then gives it, as in Chrome (Blink's
                    // `IsRestrictedBlockSizeTableCellChild`, CSS Tables 3, §3.10.5).
                    let measured_scroller = percent_basis.is_none()
                        && height_override.is_none()
                        && style.display
                            == fagus_style::values::Display::Internal(
                                fagus_style::values::InternalDisplay::TableCell,
                            )
                        && (self.restricted_cell.get()
                            || matches!(&style.height, Size::Length(length) if length.fixed().is_some()))
                        && crate::sizing::is_scroll_container(child_style)
                        && matches!(&child_style.height, Size::Length(length) if length.fixed().is_none())
                        && !matches!(&child_style.min_height, Size::Length(length) if length.fixed().is_none())
                        && !matches!(&child_style.max_height, MaxSize::Length(length) if length.fixed().is_none());
                    let mut child_space = Space {
                        containing_width: child_width,
                        containing_height: percent_basis.or(measured_scroller.then_some(Au::ZERO)),
                        // What is left below the child's border box, and the top margin it takes
                        // off itself unless it continues (even where it is truncated).
                        remaining: if child_token.is_some() {
                            resumed_limit
                        } else {
                            content_limit
                        }
                        .map(|limit| {
                            limit - cleared.unwrap_or(border_top)
                                + if child_token.is_some() {
                                    Au::ZERO
                                } else {
                                    own_margin_top
                                }
                        }),
                        // Empty boxes before a child at the very top of the page leave it there,
                        // as in Chrome (where a break before it would make no progress).
                        at_top: (space.at_top || holds_content) && !placed
                            || self.at_page_top(&space) && cleared.unwrap_or(border_top) == Au::ZERO,
                        // A box of a fixed height that fits holds its first child as if at the
                        // top, and so does one that repeats its top border and padding on every
                        // page, which could otherwise not progress (also where it starts at the
                        // top of the page, as in Chrome).
                        flush: holds_content
                            || clones && (continuing || space.flush)
                            || space.flush && cleared.unwrap_or(border_top) == Au::ZERO,
                        bfc_left: bfc_x + x0 + shift,
                        bfc_top: bfc_y + cleared.unwrap_or(border_top),
                        rtl: style.is_rtl(),
                        stretch_skips_margins,
                    };
                    let count = self.float_count();
                    let before = Placement {
                        index,
                        children: children.len(),
                        y,
                        previous,
                        leading,
                        collapsed_top,
                        floats: count,
                        placed,
                        avoid_before: self.avoids_break(self.tree.break_before(child)),
                        avoid_after: self.avoids_break(self.tree.break_after(child)),
                        space: content_limit
                            .filter(|_| child_token.is_none())
                            .map(|limit| (child_space, limit)),
                    };
                    self.margin_above.set(
                        if leading && top_adjoins {
                            margin_above.max(model.margin[TOP])
                        } else {
                            border_top - y
                        } + room_below,
                    );
                    self.room_below.set(Au::ZERO);
                    self.trim_pending.set((
                        trimmed.0 == Some(index) && child_token.is_none(),
                        trimmed.1 == Some(index),
                    ));
                    // The child that goes on from the page before has all of the room, as the
                    // box around has (see `resumed_limit`).
                    let _resumed = child_token.map(|_| SlackScope::set(self, outer_slack));
                    // A box that starts below the end of the page (moved there by clearance or its
                    // margin) breaks before it, as in Chrome, even if it takes no room; so does a
                    // box that clears a float going on to the next page (a clearfix after floats
                    // that break), also at the top of the page, where the float makes progress.
                    // So does one whose margin takes it past the end from the top of the page:
                    // its margin is truncated on the next page, where it starts.
                    let past_end = child_token.is_none()
                        && content_limit.is_some_and(|limit| {
                            !child_space.at_top && cleared.unwrap_or(border_top) > limit
                                || self.clears_past_page(child_style.clear)
                                || child_space.at_top
                                    && !child_space.flush
                                    && !truncated
                                    && cleared.is_none()
                                    && border_top > limit
                        });
                    // Beside floats, a percentage width still refers to the containing block.
                    let beside = (avoids_floats && child_width != model.content_width)
                        .then_some((Arc::as_ptr(&child_box.style) as usize, model.content_width));
                    let outer_basis = self.beside_floats_basis.replace(beside);
                    let outcome = if past_end {
                        None
                    } else {
                        self.block_level(child, child_space, child_token)
                    };
                    // Where every break before the child is avoided back to the top of the page
                    // (and the box cannot move on as a whole), a break inside it that leaves
                    // fewer lines than `orphans` and `widows` ask for is the better one, as in
                    // Chrome (`kBreakAppealViolatingOrphansAndWidows` ranks above
                    // `kBreakAppealViolatingBreakAvoid`).
                    let outcome = match outcome {
                        None if !past_end
                            && (space.at_top || self.last_resort_column.get())
                            && only_avoided_breaks(&before, &history) =>
                        {
                            self.truncate_floats(count);
                            let outer = self.poor_lines.replace(true);
                            let mut retried = self.block_level(child, child_space, child_token);
                            // Or, where that does not place it either, a break inside the boxes
                            // in it that avoid breaks inside, which is as poor as the break
                            // before it and later (see `Layouter::breaks_avoided_inside`).
                            if retried.is_none() {
                                self.truncate_floats(count);
                                let inside = self.breaks_avoided_inside.replace(true);
                                retried = self.block_level(child, child_space, child_token);
                                self.breaks_avoided_inside.set(inside);
                            }
                            self.poor_lines.set(outer);
                            retried
                        }
                        outcome => outcome,
                    };
                    // A box that must not overlap floats, laid out in the room beside them, may
                    // be taller than that room: it goes where there is room for its height and
                    // is laid out again there, as Blink tries the layout opportunities in turn
                    // until the box fits one (`LayoutNewFormattingContext`).
                    let outcome = match (outcome, retry) {
                        (Some(mut outcome), Some((left_edge, start, mut placed)))
                            if child_token.is_none() && outcome.token.is_none() =>
                        {
                            let mut laid = Some(());
                            for _ in 0..3 {
                                let height = outcome.fragment.rect.height.max(Au::EPSILON);
                                let again = self.beside_floats(
                                    child,
                                    model.content_width,
                                    (left_edge, start),
                                    height,
                                    style.is_rtl(),
                                );
                                if again == placed {
                                    break;
                                }
                                placed = again;
                                self.truncate_floats(count);
                                (shift, child_width) = (again.1, again.2);
                                if again.0 - bfc_y > cleared.unwrap_or(border_top) {
                                    cleared = Some(again.0 - bfc_y);
                                }
                                let top = cleared.unwrap_or(border_top);
                                child_space.containing_width = child_width;
                                child_space.bfc_left = bfc_x + x0 + shift;
                                child_space.bfc_top = bfc_y + top;
                                child_space.remaining =
                                    content_limit.map(|limit| limit - top + own_margin_top);
                                if let Some(retried) = self.block_level(child, child_space, None) {
                                    outcome = retried;
                                } else {
                                    laid = None;
                                    break;
                                }
                            }
                            laid.map(|()| outcome)
                        }
                        (outcome, _) => outcome,
                    };
                    self.beside_floats_basis.set(outer_basis);
                    self.trim_pending.set((false, false));
                    room_below = self.room_below.replace(Au::ZERO);
                    let Some(outcome) = outcome else {
                        self.truncate_floats(count);
                        // Floats that go on from the page before past this one make progress
                        // for a box that clears them.
                        if !placed && !space.at_top && (!past_end || !resumed_break) {
                            return None;
                        }
                        // Children that should stay with the next one move along with it, as long
                        // as something stays on this page (CSS Fragmentation 3, §4.4).
                        let mut back = None;
                        let mut avoid =
                            before.avoid_before || history.last().is_some_and(|last| last.avoid_after);
                        // Unless the items of the child's first line force a break before it,
                        // which wins over the avoided one, as in Chrome.
                        if avoid
                            && placed
                            && child_token.is_none()
                            && self.passes_forced_break(child, child_space)
                        {
                            next_token = Some(BreakToken::at(index, None));
                            self.forced_break.set(true);
                            break;
                        }
                        // The box before it breaks inside instead, at its last break on the page,
                        // where it can: that later break is the better one, as in Chrome (Blink
                        // takes the latest of the breaks of the best appeal, inside the boxes
                        // before too).
                        if avoid
                            && placed
                            && child_token.is_none()
                            && let Some(last) = history.last().copied()
                            && let Some(broken) = self.break_inside_before(boxes, last, &children)
                        {
                            children.truncate(last.children);
                            self.truncate_floats(last.floats);
                            y = broken.fragment.rect.y + broken.fragment.rect.height;
                            children.push(broken.fragment);
                            next_token = Some(BreakToken::at(last.index, broken.token.map(Box::new)));
                            break;
                        }
                        while avoid && let Some(last) = history.pop() {
                            avoid = last.avoid_before
                                || history.last().is_some_and(|earlier| earlier.avoid_after);
                            back = Some(last);
                        }
                        match back {
                            Some(back) if back.placed => {
                                children.truncate(back.children);
                                trailing_through.retain(|&index| index < back.children);
                                y = back.y;
                                previous = back.previous;
                                leading = back.leading;
                                collapsed_top = back.collapsed_top;
                                self.truncate_floats(back.floats);
                                next_token = Some(BreakToken::at(back.index, None));
                            }
                            Some(_) if !space.at_top && !self.last_resort_column.get() => return None,
                            _ => {
                                next_token = Some(BreakToken::at(index, child_token.cloned().map(Box::new)));
                            }
                        }
                        break;
                    };
                    // A forced break that layout finds before the child (before the first line
                    // of a flex or grid container) breaks before it, unless it comes first.
                    if index > start
                        && child_token.is_none()
                        && limit.is_some()
                        && self.forces_break(outcome.passed_breaks.0)
                    {
                        self.truncate_floats(count);
                        next_token = Some(BreakToken::at(index, None));
                        self.forced_break.set(true);
                        break;
                    }
                    if token.is_none() && history.is_empty() {
                        passed_breaks.0 = outcome.passed_breaks.0;
                    }
                    passed_breaks.1 = outcome.passed_breaks.1;
                    // The `break-after` that the items of a flex or grid container's last line
                    // pass on to it avoids the break after it too, as in Chrome.
                    let mut before = before;
                    before.avoid_after |= self.avoids_break(outcome.passed_breaks.1);
                    history.push(before);
                    last_child = Some(child);
                    // `margin-trim: block-start` trims the margins of the children at the start,
                    // and of the empty ones before them (CSS Box 4, §3.3).
                    let trimmed = style.margin_trim_block.0 && leading;
                    let child_top = if truncated || trimmed {
                        Margin::default()
                    } else {
                        outcome.collapsed_top
                    };
                    let mut outcome = outcome;
                    if trimmed && outcome.through {
                        outcome.collapsed_bottom = Margin::default();
                    }
                    let mut fragment = outcome.fragment;
                    let aligned = crate::block_justify::offset(style, child_style, child_width, &fragment)
                        .unwrap_or_else(|| legacy_align_offset(style, child_style, child_width, &fragment));
                    fragment.rect.x += x0 + shift + aligned;
                    placed = true;
                    let (count, start) = self.blocks_placed.get();
                    self.blocks_placed.set((count + 1, start));
                    // Whether the child is an empty box whose margins join those around it.
                    let mut through = false;
                    // An empty box that clears floats ending below this box's top plus the
                    // margins collapsed above the empty box has clearance in Chrome, even when
                    // those margins already take it past the floats: the margins above it stay
                    // above it, and only its own margins collapse with those below it
                    // (`BlockLayoutAlgorithm::FinishInflow` and `ComputeInflowPosition`). So does
                    // one whose margins would join ours: clearance separates them.
                    let clears_through = outcome.through
                        && !truncated
                        && self
                            .clearance(child_style.clear)
                            .is_some_and(|bottom| bottom - bfc_y > border_top - y);
                    if clears_through {
                        let top = cleared.unwrap_or(border_top);
                        fragment.rect.y = top;
                        y = top - child_top.resolve();
                        previous = child_top.adjoin(outcome.collapsed_bottom);
                        previous_cleared = true;
                        leading = false;
                        through = true;
                    } else if let Some(cleared) = cleared {
                        // Clearance separates the box's margins from those above it.
                        fragment.rect.y = cleared;
                    } else if leading && top_adjoins {
                        // The child's top margin joins ours, outside this box.
                        collapsed_top = collapsed_top.adjoin(child_top);
                        fragment.rect.y = y;
                        if outcome.through {
                            collapsed_top = collapsed_top.adjoin(outcome.collapsed_bottom);
                            through = true;
                        }
                    } else {
                        let combined = previous.adjoin(child_top);
                        fragment.rect.y = y + combined.resolve();
                        if outcome.through {
                            previous = combined.adjoin(outcome.collapsed_bottom);
                            through = true;
                        }
                    }
                    if marker_pending
                        && let Some(push) =
                            crate::list_marker::content_push(self.fonts, &fragment, style.writing_mode)
                    {
                        marker_pending = false;
                        fragment.rect.y += push;
                    }
                    if through {
                        trailing_through.push(children.len());
                    } else {
                        leading = false;
                        y = fragment.rect.y + fragment.rect.height;
                        previous = outcome.collapsed_bottom;
                        previous_cleared = false;
                        trailing_through.clear();
                    }
                    children.push(fragment);
                    match outcome.token {
                        // The child ends here, but its content goes on beside what follows it;
                        // not where that content holds a spanner of the columns being laid out,
                        // which what follows the child goes on below, as in Blink (which lays
                        // out nothing after the spanner's place in the content before it).
                        Some(rest @ BreakToken::Overflow { .. }) if !self.spanner_ahead(child, &rest) => {
                            overflow.push((index, rest));
                        }
                        Some(inner) => {
                            next_token = Some(BreakToken::at(index, Some(Box::new(inner))));
                            break;
                        }
                        None => {}
                    }
                    if self.forces_break(crate::tree::combine_breaks(
                        self.tree.break_after(child),
                        passed_breaks.1,
                    )) && index + 1 < boxes.len()
                        && limit.is_some()
                    {
                        next_token = Some(BreakToken::at(index + 1, None));
                        self.forced_break.set(true);
                        break;
                    }
                }
            }
            _ => {
                return Some(Ok(self.container(
                    id,
                    Space::unfragmented(space.containing_width),
                    width_override,
                )));
            }
        }

        // The floats of a formatting context that continue on the next page continue it.
        if root {
            let floats = self.take_float_tokens();
            if !floats.is_empty() {
                match &mut next_token {
                    Some(BreakToken::Block { floats: rest, .. } | BreakToken::Lines { floats: rest, .. }) => {
                        rest.extend(floats);
                    }
                    Some(_) => {}
                    None => {
                        next_token = Some(match &layout_box.kind {
                            BoxKind::Inline { .. } => BreakToken::Lines {
                                piece: lines_end.0,
                                offset: lines_end.1,
                                consumed: Consumed::default(),
                                floats,
                            },
                            kind => BreakToken::Block {
                                index: match kind {
                                    BoxKind::Block { children: boxes } => boxes.len(),
                                    _ => 0,
                                },
                                inner: None,
                                consumed: Consumed::default(),
                                floats,
                                overflow: Vec::new(),
                            },
                        });
                    }
                }
            }
        }
        // Where the content ends, below the last line or child.
        let content_end = y;
        // A formatting context that ends with monolithic content reaching past the end of the
        // page ends with that content there, as in Chrome: its last child's bottom margin, its
        // own bottom padding and margin take no room below it (its bottom border is drawn over
        // the content), unless it repeats them on every page.
        let ends_with_overflow = next_token.is_none()
            && !clones
            && height_override.is_none()
            && limit.is_some_and(|limit| content_end > limit)
            && crate::monolithic::ends_with_overflow(style)
            && children
                .iter()
                .rev()
                .find(|child| crate::monolithic::in_flow(child))
                .and_then(|child| crate::monolithic::content_bottom(child, Au::ZERO))
                == Some(content_end);
        let child_count = match &layout_box.kind {
            BoxKind::Block { children: boxes } => boxes.len(),
            _ => 0,
        };
        if ended {
            // Only the content overflowing the box is left: a part of the box without height.
            let token = with_overflow(next_token, overflow, child_count).map(|inner| BreakToken::Overflow {
                inner: Box::new(inner),
            });
            return Some(Ok(Outcome::new(
                Fragment {
                    rect: Rect::new(model.margin[LEFT], Au::ZERO, width, Au::ZERO),
                    kind: layout_box.fragment_kind(VisibleSides {
                        top: false,
                        right: false,
                        bottom: false,
                        left: false,
                    }),
                    children: children.into(),
                },
                token,
                Au::ZERO,
                Au::ZERO,
            )));
        }
        let mut broken = next_token.is_some();
        // A box whose height does not depend on all of its content ends on the page where what
        // is left of its height does, as in Chrome: the content that overflows it continues on
        // the next pages without it (`Overflow` tokens).
        let mut overflowing = None;
        // Not above a spanner, where the columns break and the box goes on below it.
        if broken
            && (height_override.is_none() || fixed_height)
            && !clones
            && let Some(limit) = limit
            && !next_token
                .as_ref()
                .is_some_and(|token| self.stops_at_spanner(id, token))
        {
            let earlier = token.map_or(Au::ZERO, BreakToken::consumed);
            let content = y - top;
            let own = match definite {
                Some(definite) if fixed_height => definite - earlier,
                _ => {
                    Self::used_height(
                        style,
                        &space.stretch_model(&model),
                        earlier + content,
                        space.containing_height,
                    ) - earlier
                }
            }
            .clamp_non_negative();
            // So does the content of a box of a definite height that ends on the page (Chrome
            // knows such a box to fit and ends it there).
            if (own < content || definite.is_some()) && top + own + bottom_frame <= limit {
                // Content that the box clips goes nowhere.
                overflowing = next_token
                    .take()
                    .filter(|_| style.overflow_y == fagus_style::values::Overflow::Visible);
                broken = false;
            }
        }
        // `margin-trim: block-end` trims the margins after the last child that is not empty
        // and those of the empty ones after it, all of them in a box with only empty children,
        // whose own margins stay (CSS Box 4, §3.3; Blink's `kMarginTrimEndDidNotApply`).
        if style.margin_trim_block.1 && !broken {
            previous = Margin::default();
            previous_cleared = false;
            for &index in &trailing_through {
                if let Some(child) = children.get_mut(index) {
                    child.rect.y = y;
                }
            }
            if leading {
                collapsed_top = Margin::of(if continuing { Au::ZERO } else { model.margin[TOP] });
            }
        }
        // The last child's bottom margin joins ours unless something separates them.
        let collapsed_bottom = if broken {
            Margin::default()
        } else if bottom_adjoins && !previous_cleared {
            previous.adjoin(Margin::of(model.margin[BOTTOM]))
        } else {
            // The margin below the end of the content of columns is cut at the end of the column,
            // as in browsers: the columns are as tall as their content.
            y = match content_limit {
                Some(limit) if self.spanning.get() == Some(id) => (y + previous.resolve()).min(limit.max(y)),
                // The last child's bottom margin inside the box is truncated at the end of the
                // page or column, as in Blink (`AdjustedMarginAfterFinalChildFragment`): balanced
                // columns do not grow for it. An item of a column flex container keeps the
                // height the flex layout gave it, which has the margin, unbroken.
                _ => match resumed_limit {
                    Some(limit) if height_override.is_none() && !content_sized => {
                        y + previous.resolve().min((limit - y).clamp_non_negative())
                    }
                    _ => y + previous.resolve(),
                },
            };
            Margin::of(model.margin[BOTTOM])
        };
        // Where the content ends if the box breaks below it: the bottom margin of the last child
        // is truncated at the end of the page, as in Chrome; in a box that repeats its bottom
        // padding and border on every page, at the end of the room above them (Blink reserves
        // that room for the content, and the margin then reaches into them).
        let mut truncated_end = if clones {
            content_limit.map_or(y, |limit| y.min(content_end.max(limit)))
        } else {
            limit.map_or(y, |limit| y.min(content_end.max(limit)))
        };
        // A formatting context contains its floats (CSS 2.1 §10.6.7).
        if root
            && let Some(bottom) = self
                .clearance(fagus_style::values::Clear::Both)
                .max(self.letter_clearance(fagus_style::values::Clear::Both))
        {
            y = y.max(bottom);
            truncated_end = truncated_end.max(bottom);
        }
        // A box that repeats its bottom padding and border on every page ends where the bottom
        // margin of its last child would reach into them at the end of the page: the margin is
        // truncated there, as in Chrome (also in balanced columns, which do not grow for it).
        if clones
            && !broken
            && let Some(limit) = content_limit
            && content_end <= limit
            && y > limit
        {
            y = truncated_end;
        }
        let content_height = y - top;
        // The height of the content box on the pages before, which counts towards the box's
        // height (CSS Fragmentation 3, §5.3).
        let earlier = token.map_or(Au::ZERO, BreakToken::consumed);
        let used_height = if broken {
            // Above a spanner, a box of a definite height (or a maximum one) ends where what is
            // left of its height does, its content overflowing it: the rows of columns below the
            // spanner have the rest of it, as in Blink.
            if next_token
                .as_ref()
                .is_some_and(|token| self.stops_at_spanner(id, token))
            {
                let own = Self::used_height(
                    style,
                    &space.stretch_model(&model),
                    earlier + content_height,
                    space.containing_height,
                ) - earlier;
                content_height.min(own.clamp_non_negative())
            } else {
                content_height
            }
        } else if fixed_height && let Some(definite) = definite {
            (definite - earlier).clamp_non_negative()
        } else if matches!(style.height, Size::Stretch)
            && height_override.is_some()
            && let Some(definite) = definite
        {
            // A `stretch` height that the box was given (between the insets of an absolutely
            // positioned box, from its static position) instead of its containing block's.
            (definite - earlier).clamp_non_negative()
        } else if height_override.is_some() && token.is_some() {
            // An item given a height that grows with its content where it breaks (see
            // `Layouter::fixed_item_height`) is at least as tall as its content on the pages
            // after its first, as in Chrome (`MinBlockSizeShouldEncompassIntrinsicSize`).
            (Self::used_height(
                style,
                &space.stretch_model(&model),
                earlier + content_height,
                space.containing_height,
            ) - earlier)
                .clamp_non_negative()
                .max(content_height)
        } else {
            (Self::used_height(
                style,
                &space.stretch_model(&model),
                earlier + content_height,
                space.containing_height,
            ) - earlier)
                .clamp_non_negative()
        };
        // A minimum height that makes the box taller than its content, or a maximum height that
        // makes it shorter, keeps the last child's bottom margin from collapsing through it: the
        // margin ends inside the box and is lost, as in Chrome (WPT
        // css/CSS2/normal-flow/margin-collapse-min-height-001, max-height-separates-margin).
        let raised = !broken && used_height != content_height;
        // The root and `body` of a document in quirks mode reach down to the end of the page.
        let used_height = match fill {
            Some(fill) if !broken => {
                let fill = (fill - frame_y).clamp_non_negative();
                let fill =
                    content_limit.map_or(fill, |limit| fill.min((limit - top - bottom_frame).max(Au::ZERO)));
                used_height.max(fill)
            }
            _ => used_height,
        };
        crate::annotation::room_after_box(
            &self.room_below,
            &model,
            (top, used_height - content_height),
            broken,
        );
        let mut height = top + used_height + if broken && !clones { Au::ZERO } else { bottom_frame };
        // A stretched box broken inside its content ends at the end of the page.
        if let Some(fixed) = height_override
            && !broken
        {
            height = height.max(fixed);
        }
        // Whether the box breaks inside its content, and so extends to the end of the page: not
        // where it breaks after the captions of a table whose box starts on the next page, which
        // Chrome ends below the captions.
        let fills_page = broken
            && !matches!(
                &next_token,
                Some(BreakToken::Block { inner: Some(inner), .. })
                    if matches!(**inner, BreakToken::Table { started: false, .. })
            );
        // The height of the box where it breaks below its content, with the truncated margin:
        // one whose height follows its content is that much shorter, a stretched, sized or
        // taller one is not (it then breaks before its bottom border and padding, below the
        // content, where a stretched box of the same height without the margin would not).
        let truncated_height = if height_override.is_some() || content_sized {
            height
        } else {
            top + (Self::used_height(
                style,
                &space.stretch_model(&model),
                earlier + truncated_end - top,
                space.containing_height,
            ) - earlier)
                .clamp_non_negative()
                + bottom_frame
        };
        // A box of a fixed height whose content reaches past the end of the page (monolithic
        // content laid out whole) ends at its own height, however far below the end of the page
        // that is: one fragment that reaches onto the next pages with its content and has its
        // bottom border and padding there, not repeated on each page where its decorations are
        // cloned, as in Chrome 157 (WPT css-page monolithic-overflow-031).
        let ends_at_height =
            height_override.is_none() && definite.is_some() && limit.is_some_and(|limit| content_end > limit);
        if !broken
            && !ends_with_overflow
            && !ends_at_height
            && bottom_frame > Au::ZERO
            && cell_height.is_none()
            && let Some(limit) = limit
            && truncated_height > limit
            && truncated_height <= truncated_end + bottom_frame
            && content_end > top
            && let BoxKind::Block { children: boxes } = &layout_box.kind
        {
            // The bottom border and padding do not fit below the content, which fills the box:
            // the box breaks before the last of it, or if it cannot, before them.
            if matches!(ending, Ending::Retry) {
                return Some(Err(content_end));
            }
            next_token = Some(BreakToken::at(boxes.len(), None));
            height = top + content_height;
            broken = true;
        }
        // A box taller than its content breaks at the end of the page like any other: the rest
        // of its height continues on the next one (CSS Fragmentation 3, §4), with all of its
        // bottom border and padding where the break falls inside them, as in Chrome (Blink's
        // `FinishFragmentation` breaks such a box before them).
        if !broken
            && !ends_with_overflow
            && let Some(limit) = limit
            && truncated_height > limit
            && (truncated_end <= limit || (content_end == top && !continuing && !space.at_top))
        {
            // Balanced columns would hold it whole that much taller, as Blink's
            // `FinishFragmentation` reports.
            self.note_shortage(truncated_height - limit.max(Au::ZERO));
            // A box whose top border and padding do not fit on the page moves to the next one,
            // as in Chrome.
            if (limit <= Au::ZERO || top > limit) && !space.at_top {
                return None;
            }
            // A stretched flex or grid item that repeats its bottom padding and border has used
            // all of the room on the page, them included (see below): what is left of its line
            // on the next page holds its rest with them.
            let rest = if clones && height_override.is_some() && cell_height.is_none() {
                truncated_height - limit - bottom_frame
            } else if clones && style.box_sizing == BoxSizing::BorderBox {
                // A border-box height holds the borders and padding repeated on every page, as in
                // Chrome: the rest of the content has that much less room.
                truncated_height - limit - (model.border[TOP] + model.padding[TOP] + bottom_frame)
            } else if clones {
                // The content that the page had no room for inside the repeated decorations,
                // no more than all of it.
                (truncated_height - limit).min(truncated_height - top - bottom_frame)
            } else {
                truncated_height - limit
            };
            let frame = if clones { Au::ZERO } else { bottom_frame };
            next_token = Some(BreakToken::Rest {
                rest: rest.max(frame),
            });
            height = limit.max(Au::ZERO);
            broken = true;
        }
        // A box broken inside its content extends to the end of the page (CSS Fragmentation 3,
        // §5.3), except above a spanner, where the content of the columns goes on below it.
        if fills_page
            && let Some(limit) = limit
            && !next_token
                .as_ref()
                .is_some_and(|token| self.stops_at_spanner(id, token))
        {
            height = height.max(limit + outer_slack);
        }
        // Children whose content overflows them continue in the box where it breaks, or past
        // its end, with the content overflowing it.
        let mut next_token = if broken {
            with_overflow(next_token, overflow, child_count)
        } else {
            // Content of the children that the box clips goes nowhere.
            if style.overflow_y != fagus_style::values::Overflow::Visible {
                overflow.clear();
            }
            with_overflow(overflowing, overflow, child_count).map(|inner| BreakToken::Overflow {
                inner: Box::new(inner),
            })
        };
        if broken && let Some(token) = &mut next_token {
            // A box that repeats its bottom padding and border on every page has used the room on
            // the page above them. A stretched flex or grid item has used all of the room on the
            // page, or as much of its height as its content reached with the bottom margin of its
            // last child before the break where that reaches further: its line goes on from
            // there on the next page, as in Chrome (Blink's row goes on below the items' block
            // end, their cloned bottom padding and border included).
            let used = if !clones {
                height - top
            } else if height_override.is_some() && cell_height.is_none() {
                let reached = if content_limit.is_some() {
                    y + previous.resolve()
                } else {
                    Au::ZERO
                };
                height.max(reached) - top
            } else {
                height - top - bottom_frame
            };
            token.set_consumed(earlier + used);
        }
        let mut collapsed_bottom = if broken {
            Margin::default()
        } else if bottom_adjoins && raised {
            Margin::of(model.margin[BOTTOM])
        } else {
            collapsed_bottom
        };
        if ends_with_overflow {
            height = content_end;
            collapsed_bottom = Margin::default();
        }
        // `align-content` moves the content of a box taller than it down (CSS Box Alignment 3,
        // §5.1), as Chrome centers the content of a button; `unsafe`, content that overflows the
        // box moves up past its start.
        if !broken && token.is_none() {
            let free = height - bottom_frame - top - content_height;
            let free = if style.align_content_unsafe {
                free
            } else {
                free.clamp_non_negative()
            };
            let shift = match style.align_content {
                ContentAlignment::Center => free / 2,
                ContentAlignment::End | ContentAlignment::FlexEnd => free,
                _ => Au::ZERO,
            };
            if shift != Au::ZERO {
                for child in &mut children {
                    child.rect.y += shift;
                }
            }
        }
        let kind = layout_box.fragment_kind(VisibleSides {
            top: !continuing || clones,
            bottom: !broken || clones,
            ..VisibleSides::ALL
        });
        // A box without content, height, border or padding lets its margins collapse through,
        // also one whose `height` is zero (as Blink's, whose block offset stays unresolved).
        let through =
            leading && top_adjoins && !root && bottom_frame == Au::ZERO && height == Au::ZERO && !broken;
        // An outside marker laid out in a formatting context inside the list item stands beside
        // the item's border box, as everywhere else in Chrome.
        if self.unplaced_marker.get()
            && !style.is_vertical()
            && matches!(
                style.display,
                fagus_style::values::Display::Normal { list_item: true, .. }
            )
        {
            place_nested_marker(&mut children, Au::ZERO, width);
            self.unplaced_marker.set(false);
        }

        Some(Ok(Outcome {
            fragment: Fragment {
                rect: Rect::new(model.margin[LEFT], Au::ZERO, width, height),
                kind,
                children: children.into(),
            },
            token: next_token,
            margin_bottom: if broken || ends_with_overflow {
                Au::ZERO
            } else {
                model.margin[BOTTOM]
            },
            collapsed_top,
            collapsed_bottom,
            through,
            passed_breaks: (
                passed_breaks.0,
                if broken {
                    BreakBetween::Auto
                } else {
                    passed_breaks.1
                },
            ),
        }))
    }

    /// Lays out a flex or grid container (unfragmented), optionally with a fixed content width.
    fn container(&self, id: BoxId, space: Space, width_override: Option<Au>) -> Outcome {
        self.container_lines(id, space, width_override, None).0
    }

    /// [`Layouter::container`], with the lines of the container (flex lines; a grid is one line),
    /// where it may break between pages. `height_override` is a border-box height the container
    /// is given (stretched, or between the insets of an absolutely positioned box), which its
    /// items then grow and align in.
    pub(crate) fn container_lines(
        &self,
        id: BoxId,
        space: Space,
        width_override: Option<Au>,
        height_override: Option<Au>,
    ) -> (Outcome, Vec<ContainerLine>) {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let model = self.flow_model(style, &space, width_override);
        let x0 = model.border[LEFT] + model.padding[LEFT];
        let y0 = model.border[TOP] + model.padding[TOP];
        let (items, out_of_flow): (Vec<BoxId>, Vec<BoxId>) = match &layout_box.kind {
            BoxKind::Flex { children } | BoxKind::Grid { children } => children
                .iter()
                .partition(|&&child| !self.tree.get(child).style.is_out_of_flow()),
            _ => (Vec::new(), Vec::new()),
        };
        // A fixed height or a minimum height gives the items room to grow and align in: it is the
        // cross size of a single-line row and the main size of a column.
        let containing_height = space.containing_height;
        let frame_y = y0 + model.padding[BOTTOM] + model.border[BOTTOM];
        let given_height = height_override.map(|height| (height - frame_y).clamp_non_negative());
        // The content height of the box as if it had no items, where size containment makes it
        // count as empty (CSS Containment 2, §3.1).
        let empty = match &layout_box.kind {
            BoxKind::Grid { .. } => self.contained_grid_height(id, model.content_width),
            _ => Au::from_px(style.contain_intrinsic_size.1.unwrap_or(0.0)),
        };
        let target_height = match (&style.height, &style.min_height) {
            _ if given_height.is_some() => given_height,
            (Size::Length(height), _) if height.fixed().is_some() || containing_height.is_some() => {
                Some(Self::used_height_contained(
                    style,
                    &space.stretch_model(&model),
                    Au::ZERO,
                    containing_height,
                    empty,
                ))
            }
            // `stretch` fills a containing block of a definite height.
            (Size::Stretch, _) if containing_height.is_some() => Some(Self::used_height_contained(
                style,
                &space.stretch_model(&model),
                Au::ZERO,
                containing_height,
                empty,
            )),
            // A height that follows the width through an aspect ratio, as in Chrome.
            _ if has_ratio_height(style) => Some(Self::used_height_contained(
                style,
                &space.stretch_model(&model),
                Au::ZERO,
                containing_height,
                empty,
            )),
            (_, Size::Length(min))
                if min.fixed().is_some_and(|min| min > 0.0)
                    || (min.fixed().is_none() && containing_height.is_some()) =>
            {
                Some(Self::used_height_contained(
                    style,
                    &space.stretch_model(&model),
                    Au::ZERO,
                    containing_height,
                    empty,
                ))
            }
            // With size containment the height does not depend on the items either.
            _ if style.size_containment().1 => Some(Self::used_height_contained(
                style,
                &space.stretch_model(&model),
                Au::ZERO,
                containing_height,
                empty,
            )),
            _ => None,
        };
        let (children, content_height, mut lines) = match &layout_box.kind {
            BoxKind::Flex { .. } => {
                self.given_height.set(given_height.is_some());
                let max_height = crate::flex_column::max_content_height(style, &model, containing_height);
                self.flex(style, &items, model.content_width, (target_height, max_height))
            }
            BoxKind::Grid { .. } => {
                let laid = self.grid(id, &items, model.content_width, target_height);
                self.note_grid_padding(id, (model.padding[LEFT], model.padding[TOP]));
                laid
            }
            _ => (Vec::new(), Au::ZERO, Vec::new()),
        };
        for line in &mut lines {
            line.top += y0;
            line.bottom += y0;
        }
        let out_of_flow_ids = out_of_flow.clone();
        let children = children
            .into_iter()
            .map(|mut fragment| {
                fragment.rect.x += x0;
                fragment.rect.y += y0;
                fragment
            })
            .chain(out_of_flow.into_iter().map(|child| {
                let start = if style.is_rtl() {
                    x0 + model.content_width
                } else {
                    x0
                };
                placeholder(child, start, y0)
            }))
            .collect();
        // A flex or grid `body` of a document in quirks mode fills the root too (Quirks Mode,
        // §3.6), as in Chrome.
        let fill = height_override
            .is_none()
            .then(|| self.quirky_fill(id, &model, &space))
            .flatten()
            .map_or(Au::ZERO, |fill| (fill - frame_y).clamp_non_negative());
        let content_height = given_height.unwrap_or_else(|| {
            Self::used_height_contained(
                style,
                &space.stretch_model(&model),
                content_height,
                containing_height,
                empty,
            )
            .max(fill)
        });
        if matches!(layout_box.kind, BoxKind::Flex { .. } | BoxKind::Grid { .. }) {
            let grid = matches!(layout_box.kind, BoxKind::Grid { .. });
            let mut statics = self.flex_statics.borrow_mut();
            for &child in &out_of_flow_ids {
                let item = &self.tree.get(child).style;
                // A grid that is the containing block places its children in their areas (see
                // `crate::grid_positioned`); a vertical one lays out turned, inside a box of its
                // own, and keeps them at the start.
                let contained = if item.position == fagus_style::values::Position::Fixed {
                    style.has_transform() || style.contains_out_of_flow()
                } else {
                    style.contains_absolute()
                };
                if grid && (contained || style.writing_mode.is_vertical()) {
                    continue;
                }
                let size = (model.content_width, content_height);
                statics.insert(
                    child,
                    if grid {
                        FlexStatic::of_grid(style, item, size)
                    } else {
                        FlexStatic::of(style, item, size)
                    },
                );
            }
        }
        let height = y0 + content_height + model.padding[BOTTOM] + model.border[BOTTOM];
        let width = model.content_width
            + model.border[LEFT]
            + model.border[RIGHT]
            + model.padding[LEFT]
            + model.padding[RIGHT];
        let outcome = Outcome::new(
            Fragment {
                rect: Rect::new(model.margin[LEFT], Au::ZERO, width, height),
                kind: FragmentKind::Box {
                    style: Arc::clone(style),
                    node: layout_box.node,
                    sides: VisibleSides::ALL,
                },
                children,
            },
            None,
            model.margin[TOP],
            model.margin[BOTTOM],
        );
        (outcome, lines)
    }

    /// Lays out a flex or grid container across pages, optionally with a fixed content width.
    /// A column with an automatic height breaks like a block when it does not fit (its items
    /// grow and justify only when it does); other containers break between and inside their
    /// lines.
    pub(crate) fn flex_or_grid(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        width_override: Option<Au>,
    ) -> Option<Outcome> {
        self.flex_or_grid_with(id, space, token, width_override, None)
    }

    /// [`Layouter::flex_or_grid`] for a flex or grid item stretched to the border-box height
    /// `full` in a flex row (or a grid area), which it keeps where it breaks: its own items (`1fr`
    /// rows, auto margins) take that height as when it does not break, as in Chrome.
    fn flex_or_grid_with(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        width_override: Option<Au>,
        full: Option<Au>,
    ) -> Option<Outcome> {
        // A container that ended on a page before, whose items' content overflows it and goes
        // on: the items continue at the top of the page, in a part of the container without
        // height or decorations (as Blink's container goes on in fragments of no block size
        // while a child has a break token).
        if let Some(BreakToken::Overflow { inner }) = token {
            let continued = self.flex_or_grid(id, space, Some(inner), width_override)?;
            let fragment = continued.fragment;
            let token = continued.token.map(|token| match token {
                rest @ BreakToken::Overflow { .. } => rest,
                rest => BreakToken::Overflow {
                    inner: Box::new(rest),
                },
            });
            return Some(Outcome::new(
                Fragment {
                    rect: Rect::new(fragment.rect.x, Au::ZERO, fragment.rect.width, Au::ZERO),
                    kind: self.tree.get(id).fragment_kind(VisibleSides {
                        top: false,
                        right: false,
                        bottom: false,
                        left: false,
                    }),
                    children: fragment.children,
                },
                token,
                Au::ZERO,
                Au::ZERO,
            ));
        }
        if self.sized_column(id, &space) {
            return self.flex_column_part(id, space, token, width_override, None);
        }
        let style = &self.tree.get(id).style;
        let column = matches!(
            style.flex_direction,
            FlexDirection::Column | FlexDirection::ColumnReverse
        ) && matches!(self.tree.get(id).kind, BoxKind::Flex { .. })
            && matches!(style.height, Size::Auto);
        // A column flex container whose height is its content's breaks as one with a height of
        // its own does (see `flex_column`): its items keep their places in the unbroken column.
        if column && space.remaining.is_some() {
            return self.flex_column_part(id, space, token, width_override, None);
        }
        self.container_part(id, space, token, width_override, full)
    }

    /// Lays out the part of a flex or grid container that fits on the page, from `token` on:
    /// its lines go on the page while they fit, and the line that reaches past the end of the
    /// page breaks inside its items, which continue on the next page (see [`crate::parallel`]),
    /// as in Chrome. `break-before` and `break-after` of the items force or avoid breaks between
    /// the lines.
    #[allow(clippy::too_many_lines, reason = "the steps of breaking a container")]
    fn container_part(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        width_override: Option<Au>,
        full: Option<Au>,
    ) -> Option<Outcome> {
        // A poor break inside is the container's to take, between its rows, not its items'.
        let poor = self.poor_lines.replace(false);
        let outcome = self.container_part_in(id, space, token, width_override, (poor, full));
        self.poor_lines.set(poor);
        outcome
    }

    /// [`Layouter::container_part`], where `poor` tells whether the container may break inside
    /// the items of a row that avoid it (see [`Layouter::poor_lines`]).
    fn container_part_in(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        width_override: Option<Au>,
        (poor, full): (bool, Option<Au>),
    ) -> Option<Outcome> {
        let Some(remaining) = space.remaining else {
            return Some(self.container_lines(id, space, width_override, full).0);
        };
        let (outcome, lines) = self.whole_container(
            id,
            Space {
                remaining: None,
                ..space
            },
            width_override,
            full,
        );
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let between = self.line_breaks(
            &lines,
            &outcome.fragment.children,
            matches!(self.tree.get(id).kind, BoxKind::Grid { .. }),
        );
        let model = self.flow_model(style, &space, width_override);
        // A forced break before the first row of a grid, which its border or padding separate
        // from the top of the grid, leaves the grid's first part on the page empty, as in Chrome
        // (the break also moves the grid to the next page).
        let empty_first = token.is_none()
            && matches!(layout_box.kind, BoxKind::Grid { .. })
            && model.border[TOP] + model.padding[TOP] > Au::ZERO
            && between.first().is_some_and(|&value| self.forces_break(value));
        let originals_forced = matches!(layout_box.kind, BoxKind::Grid { .. })
            && lines
                .iter()
                .any(|line| !self.forced_rows(line, &outcome.fragment.children).is_empty());
        // So does one inside an item, as in Chrome, which breaks the item there.
        let inside = lines
            .iter()
            .flat_map(|line| &line.boxes)
            .any(|item| self.breaks_inside(item.id));
        let forced = empty_first
            || originals_forced
            || inside
            || between.iter().skip(1).any(|&value| self.forces_break(value));
        // The items of the first line pass their `break-before` on to the container, those of
        // the last line their `break-after`, as in Chrome.
        let passed_after = lines.last().map_or(BreakBetween::Auto, |line| {
            line.boxes
                .iter()
                .map(|item| self.tree.break_after(item.id))
                .fold(BreakBetween::Auto, crate::tree::combine_breaks)
        });
        let passed_before = if token.is_none() {
            between.first().copied().unwrap_or(BreakBetween::Auto)
        } else {
            BreakBetween::Auto
        };
        // The container's own box fits on the page; the content of its items that overflows
        // them may not, and breaks with them (see `parallel::flow_extent`).
        let own_fits =
            token.is_none() && outcome.fragment.rect.height + outcome.collapsed_top.resolve() <= remaining;
        if own_fits
            && !forced
            && crate::parallel::flow_extent(&outcome.fragment) + outcome.collapsed_top.resolve() <= remaining
        {
            self.forget_container(id);
            return Some(Outcome {
                passed_breaks: (passed_before, passed_after),
                ..outcome
            });
        }
        let clones = style.clones_decorations();
        let continuing = token.is_some();
        let y0 = model.border[TOP] + model.padding[TOP];
        // A grid broken inside its top padding (see below) continues with the rest of it, which
        // its token records as a negative height consumed.
        let top = match token {
            Some(token) if !clones => (-token.consumed()).max(Au::ZERO),
            Some(_) | None => y0,
        };
        let bottom_frame = model.padding[BOTTOM] + model.border[BOTTOM];
        let limit = remaining - if continuing { Au::ZERO } else { model.margin[TOP] };
        // Where content may end on this page.
        let content_limit = if clones { limit - bottom_frame } else { limit };
        let Outcome { fragment, .. } = outcome;
        // A container no taller than its lines ends with the last of them, as one whose height
        // follows its content does.
        let follows_content = matches!(style.height, Size::Auto)
            || fragment.rect.height - y0 - bottom_frame <= lines.last().map_or(Au::ZERO, |line| line.bottom);
        let kind = |broken: bool| {
            layout_box.fragment_kind(VisibleSides {
                top: !continuing || clones,
                bottom: !broken || clones,
                ..VisibleSides::ALL
            })
        };
        // How much taller than its content the container is on this page (see
        // `grid_item_overflow`).
        let incoming_overflow = match token {
            Some(BreakToken::Container { overflow, .. }) => *overflow,
            _ => Au::ZERO,
        };
        // The same, for the next page, and how far the container reaches past the end of this
        // one for it.
        let mut outgoing_overflow = (Au::ZERO, Au::ZERO);
        let (start, mut inside) = match token {
            Some(BreakToken::Container { line, inside, .. }) => (*line, inside.as_ref()),
            // The rest of a container taller than its content.
            Some(BreakToken::Rest { rest }) => {
                let frames = if clones { top + bottom_frame } else { Au::ZERO };
                let placed = (*rest).min((limit - frames).max(Au::ZERO));
                let broken = placed < *rest;
                return Some(Outcome::new(
                    Fragment {
                        rect: Rect::new(fragment.rect.x, Au::ZERO, fragment.rect.width, placed + frames),
                        kind: kind(broken),
                        children: Children::new(),
                    },
                    broken.then(|| BreakToken::Rest { rest: *rest - placed }),
                    Au::ZERO,
                    if broken { Au::ZERO } else { model.margin[BOTTOM] },
                ));
            }
            _ => (0, None),
        };
        // How much taller than laid out whole the lines broken on the pages before have grown
        // with their breaks.
        let mut expansion = {
            let growth = self.grid_growth.borrow();
            (0..start)
                .filter_map(|line| growth.get(&(id, line)))
                .fold(Au::ZERO, |sum, &grown| sum + grown)
        };
        let earlier = token.map_or(Au::ZERO, BreakToken::consumed);
        let item_count = lines.iter().map(|line| line.items.end).max().unwrap_or(0);
        let mut originals = fragment.children.into_vec();
        let mut children: Vec<Fragment> = if continuing {
            Vec::new()
        } else {
            originals.split_off(item_count.min(originals.len()))
        };
        let mut y = top;
        let mut next = None;
        // The lines placed whole on this page: index, children and `y` before them.
        let mut history: Vec<(usize, usize, Au)> = Vec::new();
        let place = |children: &mut Vec<Fragment>, fragments: Vec<Fragment>, shift: Au| {
            children.extend(fragments.into_iter().map(|mut fragment| {
                fragment.rect.y += shift;
                fragment
            }));
        };
        // How stretched items grow where their breaks took room from them (see `parallel`).
        let growth = match &self.tree.get(id).kind {
            BoxKind::Grid { .. } => crate::parallel::Growth::Grid,
            BoxKind::Flex { .. }
                if !matches!(
                    style.flex_direction,
                    FlexDirection::Column | FlexDirection::ColumnReverse
                ) =>
            {
                crate::parallel::Growth::Flex
            }
            _ => crate::parallel::Growth::None,
        };
        // The stretched items of a single-line row of a definite height are as tall as the
        // container, which they do not grow beyond where they break: their content overflows
        // them, as in Chrome (`MinBlockSizeShouldEncompassIntrinsicSize` in Blink's flex layout).
        let fixed_stretch = growth == crate::parallel::Growth::Flex
            && style.flex_wrap == FlexWrap::NoWrap
            && Self::definite_height(style, &space.stretch_model(&model), space.containing_height, None)
                .is_some();
        // Where the items of a line that goes on at the top of the page end there, below the
        // next line, which starts beside them (see below).
        let mut beside_end = Au::ZERO;
        for (index, line) in lines.iter().enumerate().skip(start) {
            // The line of a flex container without items holds nothing: the container breaks as
            // an empty box of its height, as in Chrome.
            if line.items.is_empty() && matches!(layout_box.kind, BoxKind::Flex { .. }) {
                continue;
            }
            let first = index == start;
            // A grid row without items is empty space, which breaks anywhere, as in Chrome: what
            // is left of it continues at the top.
            let empty_row = growth == crate::parallel::Growth::Grid && line.items.is_empty();
            if first
                && empty_row
                && let Some((_, offset)) = inside.take()
            {
                let rest = (line.bottom - line.top - *offset).clamp_non_negative();
                if y + rest > content_limit && content_limit > y {
                    next = Some((index, Some((Vec::new(), *offset + content_limit - y))));
                    y = content_limit;
                    break;
                }
                y += rest;
                continue;
            }
            // A line broken on the page before continues at the top.
            if first && let Some((tokens, offset)) = inside.take() {
                let available = content_limit - y;
                let portion = self.split_container_line(
                    growth,
                    line,
                    &originals,
                    Some((tokens, *offset)),
                    available,
                    (space.at_top, space.flush && y == Au::ZERO, false, fixed_stretch),
                )?;
                let mut height = portion.height;
                let mut broken = portion.next.clone();
                // A last line that goes on only for the monolithic content of its items, which
                // all end on the page, ends the container there (see below).
                if index + 1 == lines.len()
                    && bottom_frame == Au::ZERO
                    && !clones
                    && follows_content
                    && line_ends_with_overflow(
                        line,
                        &originals,
                        &portion,
                        growth == crate::parallel::Growth::Flex,
                    )
                {
                    let flex = growth == crate::parallel::Growth::Flex;
                    let reach = portion
                        .fragments
                        .iter()
                        .filter_map(|(item, fragment)| {
                            let bottom = crate::monolithic::content_bottom(fragment, Au::ZERO)?;
                            let margin = line.boxes.get(*item).map_or(Au::ZERO, |item| item.margins.1);
                            Some(if flex && bottom == fragment.rect.y + fragment.rect.height {
                                bottom + margin
                            } else {
                                bottom
                            })
                        })
                        .max();
                    if let Some(reach) = reach {
                        height = reach;
                        broken = None;
                    }
                }
                // A line of a multi-line row flex container whose items go on past where the line
                // ends laid out whole (their content grew with the breaks) has the next line start
                // at the top of the page, beside the rest of those items, as in Chrome (Blink's
                // flex layout expands the row to the end of the page where it ended, with its
                // items going on in parallel flows).
                // (On pages, the next line follows items broken by forced breaks inside them,
                // as in Chrome; in columns it starts beside them too, as WPT
                // multi-line-row-flex-fragmentation-053 has it.)
                let beside = growth == crate::parallel::Growth::Flex
                    && broken.is_none()
                    && index + 1 < lines.len()
                    && line.bottom - line.top <= *offset
                    && (self.columns.get() > 0 || !line.boxes.iter().any(|item| self.breaks_inside(item.id)));
                let grown = (*offset + height - (line.bottom - line.top)).max(Au::ZERO);
                if broken.is_none() {
                    self.grid_growth.borrow_mut().insert((id, index), grown);
                    expansion += grown;
                }
                if growth == crate::parallel::Growth::Grid
                    && let Some(inside) = &mut broken
                {
                    let rest = lines.last().map_or(Au::ZERO, |last| last.bottom - line.bottom)
                        + if clones { Au::ZERO } else { bottom_frame };
                    outgoing_overflow = grid_item_overflow(
                        &portion.fragments,
                        inside,
                        line.bottom - line.top,
                        available,
                        rest,
                    );
                }
                place(
                    &mut children,
                    portion
                        .fragments
                        .into_iter()
                        .map(|(_, fragment)| fragment)
                        .collect(),
                    y,
                );
                if beside {
                    // The line starts at the top of the page, above monolithic content of the
                    // items that reaches into it from the page before, which only they make room
                    // for (Blink keeps that overflow in their break tokens).
                    beside_end = beside_end.max(y + height);
                    if y == top {
                        y -= self.page_overflow.get();
                    }
                } else {
                    y += height;
                }
                if let Some(inside) = broken {
                    next = Some((index, Some(inside)));
                    break;
                }
                continue;
            }
            if (!first || empty_first) && self.forces_break(between[index]) {
                next = Some((index, None));
                self.forced_break.set(true);
                break;
            }
            // The gap above the line; at the top of a page after a break, none, but what is left
            // of the room above the first line.
            let gap = if index == 0 {
                (line.top - y0 - earlier.max(Au::ZERO)).max(Au::ZERO)
            } else if first {
                Au::ZERO
            } else {
                line.top - lines[index - 1].bottom
            };
            let line_top = y + gap;
            // The room that `align-content` leaves above the first line is empty space: where the
            // line starts below the end of the page, the container breaks in it, and the line
            // keeps its place in the container on the pages after, as in Chrome.
            if index == 0 && gap > Au::ZERO && line_top > content_limit && content_limit > top {
                y = content_limit;
                next = Some((index, None));
                break;
            }
            let height = line.bottom - line.top;
            let frame = if index + 1 == lines.len() && !clones {
                bottom_frame
            } else {
                Au::ZERO
            };
            let at_top = space.at_top && first;
            // Nothing lies above the line on the page: monolithic items that do not fit stay
            // there only then, as in Chrome (see `Space::flush`). Nor does anything but the top
            // border and padding of a container that repeats them on every page, which could
            // otherwise not progress, as in block flow.
            let flush = at_top
                && (space.flush && line_top == Au::ZERO
                    || clones && (continuing || space.flush) && line_top == top);
            let whole = |children: &mut Vec<Fragment>| {
                place(
                    children,
                    originals[line.items.clone()].to_vec(),
                    line_top - line.top,
                );
            };
            if empty_row && line_top + height > content_limit && line_top < content_limit {
                history.push((index, children.len(), y));
                next = Some((index, Some((Vec::new(), content_limit - line_top))));
                y = content_limit;
                break;
            }
            // A forced break inside an item breaks the line there, as in Chrome.
            let forced_inside = growth == crate::parallel::Growth::Grid
                && !self.forced_rows(line, &originals).is_empty()
                || line.boxes.iter().any(|item| self.breaks_inside(item.id));
            // The content of items that overflows them breaks with the line (see
            // `parallel::flow_extent`).
            let reach = originals[line.items.clone()]
                .iter()
                .map(|item| item.rect.y - line.top + crate::parallel::flow_extent(item))
                .fold(height + frame, Au::max);
            if line_top + reach <= content_limit && !forced_inside {
                history.push((index, children.len(), y));
                whole(&mut children);
                y = line_top + height;
                continue;
            }
            // Lines whose items do not break (columns) move to the next page as a whole.
            let split = |avoided: bool| {
                if line.boxes.len() != line.items.len() {
                    return None;
                }
                let outer = self.breaks_avoided_items.replace(avoided);
                let portion = self.split_container_line(
                    growth,
                    line,
                    &originals,
                    None,
                    content_limit - line_top,
                    (
                        at_top,
                        flush,
                        index > start && self.avoids_break(between[index]),
                        fixed_stretch,
                    ),
                );
                self.breaks_avoided_items.set(outer);
                portion
            };
            // Where the break before a grid row that does not fit is avoided too, the row breaks
            // inside the items that avoid breaks inside it, as in Chrome (Blink's grid takes the
            // break inside where it is no worse than the one before the row, without looking for
            // a better one between the rows above; see below for the block around).
            let portion = split(false).or_else(|| {
                (growth == crate::parallel::Growth::Grid
                    && index > start
                    && self.avoids_break(between[index]))
                .then(|| split(true))
                .flatten()
                .filter(|portion| !portion.fragments.is_empty())
            });
            let Some(portion) = portion else {
                if at_top && (flush || self.lenient_top.get()) {
                    whole(&mut children);
                    y = line_top + height;
                    continue;
                }
                // The line moves to the next page, with the lines before it that avoid a break
                // before it, as long as something stays on this page. A grid row that starts at
                // or below the end of the page goes on to the next one alone, as in Chrome
                // (Blink's grid leaves its items to the next fragment without a breakpoint).
                let mut cut = (index, children.len(), y);
                let below = growth == crate::parallel::Growth::Grid && line_top >= content_limit;
                while !below
                    && self.avoids_break(between[cut.0])
                    && let Some(entry) = history.pop()
                {
                    cut = entry;
                }
                if cut.0 == start {
                    // A grid whose first row does not fit breaks before that row, its top border
                    // and padding staying on the page, as in Chrome; other containers move to
                    // the next page as a whole. A grid item that avoids breaks inside it does so
                    // too where its row breaks inside such items (see `breaks_avoided_items`).
                    let grid_breaks_before_row = start == 0
                        && !continuing
                        && y0 > Au::ZERO
                        && matches!(layout_box.kind, BoxKind::Grid { .. })
                        && (!self.avoids_breaks_inside(style.break_inside)
                            || self.breaks_avoided_items.get());
                    // A container whose own box fits stays on the page, its first line reaching
                    // out of it past the end of the page, as in Chrome.
                    if own_fits && !space.at_top && !forced {
                        whole(&mut children);
                        y = line_top + height;
                        continue;
                    }
                    if !space.at_top && !grid_breaks_before_row {
                        return None;
                    }
                    if !grid_breaks_before_row {
                        cut = (index, children.len(), y);
                    }
                }
                children.truncate(cut.1);
                y = cut.2;
                // A row gap that crosses the end of the page is left out, as Chrome leaves it
                // out where it lies in the grid laid out whole: as much of it as the rows broken
                // on the pages before grew goes on at the top of the next page.
                let carry = (line_top - content_limit).min(expansion);
                let carried = (cut.0 == index
                    && matches!(layout_box.kind, BoxKind::Grid { .. })
                    && line.boxes.len() == line.items.len()
                    && y <= content_limit
                    && carry > Au::ZERO)
                    .then(|| (vec![None; line.boxes.len()], -carry));
                next = Some((cut.0, carried));
                break;
            };
            // An item that avoids breaks inside it and breaks all the same, as the break before
            // its row is avoided too, makes the break inside a grid that starts on the page below
            // other content a poor one: Blink's grid passes the appeal of that break on to the
            // block around it, which breaks before the grid instead unless every break before it
            // on the page is avoided too (see `Layouter::poor_lines`).
            if growth == crate::parallel::Growth::Grid
                && !poor
                && !continuing
                && !space.at_top
                && self.breaks_avoiding_item(line, &portion)
            {
                return None;
            }
            // A grid row whose items all end on the page, their monolithic content reaching past
            // its end, takes the page down to the end of that content, below which the rest of
            // the row goes on, as in Chrome (Blink's grid fragment encompasses the overflowing
            // items, and the rest of the row moves below the overflow on the pages after).
            let mut portion = portion;
            let mut row_ends = false;
            if growth == crate::parallel::Growth::Grid
                && portion.fragments.len() == line.boxes.len()
                && let Some((tokens, offset)) = &mut portion.next
                && tokens.iter().all(Option::is_none)
                && let Some(reach) = portion
                    .fragments
                    .iter()
                    .filter_map(|(_, fragment)| crate::monolithic::content_bottom(fragment, Au::ZERO))
                    .max()
                && reach > portion.height
            {
                if reach < height {
                    *offset += reach - portion.height;
                    portion.height = reach;
                } else {
                    row_ends = true;
                }
            }
            // Where that content reaches past the end of the row, the row ends with it.
            if row_ends {
                portion.height = height;
                portion.next = None;
            }
            // A last line whose items all end on the page, its monolithic content reaching past
            // the end, ends the container there with the overflow as a block container does,
            // as in Chrome (Blink's container takes its whole height where only unbreakable
            // content is left; see `monolithic`).
            if index + 1 == lines.len()
                && frame == Au::ZERO
                && follows_content
                && line_ends_with_overflow(
                    line,
                    &originals,
                    &portion,
                    growth == crate::parallel::Growth::Flex,
                )
            {
                history.push((index, children.len(), y));
                whole(&mut children);
                y = line_top + height;
                continue;
            }
            history.push((index, children.len(), y));
            let mut broken = portion.next.clone();
            if growth == crate::parallel::Growth::Grid
                && let Some(inside) = &mut broken
            {
                let rest = lines.last().map_or(Au::ZERO, |last| last.bottom - line.bottom)
                    + if clones { Au::ZERO } else { bottom_frame };
                outgoing_overflow =
                    grid_item_overflow(&portion.fragments, inside, height, content_limit - line_top, rest);
            }
            place(
                &mut children,
                portion
                    .fragments
                    .into_iter()
                    .map(|(_, fragment)| fragment)
                    .collect(),
                line_top,
            );
            y = line_top + portion.height;
            if let Some(inside) = broken {
                next = Some((index, Some(inside)));
                break;
            }
        }
        y = y.max(beside_end);
        // Where the bottom border and padding do not fit below the last line, the last line
        // moves to the next page with them, if a line stays on this one.
        if next.is_none()
            && !clones
            && y <= limit
            && y + bottom_frame > limit
            && matches!(style.height, Size::Auto)
            && let Some(&(line, count, before)) = history.last()
            && line > start
        {
            children.truncate(count);
            y = before;
            next = Some((line, None));
        } else if next.is_none()
            && !clones
            && !continuing
            && !space.at_top
            && y <= limit
            && y + bottom_frame > limit
            && matches!(style.height, Size::Auto)
            && history.last().is_some_and(|&(line, _, _)| line == start)
        {
            // The container's only line on the page would leave its bottom border and padding
            // alone on the next one: the container moves there as a whole instead, as in Chrome
            // (a break before the box is better than one before its end).
            return None;
        }
        let before_first_line = next
            .as_ref()
            .is_some_and(|(line, inside)| *line == 0 && inside.is_none());
        let mut next_token = next.map(|(line, inside)| BreakToken::Container {
            line,
            inside,
            consumed: Consumed::default(),
            overflow: outgoing_overflow.0,
        });
        let mut broken = next_token.is_some();
        // The overflow of the items on the page before makes the container that much taller,
        // below its content (which goes on at the top of the page beside the overflow).
        let content = y - top + if broken { Au::ZERO } else { incoming_overflow };
        // A grid that breaks before its first row inside its top border and padding fills the
        // page with them; the rest of them goes on, as Chrome slices them.
        let sliced_top = before_first_line && !continuing && !clones && y == top && top > limit;
        let mut height = if sliced_top {
            limit.max(Au::ZERO)
        } else if broken {
            limit.max(y) + outgoing_overflow.1
        } else {
            // The height of the content box on the pages before counts towards the container's;
            // a container of fixed height does not grow with items moved down by the break, as
            // in Chrome (they overflow it).
            let used = (Self::used_height(
                style,
                &space.stretch_model(&model),
                earlier + content,
                space.containing_height,
            ) - earlier)
                .clamp_non_negative();
            top + used + bottom_frame
        };
        // A container without content at the top of a page or column breaks like an empty box
        // where the page has room for anything at all.
        if !broken && height > limit && y <= limit && !(space.at_top && content == Au::ZERO && limit <= y) {
            // (Where nothing but they are left, on a page after the content, they break across
            // the pages as Chrome slices them.)
            next_token = Some(if height == y + bottom_frame && !(continuing && y == top) {
                // The bottom border and padding do not fit below the content: they go on alone.
                height = y;
                BreakToken::Container {
                    line: lines.len(),
                    inside: None,
                    consumed: Consumed::default(),
                    overflow: Au::ZERO,
                }
            } else {
                // A container taller than its content breaks at the end of the page.
                let rest = height - limit;
                height = limit;
                BreakToken::Rest { rest }
            });
            broken = true;
        }
        if let Some(token) = &mut next_token {
            token.set_consumed(earlier + height - top - if clones { bottom_frame } else { Au::ZERO });
        }
        // A container whose height does not follow its content ends on the page where what is
        // left of its height does, as a block container does, where its items break: the content
        // of its items that overflows it goes on on the next pages without it, as in Chrome.
        if broken
            && !clones
            && !sliced_top
            && style.overflow_y == fagus_style::values::Overflow::Visible
            && matches!(next_token, Some(BreakToken::Container { inside: Some(_), .. }))
        {
            let own = (Self::used_height(
                style,
                &space.stretch_model(&model),
                earlier + content,
                space.containing_height,
            ) - earlier)
                .clamp_non_negative();
            let definite =
                Self::definite_height(style, &space.stretch_model(&model), space.containing_height, None)
                    .is_some();
            // A container stretched to `full` is that tall, or as tall as its content without
            // the stretch where that is taller (Chrome's stretched items grow to their content
            // where they break, unless their children's heights are percentages of theirs):
            // not as tall as the overflow of its items.
            let stretched = full.filter(|_| token.is_none()).map(|full| {
                if self.has_percentage_heights(id) {
                    return (full - top - bottom_frame).clamp_non_negative();
                }
                let natural = self
                    .container_lines(
                        id,
                        Space {
                            remaining: None,
                            ..space
                        },
                        width_override,
                        None,
                    )
                    .0
                    .fragment
                    .rect
                    .height;
                (full.max(natural) - top - bottom_frame).clamp_non_negative()
            });
            let (own, definite) = stretched.map_or((own, definite), |stretched| (stretched, true));
            if (own < content || definite) && top + own + bottom_frame <= limit {
                next_token = next_token.map(|inner| BreakToken::Overflow {
                    inner: Box::new(inner),
                });
                height = top + own + bottom_frame;
                broken = false;
            }
        }
        Some(Outcome {
            passed_breaks: (
                passed_before,
                if broken { BreakBetween::Auto } else { passed_after },
            ),
            ..Outcome::new(
                Fragment {
                    rect: Rect::new(fragment.rect.x, Au::ZERO, fragment.rect.width, height),
                    kind: kind(broken),
                    children: children.into(),
                },
                next_token,
                if continuing { Au::ZERO } else { model.margin[TOP] },
                if broken { Au::ZERO } else { model.margin[BOTTOM] },
            )
        })
    }

    /// The border-box height left on this page and after of a box stretched to `full` that
    /// continues at `token`: what the pages before did not take of it, without its top border
    /// and padding unless they are cloned. `None` for the rest of a box after its content.
    pub(crate) fn stretched_rest(
        &self,
        id: BoxId,
        full: Au,
        containing_width: Au,
        token: Option<&BreakToken>,
    ) -> Option<Au> {
        let Some(token) = token else {
            return Some(full);
        };
        if matches!(token, BreakToken::Rest { .. }) {
            return None;
        }
        let style = &self.tree.get(id).style;
        let model = self.box_model(style, containing_width, None);
        let top = if style.clones_decorations() {
            Au::ZERO
        } else {
            model.border[TOP] + model.padding[TOP]
        };
        Some((full - top - token.consumed()).clamp_non_negative())
    }

    /// Whether the items of the first line of flex or grid container `id` pass a forced
    /// `break-before` on to it (see [`Outcome::passed_breaks`]), laid out whole in `space`.
    fn passes_forced_break(&self, id: BoxId, space: Space) -> bool {
        if !matches!(
            self.tree.get(id).kind,
            BoxKind::Flex { .. } | BoxKind::Grid { .. }
        ) {
            return false;
        }
        let whole = Space {
            remaining: Some(Au(i32::MAX / 4)),
            ..space
        };
        // The layout takes the forced breaks between the lines, which this page does not.
        let forced = self.forced_break.get();
        let passes = self
            .block_level(id, whole, None)
            .is_some_and(|outcome| self.forces_break(outcome.passed_breaks.0));
        self.forced_break.set(forced);
        passes
    }

    /// The `break-before` and `break-after` that the first and last lines of flex or grid item
    /// `item` pass on to it (see [`Outcome::passed_breaks`]), found by laying it out whole where
    /// a forced break lies inside it: they break between the lines of its container, as in
    /// Chrome.
    fn passed_breaks(&self, item: &LineItem) -> (BreakBetween, BreakBetween) {
        if !matches!(
            self.tree.get(item.id).kind,
            BoxKind::Flex { .. } | BoxKind::Grid { .. }
        ) || !self.forces_breaks_below(item.id)
        {
            return (BreakBetween::Auto, BreakBetween::Auto);
        }
        let forced = self.forced_break.get();
        // (Laid out on a page of no end: the lines find the breaks only on pages.)
        let space = Space {
            remaining: Some(Au(i32::MAX / 4)),
            containing_height: item.containing_height,
            ..Space::unfragmented(item.containing_width)
        };
        let passed = self
            .flex_or_grid_with(item.id, space, None, item.width, None)
            .map_or((BreakBetween::Auto, BreakBetween::Auto), |outcome| {
                outcome.passed_breaks
            });
        self.forced_break.set(forced);
        passed
    }

    /// Whether a box broke at `token` inside its bottom padding and border only, after all of its
    /// content (a stretched box whose padding and border do not fit on the page).
    pub(crate) fn breaks_in_bottom_frame(
        &self,
        id: BoxId,
        token: Option<&BreakToken>,
        containing_width: Au,
    ) -> bool {
        let Some(BreakToken::Rest { rest }) = token else {
            return false;
        };
        let model = self.box_model(&self.tree.get(id).style, containing_width, None);
        *rest <= model.padding[BOTTOM] + model.border[BOTTOM]
    }

    /// Notes where `justify-self` aligns absolutely positioned box `id`, block-level in a block
    /// container with `style` whose content box is `width` wide (see [`Self::block_statics`]).
    fn note_block_static(&self, style: &ComputedStyle, id: BoxId, width: Au) {
        let fraction = crate::block_justify::static_fraction(style, &self.tree.get(id).style);
        let mut statics = self.block_statics.borrow_mut();
        if fraction > 0.0 {
            statics.insert(id, (width, fraction));
        } else if !statics.is_empty() {
            statics.remove(&id);
        }
    }

    /// Moves the placeholders of absolutely positioned boxes that were block-level on `line` to
    /// `start`, the start edge of the lines (in the coordinates of the line's parent), and takes
    /// those that follow content out of it (the first list): their static position is at the
    /// start of the next line. The second list holds the others, which stay on the line.
    fn block_level_statics(&self, line: &mut Fragment, start: Au, rtl: bool) -> (Vec<BoxId>, Vec<BoxId>) {
        let mut out = Vec::new();
        let mut kept = Vec::new();
        if !line.children.iter().any(has_placeholder) {
            return (out, kept);
        }
        let mut content = Vec::new();
        line_content(line, Au::ZERO, rtl, &mut content);
        let start = start - line.rect.x;
        let height = line.rect.height;
        take_block_level(
            self.tree,
            line,
            ((Au::ZERO, Au::ZERO), height, false),
            (&content, rtl, start),
            (&mut out, &mut kept),
        );
        (out, kept)
    }

    /// The breaks between the lines of a flex or grid container: before line `index`, the
    /// `break-after` of the items of the line before joined with the `break-before` of its own.
    fn line_breaks(&self, lines: &[ContainerLine], items: &[Fragment], grid: bool) -> Vec<BreakBetween> {
        let join = |values: &mut dyn Iterator<Item = BreakBetween>| {
            values.fold(BreakBetween::Auto, crate::tree::combine_breaks)
        };
        // Of a group of grid rows, the items that start in its first row (with no other item
        // above them) break before it, and those that end in its last row (with none below them)
        // after it; the others break between its rows (see `Layouter::forced_rows`).
        let span = |line: &ContainerLine, index: usize| {
            items.get(line.items.start + index).map(|fragment| {
                let margins = line
                    .boxes
                    .get(index)
                    .map_or((Au::ZERO, Au::ZERO), |item| item.margins);
                (
                    fragment.rect.y - margins.0,
                    fragment.rect.y + fragment.rect.height + margins.1,
                )
            })
        };
        // The highest bottom and the lowest top of the items of a line: an item starts in the
        // first row where it starts above every bottom, and ends in the last where it ends below
        // every top.
        let extremes = |line: &ContainerLine| {
            (0..line.boxes.len()).filter_map(|index| span(line, index)).fold(
                None,
                |extremes: Option<(Au, Au)>, (top, bottom)| {
                    Some(extremes.map_or((bottom, top), |(highest, lowest)| {
                        (highest.min(bottom), lowest.max(top))
                    }))
                },
            )
        };
        let starts = |line: &ContainerLine, index: usize, extremes: Option<(Au, Au)>| {
            !grid
                || span(line, index)
                    .is_none_or(|(top, _)| extremes.is_none_or(|(highest_bottom, _)| highest_bottom > top))
        };
        let ends = |line: &ContainerLine, index: usize, extremes: Option<(Au, Au)>| {
            !grid
                || span(line, index)
                    .is_none_or(|(_, bottom)| extremes.is_none_or(|(_, lowest_top)| lowest_top < bottom))
        };
        (0..lines.len())
            .map(|index| {
                let line = &lines[index];
                let found = if grid { extremes(line) } else { None };
                let before = join(
                    &mut line
                        .boxes
                        .iter()
                        .enumerate()
                        .filter(|&(item, _)| starts(line, item, found))
                        .map(|(_, item)| {
                            crate::tree::combine_breaks(
                                self.tree.break_before(item.id),
                                self.passed_breaks(item).0,
                            )
                        }),
                );
                let after = match index.checked_sub(1) {
                    Some(previous) => {
                        let line = &lines[previous];
                        let found = if grid { extremes(line) } else { None };
                        join(
                            &mut line
                                .boxes
                                .iter()
                                .enumerate()
                                .filter(|&(item, _)| ends(line, item, found))
                                .map(|(_, item)| {
                                    crate::tree::combine_breaks(
                                        self.tree.break_after(item.id),
                                        self.passed_breaks(item).1,
                                    )
                                }),
                        )
                    }
                    None => BreakBetween::Auto,
                };
                let between = crate::tree::combine_breaks(after, before);
                // Items on pages of different names force a break between their lines (CSS
                // Paged Media 3, §9.2), as in Chrome.
                let above = index
                    .checked_sub(1)
                    .and_then(|previous| lines[previous].boxes.last());
                if above
                    .zip(line.boxes.first())
                    .is_some_and(|(above, below)| self.page_name_changes(above.id, below.id))
                {
                    crate::tree::combine_breaks(between, BreakBetween::Page)
                } else {
                    between
                }
            })
            .collect()
    }

    /// Lays out the part of a line of a flex or grid container that fits in `available`, its
    /// items laid out again where they break; `originals` are the fragments of the container's
    /// items in the unbroken layout. `at_top` tells whether the line starts the page, and with
    /// it whether nothing at all lies above it there (see [`Space::flush`]).
    fn split_container_line(
        &self,
        growth: crate::parallel::Growth,
        line: &ContainerLine,
        originals: &[Fragment],
        continued: Option<(&[Option<BreakToken>], Au)>,
        available: Au,
        (at_top, flush, kept, fixed_stretch): (bool, bool, bool, bool),
    ) -> Option<crate::parallel::Portion> {
        // A forced break between the rows of a group that items span across moves the rows
        // below it to the next page, the spanning items breaking there, as in Chrome.
        let start = continued.map_or(Au::ZERO, |(_, offset)| offset);
        let push_from = (growth == crate::parallel::Growth::Grid)
            .then(|| {
                self.forced_rows(line, originals)
                    .into_iter()
                    .find(|&row| row > start)
            })
            .flatten();
        let originals = &originals[line.items.clone()];
        let pieces: Vec<crate::parallel::Piece> = line
            .boxes
            .iter()
            .zip(originals)
            .map(|(item, fragment)| crate::parallel::Piece {
                offset: fragment.rect.y - line.top,
                height: fragment.rect.height,
                margin_bottom: item.margins.1,
                stretched: item.stretched,
                clones: self.tree.get(item.id).style.clones_decorations(),
                grows: item.grows,
                outset: (Au::ZERO, Au::ZERO),
                growth,
            })
            .collect();
        let mut boxes = LineItems {
            layouter: self,
            items: &line.boxes,
            originals,
            flush,
            kept,
            fixed_stretch,
        };
        let portion = crate::parallel::split_line_from(
            &pieces,
            line.bottom - line.top,
            continued,
            available,
            at_top,
            &mut boxes,
            push_from,
        )?;
        if push_from.is_some_and(|row| row - start <= available) && portion.next.is_some() {
            self.forced_break.set(true);
        }
        Some(portion)
    }

    /// Whether `portion` of a flex or grid line breaks inside an item that avoids breaks inside
    /// it (which it does where the break before the line is avoided too).
    fn breaks_avoiding_item(&self, line: &ContainerLine, portion: &crate::parallel::Portion) -> bool {
        portion.next.as_ref().is_some_and(|(tokens, _)| {
            tokens.iter().zip(&line.boxes).any(|(token, item)| {
                token.is_some() && self.avoids_breaks_inside(self.tree.get(item.id).style.break_inside)
            })
        })
    }

    /// The places in a grid line (a group of rows that items span across) where forced breaks
    /// fall between its rows, from the top of the line: above an item with a forced
    /// `break-before`, and above the first row below an item with a forced `break-after`.
    fn forced_rows(&self, line: &ContainerLine, originals: &[Fragment]) -> Vec<Au> {
        if !line.boxes.iter().any(|item| {
            self.forces_break(self.tree.break_before(item.id))
                || self.forces_break(self.tree.break_after(item.id))
        }) {
            return Vec::new();
        }
        let items: Vec<(Au, Au, BoxId)> = line
            .boxes
            .iter()
            .zip(&originals[line.items.clone()])
            .map(|(item, fragment)| {
                (
                    fragment.rect.y - line.top,
                    fragment.rect.height + item.margins.1,
                    item.id,
                )
            })
            .collect();
        let mut rows: Vec<Au> = Vec::new();
        for &(offset, height, id) in &items {
            if offset > Au::ZERO && self.forces_break(self.tree.break_before(id)) {
                rows.push(offset);
            }
            if self.forces_break(self.tree.break_after(id))
                && let Some(next) = items
                    .iter()
                    .map(|&(other, ..)| other)
                    .filter(|&other| other >= offset + height)
                    .min()
            {
                rows.push(next);
            }
        }
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Lays out a flex or grid item across pages: the part of it that fits in `remaining` below
    /// the top of its margin box, from `token` on, with a fixed content `width` if given and,
    /// for a block container, the border-box `height` it is stretched to. Boxes that do not
    /// break (replaced elements, `break-inside: avoid`) fit as a whole or not at all, unless
    /// they are at the very top of the page (`at_top` and `flush`, see [`Space::flush`]).
    pub(crate) fn item_part(
        &self,
        item: &LineItem,
        remaining: Au,
        (at_top, flush): (bool, bool),
        token: Option<&BreakToken>,
        (height, full): (Option<Au>, Option<Au>),
    ) -> Option<Outcome> {
        let layout_box = self.tree.get(item.id);
        let style = &layout_box.style;
        let space = Space {
            remaining: Some(remaining),
            at_top,
            flush,
            containing_height: item.containing_height,
            ..Space::unfragmented(item.containing_width)
        };
        let whole = || {
            let outcome = self.item_in(
                item.id,
                (item.containing_width, item.containing_height),
                item.width,
                None,
            )?;
            (self.at_page_top(&space) || item.margins.0 + outcome.fragment.rect.height <= remaining)
                .then_some(outcome)
        };
        // Boxes with size containment or `line-clamp` are monolithic (see `block_level`).
        let monolithic =
            (style.size_containment().0 && style.size_containment().1) || style.line_clamp.is_some();
        let avoids = self.avoids_breaks_inside(style.break_inside) && !self.breaks_avoided_items.get();
        if token.is_none() && (avoids || style.is_multicol() || monolithic) {
            // An item that avoids breaks inside but is too tall for the page breaks all the same
            // at the top of one (below the top border and padding of its container too), as a
            // block-level box does, as in Chrome.
            if !at_top || style.is_multicol() || monolithic {
                return whole();
            }
            let outcome = self.item_in(
                item.id,
                (item.containing_width, item.containing_height),
                item.width,
                None,
            )?;
            if item.margins.0 + outcome.fragment.rect.height <= remaining {
                return Some(outcome);
            }
        }
        match &layout_box.kind {
            BoxKind::Block { .. } | BoxKind::Inline { .. } => {
                self.block_container_with(item.id, space, token, item.width, height, true)
            }
            BoxKind::Table { .. } => {
                let width = item.width.map_or(item.containing_width, |content| {
                    self.table_outer_width(style, item.containing_width, content)
                });
                self.table_fills.set(item.width.is_some());
                self.table(
                    item.id,
                    Space {
                        containing_width: width,
                        ..space
                    },
                    token,
                )
            }
            // A column flex container stretched in a row keeps its items where its whole
            // stretched height puts them (`justify-content`) and breaks between them there, as
            // in Chrome.
            BoxKind::Flex { .. }
                if full.is_some()
                    && matches!(
                        style.flex_direction,
                        FlexDirection::Column | FlexDirection::ColumnReverse
                    )
                    && !self.sized_column(item.id, &space) =>
            {
                self.flex_column_part(item.id, space, token, item.width, full)
            }
            // An item of a flex row keeps the height it is stretched to for its own items (a flex
            // container in a grid item, whose area gives it a containing block height, does not
            // need to; a grid does, for its rows to fill it, unless breaks are forced inside it).
            BoxKind::Flex { .. } | BoxKind::Grid { .. } => self.flex_or_grid_with(
                item.id,
                space,
                token,
                item.width,
                full.filter(|_| {
                    item.containing_height.is_none()
                        || matches!(layout_box.kind, BoxKind::Grid { .. })
                            && !self.forces_breaks_below(item.id)
                }),
            ),
            _ => whole(),
        }
    }

    /// The min-content and max-content widths of flex item `child` (as [`Layouter::intrinsic`])
    /// whose content height is `cross` where that is definite: the percentage heights inside a
    /// stretched item refer to it, and its aspect ratio (a replaced element's too) turns it into
    /// a width (CSS Flexbox 1, §9.8). So does a percentage height of the item, which refers to
    /// the container's. A box with a fixed height keeps its widths.
    pub(crate) fn stretched_intrinsic(&self, child: BoxId, cross: Option<Au>) -> (Au, Au) {
        let style = &self.tree.get(child).style;
        let percentage = matches!(&style.height, Size::Length(length) if length.fixed().is_none());
        if !self.tree.is_replaced(child)
            && percentage
            && let Some(cross) = cross
        {
            return self.intrinsic_at_height(child, Some(cross));
        }
        let Some(cross) = cross.filter(|_| matches!(style.height, Size::Auto)) else {
            return self.intrinsic(child);
        };
        if !self.tree.is_replaced(child) {
            return self.intrinsic_at_height(child, Some(cross));
        }
        let (width, height) = self.replaced_size(child, Au::ZERO, None, None, true);
        let ratio = crate::ratio::ratio(style)
            .or_else(|| (width > Au::ZERO && height > Au::ZERO).then(|| width.to_px() / height.to_px()));
        let Some(ratio) = ratio.filter(|_| matches!(style.width, Size::Auto)) else {
            return self.intrinsic(child);
        };
        let model = self.box_model(style, Au::ZERO, Some(Au::ZERO));
        let outer = Au::from_px(cross.to_px() * ratio)
            + model.border[LEFT]
            + model.border[RIGHT]
            + model.padding[LEFT]
            + model.padding[RIGHT]
            + model.margin[LEFT]
            + model.margin[RIGHT];
        (outer, outer)
    }

    /// The width of a column flex item that does not stretch (`align-self` start, end or center
    /// with an automatic width): its max-content width, at most the container's. A replaced
    /// element sizes itself.
    /// The content height of a flex row's item if it is definite: a fixed height, a percentage
    /// of the container's definite height (`basis`), or the height of the container's single
    /// line (`line`) that the item stretches to.
    fn definite_cross(
        &self,
        container: &ComputedStyle,
        child: BoxId,
        width: Au,
        (line, basis): (Option<Au>, Option<Au>),
    ) -> Option<Au> {
        let style = &self.tree.get(child).style;
        let model = self.box_model(style, width, Some(Au::ZERO));
        let frame = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        match &style.height {
            // A percentage of the container's definite height.
            Size::Length(length) => length
                .fixed()
                .or_else(|| basis.map(|basis| length.resolve(basis.to_px())))
                .map(|px| {
                    let height = Au::from_px(px);
                    if style.box_sizing == BoxSizing::BorderBox {
                        (height - frame).clamp_non_negative()
                    } else {
                        height
                    }
                }),
            Size::Auto
                if matches!(
                    item_alignment(container, style),
                    ItemAlignment::Stretch | ItemAlignment::Normal
                ) && !style.margin.top.is_auto()
                    && !style.margin.bottom.is_auto() =>
            {
                line.map(|line| {
                    let margins =
                        self.vertical_margin(style, width, true) + self.vertical_margin(style, width, false);
                    (line - margins - frame).clamp_non_negative()
                })
            }
            // `height: stretch` fills the line, or the container's definite height, whatever the
            // alignment (WPT `css-sizing/stretch/flexbox-auto-minimum-002`).
            Size::Stretch => line.or(basis).map(|room| {
                let margins =
                    self.vertical_margin(style, width, true) + self.vertical_margin(style, width, false);
                (room - margins - frame).clamp_non_negative()
            }),
            _ => None,
        }
    }

    /// Where the border box of a column flex item `border_width` wide starts across the
    /// container's content box `width` wide: auto margins take the free room first, and
    /// `align-self` places the item where it has none (CSS Flexbox 1, §8.1, §8.3). Cross-start
    /// is on the right in right-to-left text.
    pub(crate) fn cross_offset(
        &self,
        container: &ComputedStyle,
        child: BoxId,
        width: Au,
        border_width: Au,
    ) -> Au {
        let style = &self.tree.get(child).style;
        let (left, right) = self.flex_margins(style, width);
        let (left_auto, right_auto) = (style.margin.left.is_auto(), style.margin.right.is_auto());
        let rtl = container.is_rtl();
        let (start, start_auto, end_auto) = if rtl {
            (right, right_auto, left_auto)
        } else {
            (left, left_auto, right_auto)
        };
        let free = width - border_width - left - right;
        let offset = start
            + match (start_auto, end_auto) {
                (true, true) => free.clamp_non_negative() / 2,
                (true, false) => free.clamp_non_negative(),
                (false, true) => Au::ZERO,
                (false, false) => match column_alignment(container, style, free) {
                    ItemAlignment::Center => free / 2,
                    ItemAlignment::End => free,
                    _ => Au::ZERO,
                },
            };
        if rtl {
            width - offset - border_width
        } else {
            offset
        }
    }

    pub(crate) fn cross_size(&self, container: &ComputedStyle, child: BoxId, width: Au) -> Option<Au> {
        let child_box = self.tree.get(child);
        let child_style = &child_box.style;
        let alignment = item_alignment(container, child_style);
        let replaced = matches!(child_box.kind, BoxKind::Replaced { .. });
        if !matches!(child_style.width, Size::Auto) {
            return None;
        }
        let model = self.box_model(child_style, width, Some(Au::ZERO));
        let frame = model.border[LEFT] + model.border[RIGHT] + model.padding[LEFT] + model.padding[RIGHT];
        let margins = Au::from_px(
            child_style.margin.left.resolve_or(width.to_px(), 0.0)
                + child_style.margin.right.resolve_or(width.to_px(), 0.0),
        );
        // A replaced item stretches like any other, its height following its ratio (as in
        // Chrome), and so do a table and a button, which are otherwise as wide as their content.
        let stretches = matches!(alignment, ItemAlignment::Stretch | ItemAlignment::Normal)
            && !child_style.margin.left.is_auto()
            && !child_style.margin.right.is_auto();
        let fits_content =
            matches!(child_box.kind, BoxKind::Table { .. }) || self.tree.shrinks_to_fit.contains(&child);
        if replaced || (fits_content && stretches) {
            return stretches.then(|| {
                let content = (width - frame - margins).clamp_non_negative();
                self.clamped_width(child, content, width, width)
            });
        }
        // An item with an auto margin across the column does not stretch either (CSS Flexbox 1,
        // §8.1).
        let shrink = matches!(
            alignment,
            ItemAlignment::Start | ItemAlignment::End | ItemAlignment::Center
        ) || child_style.margin.left.is_auto()
            || child_style.margin.right.is_auto();
        shrink.then(|| {
            let (min, max) = self.intrinsic(child);
            (max.min(width.max(min)) - frame - margins).clamp_non_negative()
        })
    }

    /// Where a box that must not overlap floats (a new formatting context, a table, a replaced
    /// element) goes in a containing block `width` wide whose content box starts at `origin` in
    /// the formatting context: the first place from there down where its border box fits
    /// beside the floats over `height`, with the containing width it takes there and how far
    /// that starts from the content box's left edge. As in Blink's
    /// `LayoutNewFormattingContext`, its margins may overlap the floats, and a box whose width
    /// fills the room (an auto width) fits anywhere. `rtl` tells whether the containing block is
    /// right to left.
    fn beside_floats(
        &self,
        child: BoxId,
        width: Au,
        origin: (Au, Au),
        height: Au,
        rtl: bool,
    ) -> (Au, Au, Au) {
        let style = &self.tree.get(child).style;
        let basis = width.to_px();
        let margin = |margin: &LengthPercentageAuto| Au::from_px(margin.resolve_or(basis, 0.0));
        let (margin_left, margin_right) = (margin(&style.margin.left), margin(&style.margin.right));
        let (left_edge, mut top) = origin;
        let right_edge = left_edge + width;
        loop {
            let (left, right) = self.free_range(left_edge, right_edge, top, height);
            let next = self.next_float_edge(top);
            if left <= left_edge && right >= right_edge || next.is_none() {
                // No float takes room here: the box has the whole width.
                return (top, Au::ZERO, width);
            }
            let line_left = left.max(left_edge + margin_left.clamp_non_negative());
            let line_right = right.min(right_edge - margin_right.clamp_non_negative());
            let room = (line_right - line_left).clamp_non_negative();
            let available = (room + margin_left + margin_right).clamp_non_negative();
            let used =
                self.border_width_beside_floats(child, (width, available), margin_left + margin_right, room);
            // Its border box, from the start edge of the room, ends before the floats on the
            // other side; a margin may reach past them, and the box past the containing block's
            // edges (as in Blink, where the box starts at the line-right edge in a right-to-left
            // containing block).
            let fits_end = if rtl {
                left <= left_edge || line_right - used >= left
            } else {
                right >= right_edge || line_left + used <= right
            };
            // (Where the floats on both sides overlap, not even a box of no width fits, as
            // in Blink: WPT `contain-inline-size-bfc-floats-001`.)
            if used <= right - left && fits_end && !self.floats_close_band(left_edge, right_edge, top, height)
            {
                return (top, line_left - margin_left - left_edge, available);
            }
            top = next.unwrap_or(top);
        }
    }

    /// The width of the border box of a box that avoids floats in a containing block `width`
    /// wide, laid out `available` wide with `margins` across, `room` of it beside the floats.
    /// A percentage width refers to the containing block, as in Blink.
    fn border_width_beside_floats(
        &self,
        child: BoxId,
        (width, available): (Au, Au),
        margins: Au,
        room: Au,
    ) -> Au {
        let child_box = self.tree.get(child);
        let style = &child_box.style;
        let intrinsic = || {
            let (min, max) = self.intrinsic(child);
            (
                (min - margins).clamp_non_negative(),
                (max - margins).clamp_non_negative(),
            )
        };
        let table = matches!(child_box.kind, BoxKind::Table { .. });
        match &style.width {
            Size::MinContent => intrinsic().0,
            Size::MaxContent => intrinsic().1,
            Size::FitContent(_) => {
                let (min, max) = intrinsic();
                max.min(room).max(min)
            }
            Size::Auto if table => {
                let (min, max) = intrinsic();
                max.min(room).max(min)
            }
            Size::Auto if matches!(child_box.kind, BoxKind::Replaced { .. }) => intrinsic().0,
            // A box whose content runs across its lines is as wide as that content makes it.
            Size::Auto if self.is_orthogonal(child) => intrinsic().1,
            _ => {
                // `stretch` as a minimum or maximum width fills the room beside the floats, as
                // the width `stretch` does (WPT `css-sizing/stretch/bfc-next-to-float-2`).
                let basis = if matches!(style.width, Size::Length(_)) {
                    width
                } else {
                    available
                };
                let model = self.box_model(style, basis, None);
                let mut border_box = model.content_width
                    + model.border[LEFT]
                    + model.border[RIGHT]
                    + model.padding[LEFT]
                    + model.padding[RIGHT];
                if matches!(style.max_width, MaxSize::Stretch) {
                    border_box = border_box.min(room);
                }
                if matches!(style.min_width, Size::Stretch) {
                    border_box = border_box.max(room);
                }
                if table {
                    border_box.max(intrinsic().0)
                } else {
                    border_box
                }
            }
        }
    }

    /// The top margin of a box collapsed with those of its first in-flow descendants that adjoin
    /// it (CSS 2.1 §8.3.1), as far as their styles tell: where its border box starts.
    ///
    /// `origin` is where the margins above the box start in the formatting context, with the
    /// margin they collapse to. A descendant that clears floats has no clearance where its
    /// margins, collapsed with all those above it, take its border box past the floats (its
    /// hypothetical position, CSS 2.1 §9.5.2): its margin then collapses with ours, as in Chrome.
    fn leading_margin(&self, id: BoxId, containing_width: Au, origin: (Au, Margin)) -> Margin {
        self.leading_margin_beside(id, containing_width, origin, Au::ZERO)
    }

    /// [`Self::leading_margin`] of a box after floats `floats` wide that come before it in the
    /// boxes whose margins collapse with its own: those floats go where its margin takes them.
    /// A box that must not overlap floats and has no room beside them there has its margin
    /// separated from those above it and moves below the floats instead, as in Chrome (Blink's
    /// `abort_if_cleared` in `LayoutNewFormattingContext`): its margin does not count.
    fn leading_margin_beside(
        &self,
        id: BoxId,
        containing_width: Au,
        origin: (Au, Margin),
        floats: Au,
    ) -> Margin {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let basis = containing_width.to_px();
        let own = Margin::of(Au::from_px(style.margin.top.resolve_or(basis, 0.0)));
        if floats > Au::ZERO
            && own.resolve() != Au::ZERO
            && self
                .width_beside_floats(id, containing_width)
                .is_some_and(|width| width + floats > containing_width)
        {
            return Margin::default();
        }
        let adjoins = style.border_width.top == 0.0
            && style.padding.top.resolve(basis) == 0.0
            && !establishes_formatting_context(style);
        let BoxKind::Block { children } = &layout_box.kind else {
            return own;
        };
        if !adjoins {
            return own;
        }
        let top = origin.0;
        let mut collapsed = own;
        let mut floats = floats;
        // Floats before the descendant, not yet placed, are cleared where they would go.
        let mut floats_before = false;
        for child in self.in_flow(children) {
            // A spanner of the columns being laid out ends their content: its margin stays apart.
            if self
                .spanning
                .get()
                .is_some_and(|flow| self.tree.spanned_flow(child) == Some(flow))
            {
                return collapsed;
            }
            if self.tree.get(child).style.is_floating() {
                floats_before = true;
                floats += self
                    .fixed_width(child)
                    .unwrap_or_else(|| self.intrinsic(child).1.min(containing_width));
                continue;
            }
            let above = origin.1.adjoin(collapsed);
            let below = self.leading_margin_beside(child, containing_width, (top, above), floats);
            let clear = self.tree.get(child).style.clear;
            let collapses = clear == fagus_style::values::Clear::None
                || !floats_before
                    && self
                        .clearance(clear)
                        .is_none_or(|bottom| bottom <= top + above.adjoin(below).resolve());
            if !collapses {
                return collapsed;
            }
            collapsed = collapsed.adjoin(below);
            // The margins of an empty box collapse through it with those of the box after it
            // (CSS 2.1 §8.3.1), which then also start where this box's border box does.
            // Floats inside it come before the boxes after it.
            match self.margins_through(child, containing_width) {
                Some((through, inside)) if clear == fagus_style::values::Clear::None => {
                    collapsed = collapsed.adjoin(through);
                    if let Some(inside) = inside {
                        floats_before = true;
                        floats += inside;
                    }
                }
                _ => return collapsed,
            }
        }
        collapsed
    }

    /// All the margins of box `id` and its descendants collapsed together, where its styles
    /// tell that it is empty and its margins collapse through it (CSS 2.1 §8.3.1): no height,
    /// border, padding or content but floats and positioned boxes, and no new formatting context;
    /// `None` for any other box. With them, how wide the floats inside it are together, if it
    /// holds any.
    fn margins_through(&self, id: BoxId, containing_width: Au) -> Option<(Margin, Option<Au>)> {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let basis = containing_width.to_px();
        let zero = |value: &LengthPercentage| value.fixed() == Some(0.0);
        let no_height = |size: &Size| match size {
            Size::Auto => true,
            Size::Length(length) => length.fixed() == Some(0.0),
            _ => false,
        };
        if style.border_width.top != 0.0
            || style.border_width.bottom != 0.0
            || !zero(&style.padding.top)
            || !zero(&style.padding.bottom)
            || !no_height(&style.height)
            || !no_height(&style.min_height)
            || establishes_formatting_context(style)
            || style.is_vertical()
        {
            return None;
        }
        let mut margins = Margin::of(Au::from_px(style.margin.top.resolve_or(basis, 0.0))).adjoin(
            Margin::of(Au::from_px(style.margin.bottom.resolve_or(basis, 0.0))),
        );
        let mut floats: Option<Au> = None;
        let float_width = |float: BoxId| {
            self.fixed_width(float)
                .unwrap_or_else(|| self.intrinsic(float).1.min(containing_width))
        };
        match &layout_box.kind {
            BoxKind::Block { children } => {
                for child in self.in_flow(children) {
                    if self.tree.get(child).style.is_floating() {
                        floats = Some(floats.unwrap_or(Au::ZERO) + float_width(child));
                    } else {
                        let (inner, inside) = self.margins_through(child, containing_width)?;
                        margins = margins.adjoin(inner);
                        if let Some(inside) = inside {
                            floats = Some(floats.unwrap_or(Au::ZERO) + inside);
                        }
                    }
                }
            }
            BoxKind::Inline { items } => {
                for item in items {
                    if let crate::tree::InlineItem::Float(float) = item {
                        floats = Some(floats.unwrap_or(Au::ZERO) + float_width(*float));
                    }
                }
                let empty = items.iter().all(|item| match item {
                    crate::tree::InlineItem::OutOfFlow(_)
                    | crate::tree::InlineItem::Float(_)
                    | crate::tree::InlineItem::Close => true,
                    crate::tree::InlineItem::Open { style, .. } => {
                        style.margin.left.resolve_or(basis, 0.0) == 0.0
                            && style.margin.right.resolve_or(basis, 0.0) == 0.0
                            && style.border_width.left == 0.0
                            && style.border_width.right == 0.0
                            && zero(&style.padding.left)
                            && zero(&style.padding.right)
                    }
                    _ => false,
                });
                if !empty {
                    return None;
                }
            }
            _ => return None,
        }
        Some((margins, floats))
    }

    /// The width of the border box of a box that must not overlap floats (a new formatting
    /// context, a table), where it does not take the room it is given: `None` for a box of an
    /// automatic width, which fits beside any floats.
    fn width_beside_floats(&self, id: BoxId, containing_width: Au) -> Option<Au> {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let table = matches!(layout_box.kind, BoxKind::Table { .. });
        if !table && !establishes_formatting_context(style) {
            return None;
        }
        match &style.width {
            Size::Length(length) => {
                let width = Au::from_px(length.resolve(containing_width.to_px()));
                if table {
                    return Some(self.table_fixed_width(id, width));
                }
                let model = self.box_model(style, containing_width, None);
                Some(if style.box_sizing == BoxSizing::BorderBox {
                    width
                } else {
                    width
                        + model.border[LEFT]
                        + model.border[RIGHT]
                        + model.padding[LEFT]
                        + model.padding[RIGHT]
                })
            }
            Size::Auto if table => Some(self.table_intrinsic(id).0),
            _ => None,
        }
    }

    /// The page name a box starts on: that of its first in-flow child, recursively (its start
    /// page value, CSS Paged Media 3, §9.2).
    pub(crate) fn start_page(&self, id: BoxId) -> Option<&Arc<str>> {
        self.edge_page(id, true)
    }

    /// The page name a box ends on: that of its last in-flow child, recursively.
    pub(crate) fn end_page(&self, id: BoxId) -> Option<&Arc<str>> {
        self.edge_page(id, false)
    }

    /// Whether box `below` starts on a page of another name than box `above`, which comes before
    /// it, ends on: a forced break between them (CSS Paged Media 3, §9.2). Not in columns, which
    /// are no pages, as in Chrome.
    pub(crate) fn page_name_changes(&self, above: BoxId, below: BoxId) -> bool {
        self.tree.names_pages()
            && !self.measuring.get()
            && self.columns.get() == 0
            && self.end_page(above) != self.start_page(below)
    }

    fn edge_page(&self, id: BoxId, start: bool) -> Option<&Arc<str>> {
        // Flex items pass theirs on to their container as block children do, as in Chrome.
        if let BoxKind::Block { children } | BoxKind::Flex { children } = &self.tree.get(id).kind {
            let mut in_flow = self.in_flow(children);
            let child = if start { in_flow.next() } else { in_flow.last() };
            if let Some(child) = child {
                return self.edge_page(child, start);
            }
        }
        self.tree.page_name(id)
    }

    /// The boxes on the page on which layout of box `id` resumes at `token`, from `id` down:
    /// those that go on from the page before (`continued`) and those that start at the top of
    /// the page (`started`), a box and its first in-flow children.
    pub(crate) fn boxes_at_page_start(
        &self,
        id: BoxId,
        token: Option<&BreakToken>,
        continued: &mut Vec<BoxId>,
        started: &mut Vec<BoxId>,
    ) {
        let Some(token) = token else {
            let mut id = id;
            loop {
                started.push(id);
                let (BoxKind::Block { children } | BoxKind::Flex { children }) = &self.tree.get(id).kind
                else {
                    return;
                };
                match self.in_flow(children).next() {
                    Some(child) => id = child,
                    None => return,
                }
            }
        };
        continued.push(id);
        if let (BoxKind::Block { children }, BreakToken::Block { index, inner, .. }) =
            (&self.tree.get(id).kind, token)
            && let Some(&child) = children.get(*index)
        {
            self.boxes_at_page_start(child, inner.as_deref(), continued, started);
        }
    }

    /// The name of the page on which layout of box `id` resumes at `token`.
    pub(crate) fn page_name_at(&self, id: BoxId, token: Option<&BreakToken>) -> Option<&Arc<str>> {
        let Some(token) = token else {
            return self.start_page(id);
        };
        if let (BoxKind::Block { children }, BreakToken::Block { index, inner, .. }) =
            (&self.tree.get(id).kind, token)
            && let Some(&child) = children.get(*index)
        {
            return self.page_name_at(child, inner.as_deref());
        }
        self.tree.page_name(id)
    }

    /// The min-content and max-content widths of a box's border box plus horizontal margins.
    pub fn intrinsic(&self, id: BoxId) -> (Au, Au) {
        let given_height = self.has_given_height(id);
        self.forget_page_widths();
        if !given_height && let Some(cached) = self.intrinsic_cache.borrow().get(&id) {
            return *cached;
        }
        let widths = self.intrinsic_uncached(id);
        // Those of a replaced element or box with an aspect ratio with a percentage height
        // depend on the box it is in.
        if !self.percentage_replaced(id) && !self.ratio_percentage(id) && !given_height {
            self.intrinsic_cache.borrow_mut().insert(id, widths);
        }
        widths
    }

    /// The min-content and max-content widths of box `id` (as [`Layouter::intrinsic`]) where
    /// its content height is `height`, whatever its own height says: percentage heights of
    /// replaced elements in it refer to that. For a replaced element, `height` is what its own
    /// percentage heights refer to.
    pub(crate) fn intrinsic_at_height(&self, id: BoxId, height: Option<Au>) -> (Au, Au) {
        let outer = self.height_for_widths.replace(Some((id, height)));
        let widths = self.intrinsic(id);
        self.height_for_widths.set(outer);
        widths
    }

    /// Whether [`Layouter::intrinsic_at_height`] gives box `id` a height.
    pub(crate) fn has_given_height(&self, id: BoxId) -> bool {
        self.height_for_widths.get().is_some_and(|(given, _)| given == id)
    }

    /// The height [`Layouter::intrinsic_at_height`] gives box `id` (see
    /// [`Layouter::has_given_height`]).
    fn given_height(&self) -> Option<Au> {
        self.height_for_widths.get().and_then(|(_, height)| height)
    }

    /// Whether box `id` is a replaced element whose height, minimum or maximum height is a
    /// percentage.
    pub(crate) fn percentage_replaced(&self, id: BoxId) -> bool {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let percentage = |length: &LengthPercentage| length.fixed().is_none();
        matches!(layout_box.kind, BoxKind::Replaced { .. })
            && (matches!(&style.height, Size::Length(length) if percentage(length))
                || matches!(&style.min_height, Size::Length(length) if percentage(length))
                || matches!(&style.max_height, MaxSize::Length(length) if percentage(length)))
    }

    fn intrinsic_uncached(&self, id: BoxId) -> (Au, Au) {
        if let Some(column) = self.tree.ruby_columns.get(&id) {
            let width = crate::ruby::intrinsic(self, id, column, 0.0);
            return (width, width);
        }
        if let Some(widths) = self.keyword_contributions(id) {
            return widths;
        }
        match self.fixed_width(id) {
            Some(width) if self.compressible(id) => {
                // A replaced element or control with a percentage maximum may shrink to
                // nothing, whatever its width (`<svg width=330>` under `max-width: 100%`), as in
                // Chrome (CSS Sizing 3, §5.2.2): only its frame is left of its min-content.
                let style = &self.tree.get(id).style;
                let model = self.box_model(style, Au::ZERO, None);
                let frame = model.border[LEFT]
                    + model.border[RIGHT]
                    + model.padding[LEFT]
                    + model.padding[RIGHT]
                    + model.margin[LEFT]
                    + model.margin[RIGHT];
                (frame.min(width), width)
            }
            Some(width) => (width, width),
            None => self.content_widths(id),
        }
    }

    /// Whether box `id` is a replaced element or form control whose `max-width` is a percentage.
    fn compressible(&self, id: BoxId) -> bool {
        let layout_box = self.tree.get(id);
        (matches!(layout_box.kind, BoxKind::Replaced { .. }) || self.tree.control_widths.contains_key(&id))
            && matches!(&layout_box.style.max_width, MaxSize::Length(length) if length.fixed().is_none())
    }

    /// The width of a box's border box plus horizontal margins, if its `width` is fixed.
    pub(crate) fn fixed_width(&self, id: BoxId) -> Option<Au> {
        let style = &self.tree.get(id).style;
        let Size::Length(length) = &style.width else {
            return None;
        };
        let width = Au::from_px(length.fixed()?);
        let model = self.box_model(style, Au::ZERO, None);
        let margins = model.margin[LEFT] + model.margin[RIGHT];
        if matches!(self.tree.get(id).kind, BoxKind::Table { .. }) {
            return Some(self.table_fixed_width(id, width) + margins);
        }
        Some(if style.box_sizing == BoxSizing::BorderBox {
            width + margins
        } else {
            width
                + model.border[LEFT]
                + model.border[RIGHT]
                + model.padding[LEFT]
                + model.padding[RIGHT]
                + margins
        })
    }

    /// The min-content and max-content widths of a box's border box plus horizontal margins,
    /// from its content, whatever its `width`.
    pub(crate) fn content_widths(&self, id: BoxId) -> (Au, Au) {
        let layout_box = self.tree.get(id);
        // The percentage heights of the replaced elements inside refer to the box's height,
        // where it is fixed; an anonymous box passes on that of the box it is in.
        if (layout_box.node.is_none() && !layout_box.generated)
            || matches!(layout_box.kind, BoxKind::Replaced { .. })
        {
            // A replaced element given the height of its container refers to it.
            if self.has_given_height(id) {
                let outer = self.intrinsic_height.replace(self.given_height());
                let widths = self.own_content_widths(id);
                self.intrinsic_height.set(outer);
                return widths;
            }
            return self.own_content_widths(id);
        }
        let model = self.box_model(&layout_box.style, Au::ZERO, None);
        // A percentage height refers to the height of the box this one is in, where that is
        // definite.
        let outer = self.intrinsic_height.get();
        // In quirks mode, a box of an automatic height passes that of its containing block on.
        let height = if self.has_given_height(id) {
            self.given_height()
        } else {
            Self::definite_height(&layout_box.style, &model, outer, None)
                .or_else(|| outer.filter(|_| self.passes_percentages_on(&layout_box.style)))
        };
        self.intrinsic_height.set(height);
        let widths = self.own_content_widths(id);
        self.intrinsic_height.set(outer);
        // A box whose height gives its width through an aspect ratio is as wide, also the
        // content height it is given (a stretched flex item's).
        let given = if self.has_given_height(id) { height } else { None };
        self.ratio_widths(id, (outer, given), widths)
    }

    /// [`Layouter::content_widths`], with the percentage heights of replaced elements referring
    /// to [`Layouter::intrinsic_height`].
    fn own_content_widths(&self, id: BoxId) -> (Au, Au) {
        if let Some(inner) = self.tree.flows.content_of(id)
            && let Some(widths) = self.orthogonal_widths(id, inner)
        {
            return widths;
        }
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let model = self.box_model(style, Au::ZERO, None);
        let frame = model.border[LEFT]
            + model.border[RIGHT]
            + model.padding[LEFT]
            + model.padding[RIGHT]
            + model.margin[LEFT]
            + model.margin[RIGHT];
        // A text field or select is as wide as its `size` or longest option, whatever text it
        // shows, and its width stays auto: it fits that width in block flow and stretches in
        // flex and grid layout, as in Chrome. With a percentage width or maximum it is
        // compressible, like a replaced element: it may shrink to nothing.
        if !matches!(layout_box.kind, BoxKind::Replaced { .. })
            && let Some(&width) = self.tree.control_widths.get(&id)
        {
            let percentage = |length: &LengthPercentage| length.fixed().is_none();
            let compressible = matches!(&style.width, Size::Length(length) if percentage(length))
                || matches!(&style.max_width, MaxSize::Length(length) if percentage(length));
            let width = Au::from_px(width);
            return (if compressible { Au::ZERO } else { width } + frame, width + frame);
        }
        // Size containment: the box is as wide as if it were empty (CSS Containment 2, §3.1).
        if style.size_containment().0
            && !matches!(
                layout_box.kind,
                BoxKind::Table { .. }
                    | BoxKind::Replaced { .. }
                    | BoxKind::RowGroup { .. }
                    | BoxKind::Row { .. }
            )
        {
            let width = Au::from_px(style.contain_intrinsic_size.0.unwrap_or(0.0));
            // A grid keeps the columns of its template, sized without its items.
            if let BoxKind::Grid { .. } = layout_box.kind
                && style.contain_intrinsic_size.0.is_none()
            {
                let (min, max) = self.grid_intrinsic(id, style, &[]);
                return (min + frame, max + frame);
            }
            // A multi-column container still takes its columns (of its `column-width`) and the
            // gaps between them, as in Blink (WPT `contain-size-multicol-001` and `-002`).
            if style.is_multicol() {
                let (min, max) = multicol_widths(style, (width, width));
                return (min + frame, max + frame);
            }
            return (width + frame, width + frame);
        }
        let (min, max) = match &layout_box.kind {
            // Columns side by side, each at least as wide as the content's widest part, and
            // the spanners across them (CSS Multi-column Layout 1, §3.4), as Blink sizes them:
            // with a `column-width`, the min-content width is no wider than it and takes no
            // column count, and the max-content width is one column of it where the count is
            // `auto`.
            BoxKind::Block { children } if style.is_multicol() && children.len() == 1 => {
                let (mut min, mut max) = multicol_widths(style, self.intrinsic(children[0]));
                for spanner in self.tree.spanners_of(children[0]) {
                    let (spanner_min, spanner_max) = self.intrinsic(spanner);
                    min = min.max(spanner_min);
                    max = max.max(spanner_max);
                }
                (min, max)
            }
            BoxKind::Inline { items } => {
                let indent = style.text_indent.resolve(0.0);
                let (min, max) = intrinsic_widths_in(
                    self,
                    (items, Some(style)),
                    (Some(style.white_space.wraps()), indent),
                );
                (Au::from_px_ceil(min), Au::from_px_ceil(max))
            }
            BoxKind::Block { children } => {
                // Floats side by side add up, and the content after them sits beside them.
                let mut floats = Au::ZERO;
                let (mut min, mut max) = (Au::ZERO, Au::ZERO);
                // Spanners are as wide as the multi-column container, not a column: they size
                // it on their own (see above).
                for child in self
                    .in_flow(children)
                    .filter(|&child| self.tree.spanned_flow(child).is_none())
                {
                    let child_style = &self.tree.get(child).style;
                    let (child_min, child_max) = self.intrinsic(child);
                    min = min.max(child_min);
                    if child_style.clear != fagus_style::values::Clear::None {
                        floats = Au::ZERO;
                    }
                    if child_style.is_floating() {
                        floats += child_max;
                        max = max.max(floats);
                    } else {
                        // Floats whose negative margins make them narrower than nothing together
                        // take no room beside the content after them (as in Chrome).
                        max = max.max(floats.max(Au::ZERO) + child_max);
                        floats = Au::ZERO;
                    }
                }
                (min, max)
            }
            BoxKind::Grid { children } => {
                let children: Vec<BoxId> = self.in_flow(children).collect();
                self.grid_intrinsic(id, style, &children)
            }
            BoxKind::Flex { children } => {
                let children: Vec<BoxId> = self.in_flow(children).collect();
                let row = matches!(
                    style.flex_direction,
                    FlexDirection::Row | FlexDirection::RowReverse
                );
                let gap = Au::from_px(
                    style
                        .column_gap
                        .as_ref()
                        .and_then(LengthPercentage::fixed)
                        .unwrap_or(0.0),
                );
                // A row that wraps is as narrow as its widest item: each item may go on a line
                // of its own (as in Chrome).
                let wraps = style.flex_wrap != FlexWrap::NoWrap;
                // A column that wraps at a definite or maximum height is as wide as its lines.
                let limit = self
                    .intrinsic_height
                    .get()
                    .or_else(|| crate::flex_column::max_content_height(style, &model, None))
                    .or_else(|| style.flex_balance.then_some(Au::MAX));
                if !row
                    && wraps
                    && let Some(limit) = limit
                {
                    let basis = if limit == Au::MAX { 0.0 } else { limit.to_px() };
                    let row_gap = Au::from_px(style.row_gap.as_ref().map_or(0.0, |gap| gap.resolve(basis)));
                    let min = children
                        .iter()
                        .map(|&child| self.intrinsic(child).0)
                        .max()
                        .unwrap_or(Au::ZERO);
                    let definite = self.intrinsic_height.get() == Some(limit);
                    let max = self.wrapped_column_width(style, &children, (limit, definite), (gap, row_gap));
                    (min, max.max(min))
                } else if row && let Some(count) = crate::flex_column::balance(style) {
                    // A balanced row is as wide as its widest line where its items need at least
                    // `flex-line-count` lines (Blink's `ComputeMinMaxSizeOfRowContainer`).
                    let contributions: Vec<(Au, Au)> = children
                        .iter()
                        .map(|&child| self.row_item_contributions(child, true, None))
                        .collect();
                    let maxes: Vec<Au> = contributions.iter().map(|&(_, max)| max).collect();
                    let min = contributions
                        .iter()
                        .map(|&(min, _)| min)
                        .max()
                        .unwrap_or(Au::ZERO);
                    let max = crate::flex_balance::balanced_lines(&maxes, Au::MAX, gap, count)
                        .into_iter()
                        .map(|range| {
                            let gaps = gap * (i32::try_from(range.len()).unwrap_or(i32::MAX) - 1).max(0);
                            maxes[range].iter().copied().sum::<Au>() + gaps
                        })
                        .max()
                        .unwrap_or(Au::ZERO);
                    (min, max.max(min))
                } else {
                    children
                        .iter()
                        .enumerate()
                        .fold((Au::ZERO, Au::ZERO), |(min, max), (index, &child)| {
                            let (child_min, child_max) = if row {
                                // A single line's items stretch to a definite height.
                                let height = self.intrinsic_height.get().filter(|_| !wraps);
                                let cross = self.definite_cross(style, child, Au::ZERO, (height, height));
                                self.row_item_contributions(child, wraps, cross)
                            } else {
                                self.intrinsic(child)
                            };
                            if row {
                                let gap = if index > 0 { gap } else { Au::ZERO };
                                if wraps {
                                    (min.max(child_min), max + child_max + gap)
                                } else {
                                    (min + child_min + gap, max + child_max + gap)
                                }
                            } else {
                                (min.max(child_min), max.max(child_max))
                            }
                        })
                }
            }
            BoxKind::Table { .. } => {
                let (min, max) = self.table_intrinsic(id);
                // The frame is already part of the table's widths; only margins are added below.
                let margins = model.margin[LEFT] + model.margin[RIGHT];
                return (min + margins, max + margins);
            }
            BoxKind::RowGroup { .. } | BoxKind::Row { .. } => (Au::ZERO, Au::ZERO),
            BoxKind::Replaced { .. } => {
                let (width, _) = self.replaced_size(id, Au::ZERO, None, self.intrinsic_height.get(), true);
                // Percentage sizes make a replaced element compressible: it may shrink to nothing.
                let percentage = |length: &LengthPercentage| length.fixed().is_none();
                let compressible = matches!(&style.width, Size::Length(length) if percentage(length))
                    || matches!(&style.max_width, MaxSize::Length(length) if percentage(length));
                (if compressible { Au::ZERO } else { width }, width)
            }
        };
        (min + frame, max + frame)
    }

    /// Lays out an atomic inline (`inline-block`, `inline-flex`, …) in a line of width
    /// `available`: shrink-to-fit unless its width is given (CSS 2.1 §10.3.9).
    pub(crate) fn atomic(&self, id: BoxId, available: Au) -> Atomic {
        self.kept_atomic(id, available, || self.lay_out_atomic(id, available))
    }

    /// [`Layouter::atomic`], laid out anew.
    fn lay_out_atomic(&self, id: BoxId, available: Au) -> Atomic {
        if let Some(column) = self.tree.ruby_columns.get(&id) {
            return crate::ruby::layout(self, id, column, 0.0);
        }
        let style = &self.tree.get(id).style;
        // Auto margins of atomic inlines are zero.
        let margin = |value: &LengthPercentageAuto| Au::from_px(value.resolve_or(available.to_px(), 0.0));
        let (margin_left, margin_right) = (margin(&style.margin.left), margin(&style.margin.right));
        let (margin_top, margin_bottom) = (margin(&style.margin.top), margin(&style.margin.bottom));
        // A replaced element sizes itself (CSS 2.1, §10.3.2); other boxes shrink to fit.
        let replaced = matches!(self.tree.get(id).kind, BoxKind::Replaced { .. });
        let content_width = if replaced {
            None
        } else {
            self.shrunk_width(id, available, available, self.inline_containing_height.get())
        };
        let outcome = if replaced {
            let containing_height = self
                .cell_replaced_height
                .get()
                .or(self.inline_containing_height.get());
            Some(self.replaced(id, available, None, None, containing_height))
        } else {
            self.inline_table
                .set(matches!(self.tree.get(id).kind, BoxKind::Table { .. }));
            // Percentage heights refer to the height of the block the line is in, where that is
            // definite, as for floats and inline blocks in Chrome.
            self.item_in(
                id,
                (available, self.inline_containing_height.get()),
                content_width,
                None,
            )
        };
        let Some(Outcome { mut fragment, .. }) = outcome else {
            return Atomic {
                fragment: placeholder(id, Au::ZERO, Au::ZERO),
                width: Au::ZERO,
                height: Au::ZERO,
                baseline: Au::ZERO,
            };
        };
        fragment.rect.x = margin_left;
        fragment.rect.y = margin_top;
        let height = margin_top + fragment.rect.height + margin_bottom;
        // The baseline of the last line box inside, or the bottom margin edge of a scroll
        // container (CSS 2.1 §10.8.1; `overflow: clip` makes none, as in Chrome). Flex and grid
        // containers keep the baseline of their items when they scroll, as in Chrome, and tables
        // do not scroll.
        // Layout containment leaves it without a baseline too (CSS Contain 2, §3.2).
        let clips = style.layout_containment
            || (style.overflow_x == fagus_style::values::Overflow::Hidden
                || style.overflow_y == fagus_style::values::Overflow::Hidden)
                && !matches!(
                    style.display,
                    fagus_style::values::Display::Normal {
                        inside: fagus_style::values::DisplayInside::Flex
                            | fagus_style::values::DisplayInside::Grid
                            | fagus_style::values::DisplayInside::Table,
                        ..
                    }
                );
        // A box whose lines run the other way has none (as in Chrome). Without one, it sits on
        // its bottom margin edge, in vertical lines on the middle of its margin box (the central
        // baseline, CSS Inline 3, §5.1).
        let mode = self.writing_mode();
        let raised = self
            .tree
            .border_box_baselines
            .get(&id)
            .filter(|_| !style.layout_containment);
        let baseline = if let Some(raise) = raised {
            Some(margin_top + fragment.rect.height - Au::from_px(*raise))
        } else if clips || !style.writing_mode.is_parallel(mode) {
            None
        } else if matches!(self.tree.get(id).kind, BoxKind::Table { .. })
            || matches!(
                style.display,
                fagus_style::values::Display::Normal {
                    inside: fagus_style::values::DisplayInside::Flex
                        | fagus_style::values::DisplayInside::Grid,
                    ..
                }
            )
        {
            // An inline table sits on the baseline of its first row (CSS 2.1 §10.8.1), an
            // inline flex or grid container on its first baseline (as in Chrome). A table with
            // captions is laid out in its wrapper box.
            first_baseline(&fragment, mode).map(|baseline| margin_top + baseline)
        } else {
            last_baseline(&fragment, mode).map(|baseline| margin_top + baseline)
        }
        // In `vertical-lr` a line's over side is its bottom in its coordinates, where the box
        // lies mirrored, its content not (see `crate::writing::physical_rect`): its baseline is
        // as far from the bottom of its margin box as it is from the top inside (where
        // `last_baseline` measures it).
        .map_or_else(
            || {
                if mode.is_vertical() && !mode.is_sideways() {
                    height / 2
                } else {
                    height
                }
            },
            |baseline| {
                if mode == WritingMode::VerticalLr && raised.is_none() {
                    height - baseline
                } else {
                    baseline
                }
            },
        );
        Atomic {
            width: margin_left + fragment.rect.width + margin_right,
            height,
            baseline,
            fragment,
        }
    }

    /// The content size of a replaced element from its style, its attributes and the image's
    /// intrinsic size and ratio (CSS 2.1 §10.3.2, §10.4, §10.6.2). With `intrinsic`, percentage
    /// sizes are ignored, as for intrinsic size contributions.
    pub(crate) fn replaced_size(
        &self,
        id: BoxId,
        containing_width: Au,
        width_override: Option<Au>,
        containing_height: Option<Au>,
        intrinsic: bool,
    ) -> (Au, Au) {
        let layout_box = self.tree.get(id);
        let BoxKind::Replaced { image, attributes } = &layout_box.kind else {
            return (Au::ZERO, Au::ZERO);
        };
        let style = &layout_box.style;
        let model = self.box_model(style, containing_width, Some(Au::ZERO));
        // A formula is as large as its content; a block one fills the width of its containing
        // block.
        if let Some(math) = self.tree.formulas.get(&id) {
            let frame_x =
                model.border[LEFT] + model.border[RIGHT] + model.padding[LEFT] + model.padding[RIGHT];
            let available = width_override.or_else(|| {
                (math.block && !intrinsic).then(|| {
                    (containing_width - model.margin[LEFT] - model.margin[RIGHT] - frame_x)
                        .clamp_non_negative()
                })
            });
            let laid = math.laid(self.fonts, available);
            return (available.unwrap_or(laid.width), laid.height);
        }
        let frame_x = model.border[LEFT] + model.border[RIGHT] + model.padding[LEFT] + model.padding[RIGHT];
        let frame_y = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        let border_box = style.box_sizing == BoxSizing::BorderBox;
        let content = |size: f32, frame: Au| {
            if border_box {
                (size - frame.to_px()).max(0.0)
            } else {
                size
            }
        };
        let basis = containing_width.to_px();
        let length = |length: &LengthPercentage| {
            if intrinsic && length.fixed().is_none() {
                None
            } else {
                Some(length.resolve(basis))
            }
        };
        // Without natural sizes, an image takes the default size of 300 × 150 pixels (CSS Images 3,
        // §5.2); a box without an image is empty.
        let default = if image.is_some() {
            (300.0 * style.zoom, 150.0 * style.zoom)
        } else {
            (0.0, 0.0)
        };
        let mut natural_width = image
            .as_ref()
            .filter(|image| image.natural.0)
            .map(|image| image.width);
        let mut natural_height = image
            .as_ref()
            .filter(|image| image.natural.1)
            .map(|image| image.height);
        // `aspect-ratio: <ratio>` wins over the image's ratio, `auto && <ratio>` gives way to it
        // (CSS Box Sizing 4, §5.1).
        let forced = !style.aspect_ratio_auto && style.aspect_ratio.is_some();
        let pick = |natural: Option<f32>| {
            if forced {
                style.aspect_ratio
            } else {
                natural.or(style.aspect_ratio)
            }
        };
        // Size containment leaves the element without the natural sizes and ratio of what it
        // shows: it is as large as `contain-intrinsic-size` says, else empty (CSS Contain 2,
        // §3.1), as in Chrome.
        let contained = style.size_containment();
        let natural_ratio = image
            .as_ref()
            .and_then(|image| image.ratio)
            .filter(|_| !contained.0 && !contained.1);
        let mut ratio = pick(natural_ratio);
        let mut attributes = *attributes;
        // In vertical lines the box's width is its height: so are the image's sizes (its style is
        // turned already).
        if self.writing_mode().is_vertical() {
            std::mem::swap(&mut natural_width, &mut natural_height);
            ratio = pick(natural_ratio.map(|ratio| 1.0 / ratio));
            attributes = (attributes.1, attributes.0);
        }
        // The sizes containment gives are the (turned) style's own.
        if contained.0 {
            natural_width = Some(style.contain_intrinsic_size.0.unwrap_or(0.0));
        }
        if contained.1 {
            natural_height = Some(style.contain_intrinsic_size.1.unwrap_or(0.0));
        }
        // A ratio of `aspect-ratio` itself sizes the box `box-sizing` names (CSS Box Sizing 4,
        // §5.1): with `border-box`, the frame is part of both sides.
        let (ratio_x, ratio_y) = if forced && crate::ratio::ratio_sizing(style) == BoxSizing::BorderBox {
            (frame_x.to_px(), frame_y.to_px())
        } else {
            (0.0, 0.0)
        };
        let height_of_width = |width: f32, ratio: f32| ((width + ratio_x) / ratio - ratio_y).max(0.0);
        let width_of_height = |height: f32, ratio: f32| ((height + ratio_y) * ratio - ratio_x).max(0.0);
        let specified_width = width_override.map(Au::to_px).or_else(|| match &style.width {
            Size::Length(width) => length(width).map(|width| content(width, frame_x)),
            // The margin box fills the containing block (not for intrinsic sizes).
            Size::Stretch if !intrinsic => Some(
                (containing_width - model.margin[LEFT] - model.margin[RIGHT] - frame_x)
                    .clamp_non_negative()
                    .to_px(),
            ),
            _ => attributes.0,
        });
        // Percentage heights refer to a containing block of a definite height (for intrinsic
        // sizes, of a fixed one: see `Layouter::intrinsic_height`).
        let height_of = |length: &LengthPercentage| match length.fixed() {
            Some(px) => Some(px),
            None => containing_height.map(|basis| length.resolve(basis.to_px())),
        };
        let specified_height = match &style.height {
            Size::Length(length) => height_of(length).map(|height| content(height, frame_y)),
            // The margin box fills a containing block of a definite height (not for intrinsic
            // sizes; an absolutely positioned element fills the room its insets leave, see
            // `crate::positioned`).
            Size::Stretch
                if !intrinsic
                    && !style.is_out_of_flow()
                    && let Some(containing) = containing_height =>
            {
                Some(
                    (containing - model.margin[TOP] - model.margin[BOTTOM] - frame_y)
                        .clamp_non_negative()
                        .to_px(),
                )
            }
            _ => attributes.1,
        };
        let (mut width, mut height) = match (specified_width, specified_height) {
            (Some(width), Some(height)) => (width, height),
            (Some(width), None) => (
                width,
                ratio
                    .map(|ratio| height_of_width(width, ratio))
                    .or(natural_height)
                    .unwrap_or(default.1),
            ),
            (None, Some(height)) => (
                ratio
                    .map(|ratio| width_of_height(height, ratio))
                    .or(natural_width)
                    .unwrap_or(default.0),
                height,
            ),
            (None, None) => match (natural_width, natural_height, ratio) {
                (Some(width), Some(_), Some(ratio)) if forced => (width, height_of_width(width, ratio)),
                (Some(width), Some(height), _) => (width, height),
                (Some(width), None, Some(ratio)) => (width, height_of_width(width, ratio)),
                (None, Some(height), Some(ratio)) => (width_of_height(height, ratio), height),
                // An image with a ratio only is as wide as the containing block allows (CSS 2.1,
                // §10.3.2, as browsers do), which counts for nothing in intrinsic sizes; where its
                // width is a percentage, which refers to that, it counts as the default width, as
                // in Chrome (WPT `svg-intrinsic-size-006`).
                (None, None, Some(ratio)) if image.is_some() => {
                    let margins =
                        style.margin.left.resolve_or(basis, 0.0) + style.margin.right.resolve_or(basis, 0.0);
                    let percentage = matches!(&style.width, Size::Length(length) if length.fixed().is_none());
                    let width = if intrinsic && percentage {
                        default.0
                    } else if intrinsic {
                        0.0
                    } else {
                        (basis - margins - frame_x.to_px()).max(0.0)
                    };
                    (width, height_of_width(width, ratio))
                }
                (width, height, _) => (width.unwrap_or(default.0), height.unwrap_or(default.1)),
            },
        };
        // min-width and max-width, then min-height and max-height; an auto size follows the ratio.
        // A content-based keyword is the size the element has where that size is auto: through
        // the ratio from the other size, else its natural size (CSS Box Sizing 3, §5.1).
        let auto_width = |height: f32| {
            ratio
                .map(|ratio| width_of_height(height, ratio))
                .or(natural_width)
                .unwrap_or(default.0)
        };
        let auto_height = |width: f32| {
            ratio
                .map(|ratio| height_of_width(width, ratio))
                .or(natural_height)
                .unwrap_or(default.1)
        };
        let max_width = match &style.max_width {
            MaxSize::Length(max) => length(max).map(|max| content(max, frame_x)),
            MaxSize::MinContent | MaxSize::MaxContent | MaxSize::FitContent(_) => Some(auto_width(height)),
            _ => None,
        };
        let min_width = match &style.min_width {
            Size::Length(min) => length(min).map(|min| content(min, frame_x)),
            Size::MinContent | Size::MaxContent | Size::FitContent(_) => Some(auto_width(height)),
            _ => None,
        };
        // Both sizes automatic: the bounds of either may decide both (CSS 2.1, §10.4).
        if specified_width.is_none() && specified_height.is_none() && ratio.is_some() {
            let max_height = match &style.max_height {
                MaxSize::Length(max) => height_of(max).map(|max| content(max, frame_y)),
                _ => None,
            };
            let min_height = match &style.min_height {
                Size::Length(min) => height_of(min).map(|min| content(min, frame_y)),
                _ => None,
            };
            let (width, height) = crate::ratio::constrain_both(
                (width, height),
                (min_width.unwrap_or(0.0), max_width.unwrap_or(f32::INFINITY)),
                (min_height.unwrap_or(0.0), max_height.unwrap_or(f32::INFINITY)),
            );
            return (Au::from_px(width), Au::from_px(height));
        }
        let clamped = width
            .min(max_width.unwrap_or(f32::INFINITY))
            .max(min_width.unwrap_or(0.0));
        if (clamped - width).abs() > f32::EPSILON {
            width = clamped;
            if specified_height.is_none()
                && let Some(ratio) = ratio
            {
                height = height_of_width(width, ratio);
            }
        }
        let max_height = match &style.max_height {
            MaxSize::Length(max) => height_of(max).map(|max| content(max, frame_y)),
            MaxSize::MinContent | MaxSize::MaxContent | MaxSize::FitContent(_) => Some(auto_height(width)),
            _ => None,
        };
        let min_height = match &style.min_height {
            Size::Length(min) => height_of(min).map(|min| content(min, frame_y)),
            Size::MinContent | Size::MaxContent | Size::FitContent(_) => Some(auto_height(width)),
            _ => None,
        };
        let clamped = height
            .min(max_height.unwrap_or(f32::INFINITY))
            .max(min_height.unwrap_or(0.0));
        if (clamped - height).abs() > f32::EPSILON {
            height = clamped;
            if specified_width.is_none()
                && let Some(ratio) = ratio
            {
                width = width_of_height(height, ratio);
            }
        }
        (Au::from_px(width), Au::from_px(height))
    }

    /// Lays out a replaced element; `height_override` is a border-box height to stretch to,
    /// `containing_height` the definite height of the containing block, if any.
    fn replaced(
        &self,
        id: BoxId,
        containing_width: Au,
        width_override: Option<Au>,
        height_override: Option<Au>,
        containing_height: Option<Au>,
    ) -> Outcome {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let (width, mut height) =
            self.replaced_size(id, containing_width, width_override, containing_height, false);
        let model = self.box_model(style, containing_width, Some(width));
        let frame_y = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        if let Some(stretched) = height_override {
            // A grid item that keeps the height of its area (see `Layouter::fixed_item_height`)
            // takes it also where it is shorter than its own.
            height = if self.fixed_item_height.get() {
                (stretched - frame_y).clamp_non_negative()
            } else {
                height.max(stretched - frame_y)
            };
        }
        let x0 = model.border[LEFT] + model.padding[LEFT];
        let y0 = model.border[TOP] + model.padding[TOP];
        let children = match &layout_box.kind {
            BoxKind::Replaced {
                image: Some(image), ..
            } => vec![match self.tree.marks.get(&id) {
                // A control's mark is drawn at the size of its content box, or of its border
                // box (a slider).
                Some(mark) => {
                    let (rect, content) = if mark.covers_border_box() {
                        let rect = Rect::new(
                            Au::ZERO,
                            Au::ZERO,
                            width + x0 + model.border[RIGHT] + model.padding[RIGHT],
                            height + frame_y,
                        );
                        (rect, [x0.to_px(), y0.to_px(), width.to_px(), height.to_px()])
                    } else {
                        let rect = Rect::new(x0, y0, width, height);
                        (rect, [0.0, 0.0, width.to_px(), height.to_px()])
                    };
                    Fragment {
                        rect,
                        kind: FragmentKind::Image(mark.image(
                            rect.width.to_px(),
                            rect.height.to_px(),
                            content,
                            style.zoom,
                        )),
                        children: Children::new(),
                    }
                }
                // An inline `<svg>` drawn in layers between its `<foreignObject>` boxes starts
                // with the first.
                None => Fragment {
                    rect: Rect::new(x0, y0, width, height),
                    kind: FragmentKind::Image(self.first_svg_layer(id).unwrap_or_else(|| Arc::clone(image))),
                    children: Children::new(),
                },
            }]
            .into_iter()
            .chain(self.foreign_objects(id, (x0, y0), width, height))
            .collect(),
            // The line of a formula, centered in the width of a block one.
            _ => match self.tree.formulas.get(&id) {
                Some(math) => {
                    let mut line = math.laid(self.fonts, Some(width)).line.clone();
                    line.rect.x = x0;
                    line.rect.y = y0;
                    vec![line]
                }
                None => Vec::new(),
            },
        };
        Outcome::new(
            Fragment {
                rect: Rect::new(
                    model.margin[LEFT],
                    Au::ZERO,
                    width
                        + model.border[LEFT]
                        + model.border[RIGHT]
                        + model.padding[LEFT]
                        + model.padding[RIGHT],
                    height + frame_y,
                ),
                kind: FragmentKind::Box {
                    style: Arc::clone(style),
                    node: layout_box.node,
                    sides: VisibleSides::ALL,
                },
                children: children.into(),
            },
            None,
            model.margin[TOP],
            model.margin[BOTTOM],
        )
    }

    /// The children that take part in the layout of their parent (all but absolutely positioned).
    pub(crate) fn in_flow<'b>(&'b self, children: &'b [BoxId]) -> impl Iterator<Item = BoxId> + 'b {
        children
            .iter()
            .copied()
            .filter(|&child| !self.tree.get(child).style.is_out_of_flow())
    }

    /// Lays out an absolutely positioned box in a containing block of `cb_width` × `cb_height`
    /// (its padding box). `static_position` is where the box would be if it were in flow. The
    /// fragment is positioned relative to the containing block (CSS 2.1 §10.3.7 and §10.6.4).
    pub(crate) fn absolute(
        &self,
        id: BoxId,
        cb_width: Au,
        cb_height: Au,
        static_position: (Au, Au),
    ) -> Fragment {
        self.absolute_styled(id, cb_width, cb_height, static_position, &self.tree.get(id).style)
    }

    /// [`Self::absolute`] with the insets, margins and sizes of `style` instead of the box's own
    /// (anchor positioning resolves them, see [`crate::anchor`]).
    pub(crate) fn absolute_styled(
        &self,
        id: BoxId,
        cb_width: Au,
        cb_height: Au,
        static_position: (Au, Au),
        style: &ComputedStyle,
    ) -> Fragment {
        let layout_box = self.tree.get(id);
        let _whole = self.enter_box(&layout_box.style);
        let inset = |value: &LengthPercentageAuto, basis: Au| match value {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Length(length) => Some(Au::from_px(length.resolve(basis.to_px()))),
        };
        let left = inset(&style.inset.left, cb_width);
        // Without insets the box starts at its static position: its left edge, or in a
        // right-to-left block container its right one (CSS 2.1, §10.3.7). In content of another
        // writing mode than the containing block's, the corner of its block-start and
        // inline-start sides there.
        let static_ends = self.tree.flows.static_ends.get(&id).copied();
        let end_x = static_ends.map_or_else(
            || {
                self.inline_static_rtl
                    .borrow()
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| self.static_rtl.contains(&id))
            },
            |ends| ends.0,
        );
        let rtl_static = end_x && left.is_none() && style.inset.right.is_auto();
        let right = if rtl_static {
            Some(cb_width - static_position.0)
        } else {
            inset(&style.inset.right, cb_width)
        };
        let static_left = if rtl_static { Au::ZERO } else { static_position.0 };
        let top = inset(&style.inset.top, cb_height);
        let bottom = match inset(&style.inset.bottom, cb_height) {
            None if top.is_none() && static_ends.is_some_and(|ends| ends.1) => {
                Some(cb_height - static_position.1)
            }
            bottom => bottom,
        };
        // Margins resolve against the containing block's width, even vertical ones.
        let margin = |value: &LengthPercentageAuto| inset(value, cb_width);
        let mut margin_left = margin(&style.margin.left);
        let margin_right = margin(&style.margin.right);
        let mut margin_top = margin(&style.margin.top);
        let margin_bottom = margin(&style.margin.bottom);
        let model = self.box_model(style, cb_width, Some(Au::ZERO));
        let frame_x = model.border[LEFT] + model.border[RIGHT] + model.padding[LEFT] + model.padding[RIGHT];
        let frame_y = model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
        let content_box = |border_box: Au, frame: Au| {
            if style.box_sizing == BoxSizing::BorderBox {
                (border_box - frame).clamp_non_negative()
            } else {
                border_box
            }
        };
        let margins_x = margin_left.unwrap_or(Au::ZERO) + margin_right.unwrap_or(Au::ZERO);
        // `justify-self` and `align-self` other than `normal` and `stretch` align the box in the
        // room between its insets, which it no longer stretches across (CSS Box Alignment 3,
        // §6.1, as in Chrome). Only in horizontal writing modes, from a static position in one.
        let horizontal = !style.writing_mode.is_vertical() && static_ends.is_none();
        let justify = absolute_alignment(style.justify_self).filter(|_| horizontal);
        let align = absolute_alignment(style.align_self).filter(|_| horizontal);

        // Width: specified, from the height and the aspect ratio, stretched between both insets,
        // or shrink-to-fit in the space between the insets.
        let available = cb_width - left.unwrap_or(static_left) - right.unwrap_or(Au::ZERO);
        // The aspect ratio of a box that is not replaced (a replaced element sizes itself with
        // its natural ratio): its width follows a definite height, or one stretched between its
        // top and bottom insets where it is not stretched between its left and right ones, as
        // in Chrome; otherwise its height follows its width (CSS Box Sizing 4, §5.1).
        let replaced = matches!(layout_box.kind, BoxKind::Replaced { .. });
        let ratio = crate::ratio::ratio(style).filter(|_| !replaced);
        let stretches_x = left.is_some() && right.is_some() && justify.is_none();
        let stretches_y = top.is_some() && bottom.is_some() && align.is_none();
        let ratio_height = ratio.and_then(|ratio| {
            let height = crate::ratio::definite_height(style, frame_y, Some(cb_height)).or_else(|| {
                let (Some(top), Some(bottom)) = (top, bottom) else {
                    return None;
                };
                (matches!(style.height, Size::Auto) && stretches_y && !stretches_x).then(|| {
                    let margins = margin_top.unwrap_or(Au::ZERO) + margin_bottom.unwrap_or(Au::ZERO);
                    let stretched = (cb_height - top - bottom - margins).clamp_non_negative();
                    crate::ratio::bounded_height(style, frame_y, Some(cb_height), stretched)
                })
            })?;
            Some((height, ratio))
        });
        // The room for the lines of content that runs across the containing block's lines
        // (an orthogonal flow, CSS Writing Modes 4, §7.3): the containing block's height between
        // the top inset (or the static position) and the bottom one, as in Chrome.
        let orthogonal_room = self
            .tree
            .flows
            .content_of(id)
            .filter(|_| self.is_orthogonal(id))
            .map(|_| {
                (cb_height - top.unwrap_or(static_position.1) - bottom.unwrap_or(Au::ZERO))
                    .clamp_non_negative()
            });
        let mut content_width = match (&style.width, left, right) {
            (Size::Length(length), _, _) => {
                content_box(Au::from_px(length.resolve(cb_width.to_px())), frame_x)
            }
            (Size::Auto, _, _) if let Some((height, ratio)) = ratio_height => {
                let border_box = crate::ratio::width_from_height(style, ratio, (frame_x, frame_y), height);
                (border_box - frame_x).clamp_non_negative()
            }
            // Sizing keywords, `stretch` from the box's start inset to its end one.
            (width, _, _) if crate::sizing::is_keyword(width) => {
                self.keyword_width(id, cb_width, available).unwrap_or(Au::ZERO)
            }
            (_, Some(left), Some(right)) if justify.is_none() => {
                (cb_width - left - right - margins_x - frame_x).clamp_non_negative()
            }
            _ => {
                // The percentage heights of replaced elements inside refer to the box's height
                // where that is definite: given, or stretched between its insets (as in
                // Chrome).
                let height = crate::ratio::definite_height(style, frame_y, Some(cb_height))
                    .or_else(|| {
                        let (Some(top), Some(bottom)) = (top, bottom) else {
                            return None;
                        };
                        let margins = margin_top.unwrap_or(Au::ZERO) + margin_bottom.unwrap_or(Au::ZERO);
                        (matches!(style.height, Size::Auto) && align.is_none()).then(|| {
                            let stretched = (cb_height - top - bottom - margins).clamp_non_negative();
                            crate::ratio::bounded_height(style, frame_y, Some(cb_height), stretched)
                        })
                    })
                    // (A fixed height is definite without the containing block.)
                    .filter(|_| !matches!(&style.height, Size::Length(length) if length.fixed().is_some()));
                let margins_y = margin_top.unwrap_or(Au::ZERO) + margin_bottom.unwrap_or(Au::ZERO);
                let (min, max) = match (height, orthogonal_room) {
                    // Those of a height of its own (stretched between the insets) as long.
                    (Some(height), Some(_)) => {
                        self.with_cross_room(Some(height + margins_y), || self.content_widths(id))
                    }
                    (Some(height), None) => {
                        self.intrinsic_at_height(id, Some((height - frame_y).clamp_non_negative()))
                    }
                    // Lines across the containing block's are as long as the room between the
                    // vertical insets lets them be, which sets the width they take.
                    (None, Some(room)) => self.with_cross_room(Some(room), || self.content_widths(id)),
                    (None, None) => self.intrinsic(id),
                };
                (max.min(available.max(min)) - frame_x - margins_x).clamp_non_negative()
            }
        };
        // An automatic width otherwise stays within the widths the minimum and maximum heights
        // give through the ratio.
        if let Some(ratio) = ratio
            && ratio_height.is_none()
            && matches!(style.width, Size::Auto)
        {
            let (low, high) =
                crate::ratio::transferred_widths(style, ratio, (frame_x, frame_y), Some(cb_height));
            let content = |width: Au| {
                if width == Au::MAX {
                    width
                } else {
                    (width - frame_x).clamp_non_negative()
                }
            };
            content_width = content_width.min(content(high)).max(content(low));
        }
        // `calc-size()` of the width its basis gives.
        content_width = crate::sizing::calculated(style, 0, content_width, frame_x, cb_width);
        content_width = self.clamped_width(id, content_width, cb_width, available);
        // A width from the height through the ratio is no narrower than the content's
        // min-content width while `min-width` is `auto` (the automatic minimum size).
        if ratio_height.is_some()
            && matches!(style.width, Size::Auto)
            && matches!(style.min_width, Size::Auto)
            && !crate::sizing::is_scroll_container(style)
        {
            let max = self.clamped_width(id, Au::MAX, cb_width, available);
            content_width = content_width.max(self.content_box_widths(id).0.min(max));
        }
        let width = content_width + frame_x;

        // Height: specified, or stretched between both insets; otherwise the content's, also
        // where it is `fit-content` (a popover's) or another content size.
        let specified_height = match &style.height {
            Size::Length(length) => {
                Some(content_box(Au::from_px(length.resolve(cb_height.to_px())), frame_y) + frame_y)
            }
            _ => None,
        };
        // An automatic height that follows the width through the ratio is not stretched.
        let ratio_follows = ratio.is_some() && ratio_height.is_none() && matches!(style.height, Size::Auto);
        let stretched_height = match (top, bottom) {
            (Some(top), Some(bottom))
                if align.is_none()
                    && !ratio_follows
                    && matches!(style.height, Size::Auto | Size::Stretch) =>
            {
                Some(
                    (cb_height
                        - top
                        - bottom
                        - margin_top.unwrap_or(Au::ZERO)
                        - margin_bottom.unwrap_or(Au::ZERO))
                    .clamp_non_negative(),
                )
            }
            // `stretch` fills the room between the insets, from the static position where the
            // top one is auto (CSS Box Sizing 4, §3.1).
            (top, bottom) if align.is_none() && matches!(style.height, Size::Stretch) => Some(
                (cb_height
                    - top.unwrap_or(static_position.1)
                    - bottom.unwrap_or(Au::ZERO)
                    - margin_top.unwrap_or(Au::ZERO)
                    - margin_bottom.unwrap_or(Au::ZERO))
                .clamp_non_negative(),
            ),
            _ => None,
        };
        let space = Space {
            containing_height: Some(cb_height),
            ..Space::unfragmented(cb_width)
        };
        // A maximum height caps a specified or stretched height and a minimum one raises it
        // (CSS 2.1 §10.7, the maximum first); content-based keywords take the content's height.
        // The lines of orthogonal content without a height of its own are as long as the room
        // lets them be, down to their max-content length.
        let orthogonal_height = orthogonal_room
            .filter(|_| {
                specified_height.is_none() && stretched_height.is_none() && matches!(style.height, Size::Auto)
            })
            .and_then(|room| {
                let inner = self.tree.flows.content_of(id)?;
                let inline = self.orthogonal_inline_size(
                    id,
                    inner,
                    (None, Some(content_width)),
                    (cb_width, Some(room)),
                );
                Some(inline + frame_y)
            });
        let given_height = specified_height.or(stretched_height).or(orthogonal_height);
        let height_override = given_height.map(|height| {
            let content = std::cell::OnceCell::new();
            let content_height = || {
                *content.get_or_init(|| match &layout_box.kind {
                    BoxKind::Block { .. } | BoxKind::Inline { .. } => self
                        .block_container_with(id, space, None, Some(content_width), None, true)
                        .map(|outcome| outcome.fragment.rect.height),
                    _ => None,
                })
            };
            let length = |length: &LengthPercentage| {
                content_box(Au::from_px(length.resolve(cb_height.to_px())), frame_y) + frame_y
            };
            let max = match &style.max_height {
                MaxSize::Length(max) => Some(length(max)),
                MaxSize::MinContent | MaxSize::MaxContent | MaxSize::FitContent(_) => content_height(),
                MaxSize::Stretch | MaxSize::None => None,
            };
            let min = match &style.min_height {
                Size::Length(min) => Some(length(min)),
                Size::MinContent | Size::MaxContent | Size::FitContent(_) => content_height(),
                _ => None,
            };
            let height = max.map_or(height, |max| height.min(max.max(frame_y)));
            min.map_or(height, |min| height.max(min))
        });
        // Columns take a height that is auto or a fixed length (see `multicol_in`), not one
        // stretched between the insets.
        let columns = layout_box.style.is_multicol()
            && match &style.height {
                Size::Auto => stretched_height.is_none(),
                Size::Length(length) => length.fixed().is_some(),
                _ => false,
            };
        let outcome = match &layout_box.kind {
            // A multi-column container lays out its columns (balanced, in a height that is auto or
            // a fixed length, which `multicol_with` reads from the style); it is at least as tall as
            // its specified height. (A fieldset with a legend keeps its own layout.)
            BoxKind::Block { .. } if columns && !self.tree.fieldsets.contains_key(&id) => self
                .in_multicol(id, || self.multicol_with(id, space, None, Some(content_width)))
                .map(|mut outcome| {
                    if let Some(height) = height_override {
                        outcome.fragment.rect.height = outcome.fragment.rect.height.max(height);
                    }
                    outcome
                }),
            BoxKind::Block { .. } | BoxKind::Inline { .. } => {
                self.block_container_with(id, space, None, Some(content_width), height_override, true)
            }
            BoxKind::Table { .. } => self.table(id, space, None),
            BoxKind::Replaced { .. } => Some(self.replaced(
                id,
                cb_width,
                Some(content_width),
                height_override,
                Some(cb_height),
            )),
            _ => {
                let (mut outcome, _) = self.container_lines(id, space, Some(content_width), height_override);
                if let Some(height) = height_override {
                    outcome.fragment.rect.height = outcome.fragment.rect.height.max(height);
                }
                Some(outcome)
            }
        };
        let Some(Outcome { mut fragment, .. }) = outcome else {
            return placeholder(id, Au::ZERO, Au::ZERO);
        };
        let height = fragment.rect.height;
        // A table sizes itself in the containing block: it is placed by the width it takes,
        // as in Chrome (WPT `absolute-tables-011`).
        let width = if matches!(layout_box.kind, BoxKind::Table { .. }) {
            fragment.rect.width
        } else {
            width
        };

        // Auto margins center the box between the insets.
        let mut x = match (left, right) {
            (Some(left), Some(right)) => {
                let free = cb_width - left - right - width - margins_x;
                let mut shift = Au::ZERO;
                match (margin_left, margin_right) {
                    (None, None) if free >= Au::ZERO => {
                        margin_left = Some(free / 2);
                    }
                    (None, _) => margin_left = Some(free + margin_right.unwrap_or(Au::ZERO)),
                    // Over-constrained: the right inset is ignored, or in a right-to-left
                    // containing block the left one.
                    (Some(_), Some(_)) if justify.is_none() && self.rtl_containers.contains(&id) => {
                        shift = free;
                    }
                    (Some(_), Some(_)) => {
                        shift = aligned_shift(justify, free, style.is_rtl());
                        if free < Au::ZERO && justify.is_some() {
                            shift = kept_in(left, shift, width + margins_x, cb_width);
                        }
                    }
                    _ => {}
                }
                left + margin_left.unwrap_or(Au::ZERO) + shift
            }
            (Some(left), None) => left + margin_left.unwrap_or(Au::ZERO),
            (None, Some(right)) => cb_width - right - margin_right.unwrap_or(Au::ZERO) - width,
            (None, None) => static_left + margin_left.unwrap_or(Au::ZERO),
        };
        let mut y = match (top, bottom) {
            (Some(top), Some(bottom)) => {
                let free = cb_height
                    - top
                    - bottom
                    - height
                    - margin_top.unwrap_or(Au::ZERO)
                    - margin_bottom.unwrap_or(Au::ZERO);
                let mut shift = Au::ZERO;
                match (margin_top, margin_bottom) {
                    (None, None) if free >= Au::ZERO => margin_top = Some(free / 2),
                    (None, _) => margin_top = Some(free + margin_bottom.unwrap_or(Au::ZERO)),
                    (Some(_), Some(_)) => {
                        shift = aligned_shift(align, free, false);
                        if free < Au::ZERO && align.is_some() {
                            let outer =
                                height + margin_top.unwrap_or(Au::ZERO) + margin_bottom.unwrap_or(Au::ZERO);
                            shift = kept_in(top, shift, outer, cb_height);
                        }
                    }
                    _ => {}
                }
                top + margin_top.unwrap_or(Au::ZERO) + shift
            }
            (Some(top), None) => top + margin_top.unwrap_or(Au::ZERO),
            (None, Some(bottom)) => cb_height - bottom - margin_bottom.unwrap_or(Au::ZERO) - height,
            // At its static position, the box is aligned on it: `end` ends it there, `center`
            // centers it on it (the static-position rectangle has no height).
            (None, None) => {
                let outer = margin_top.unwrap_or(Au::ZERO) + height + margin_bottom.unwrap_or(Au::ZERO);
                // Flex and grid containers align their children themselves.
                let align = align
                    .filter(|_| !self.grid_aligned.get() && !self.flex_statics.borrow().contains_key(&id));
                static_position.1 + margin_top.unwrap_or(Au::ZERO) + aligned_shift(align, -outer, false)
            }
        };
        // `justify-self` aligns a box that was block-level in a block container across the line
        // its static position is on.
        if left.is_none()
            && style.inset.right.is_auto()
            && horizontal
            && let Some(&(line, fraction)) = self.block_statics.borrow().get(&id)
        {
            let shift = Au::from_px((line - width - margins_x).to_px() * fraction);
            x += if rtl_static { -shift } else { shift };
        }
        // The static position of a child of a flex container is where the container would
        // align it as its only item (CSS Flexbox 1, §4.1).
        // A grid container that is the containing block aligns its children itself (see
        // `crate::grid_positioned`).
        let flex_static = (static_ends.is_none() && !self.grid_aligned.get())
            .then(|| self.flex_statics.borrow().get(&id).copied())
            .flatten();
        if let Some(aligned) = flex_static {
            if left.is_none() && style.inset.right.is_auto() {
                let content_left = if rtl_static {
                    static_position.0 - aligned.size.0
                } else {
                    static_position.0
                };
                // `safe` alignment keeps the box from starting before its containing block, as
                // in Chrome (WPT `flex-abspos-staticpos-align-self-safe-*`).
                let outer = width + margins_x;
                let free = aligned.size.0 - outer;
                let margin = margin_left.unwrap_or(Au::ZERO);
                x = content_left + margin + Au::from_px(free.to_px() * aligned.fraction.0);
                match aligned.safe.0 {
                    Some(true) => x = x.min(cb_width - outer + margin),
                    Some(false) => x = x.max(margin),
                    None => {}
                }
            }
            if top.is_none() && bottom.is_none() {
                let outer = height + margin_top.unwrap_or(Au::ZERO) + margin_bottom.unwrap_or(Au::ZERO);
                let free = aligned.size.1 - outer;
                let margin = margin_top.unwrap_or(Au::ZERO);
                y = static_position.1 + margin + Au::from_px(free.to_px() * aligned.fraction.1);
                if aligned.safe.1.is_some() {
                    y = y.max(margin);
                }
            }
        }
        fragment.rect.x = x;
        fragment.rect.y = y;
        fragment
    }

    /// Lays out the items of a flex container whose content box is `width` wide and, if
    /// `height` is given, that tall at least, and no taller than `max_height` (CSS Flexbox 1,
    /// §9). Returns the fragments, the content height and the flex lines.
    fn flex(
        &self,
        style: &ComputedStyle,
        children: &[BoxId],
        width: Au,
        (height, max_height): (Option<Au>, Option<Au>),
    ) -> (Vec<Fragment>, Au, Vec<ContainerLine>) {
        let given = self.given_height.replace(false);
        let mut order: Vec<BoxId> = children.to_vec();
        order.sort_by_key(|&child| self.tree.get(child).style.order);
        let gap = |value: &Option<LengthPercentage>, basis: Au| {
            Au::from_px(value.as_ref().map_or(0.0, |gap| gap.resolve(basis.to_px())))
        };
        // Percentages of `row-gap` refer to the height of the content box, nothing where that is
        // not definite (CSS Box Alignment 3, §8.1); a height from an aspect ratio is definite.
        let definite = given || matches!(style.height, Size::Length(_)) || has_ratio_height(style);
        let row_basis = height.filter(|_| definite).unwrap_or(Au::ZERO);
        let (column_gap, row_gap) = (gap(&style.column_gap, width), gap(&style.row_gap, row_basis));
        match style.flex_direction {
            FlexDirection::Column | FlexDirection::ColumnReverse => {
                // A column with a definite or a maximum height wraps into flex lines side by
                // side.
                let column_height = crate::flex_column::ColumnHeight {
                    definite: height.filter(|_| definite),
                    min: height.filter(|_| !definite).unwrap_or(Au::ZERO),
                    max: max_height,
                };
                if style.flex_wrap != FlexWrap::NoWrap
                    && (column_height.limit().is_some() || style.flex_balance)
                {
                    return self.wrapped_column(style, &order, width, column_height, (column_gap, row_gap));
                }
                if style.flex_direction == FlexDirection::ColumnReverse {
                    order.reverse();
                }
                let (fragments, content_height, boxes) =
                    self.flex_column_items(style, &order, width, (height, given, max_height), row_gap);
                // The items of a column stack in the line (see `crate::flex_column`).
                let line = ContainerLine {
                    items: 0..fragments.len(),
                    top: Au::ZERO,
                    bottom: content_height,
                    boxes,
                };
                (fragments, content_height, vec![line])
            }
            FlexDirection::Row | FlexDirection::RowReverse => self.flex_rows(
                style,
                &order,
                width,
                (height, given, max_height),
                column_gap,
                row_gap,
            ),
        }
    }

    /// The items of a column flex container, stacked without margin collapsing; they grow into
    /// the container's height and are justified in it. Returns their fragments, the content
    /// height and how they are laid out again where the column breaks across pages (an item
    /// that grew counts as stretched). `given` says that `height` is one the container was
    /// given (between the insets of an absolutely positioned box, or stretched), which is
    /// definite like a height of its own; items shrink into `max_height` where they are taller.
    fn flex_column_items(
        &self,
        style: &ComputedStyle,
        order: &[BoxId],
        width: Au,
        (height, given, max_height): (Option<Au>, bool, Option<Au>),
        row_gap: Au,
    ) -> (Vec<Fragment>, Au, Vec<LineItem>) {
        let mut laid: Vec<(BoxId, Option<Au>, Fragment, Au)> = Vec::new();
        let mut grown: Vec<bool> = Vec::new();
        let mut y = Au::ZERO;
        for &child in order {
            if !laid.is_empty() {
                y += row_gap;
            }
            let child_style = &self.tree.get(child).style;
            let width_override = self.cross_size(style, child, width);
            // A percentage or content-based flex basis in a column whose height is its content's
            // is the item's content height, which percentages inside do not refer to (Blink's
            // `is_initial_block_size_indefinite`; WPT `css-flexbox/percentage-heights-016`).
            if height.is_none()
                && (matches!(&child_style.flex_basis, Size::Length(length) if length.fixed().is_none())
                    || matches!(
                        child_style.flex_basis,
                        Size::MinContent | Size::MaxContent | Size::FitContent(_)
                    ))
                && matches!(
                    self.tree.get(child).kind,
                    BoxKind::Block { .. } | BoxKind::Inline { .. }
                )
                && !child_style.is_multicol()
            {
                self.indefinite_basis.set(true);
            }
            let Some((outcome, basis)) = self.column_item(child, (width, None), width_override) else {
                continue;
            };
            let mut fragment = outcome.fragment;
            let margin_top = self.vertical_margin(child_style, width, true);
            fragment.rect.y = y + margin_top;
            y = fragment.rect.y + fragment.rect.height + outcome.margin_bottom;
            laid.push((child, width_override, fragment, outcome.margin_bottom));
            grown.push(basis);
        }
        // Flexible lengths along the column (CSS Flexbox 1, §9.7): free space goes to items that
        // grow; in a fixed height, items that overflow it shrink, no further than their minimums.
        // A column whose height is its content's is as tall as its items' hypothetical heights
        // (their flex bases within their minimums and maximums), which they take.
        {
            let definite = height.is_some()
                && (given || matches!(style.height, Size::Length(_)) || has_ratio_height(style));
            let lengths: Vec<crate::flex_sizing::FlexLength> = laid
                .iter()
                .map(|(child, _, fragment, margin_bottom)| {
                    let margin_top = self.vertical_margin(&self.tree.get(*child).style, width, true);
                    let laid_out = margin_top + fragment.rect.height + *margin_bottom;
                    let content =
                        margin_top + self.item_content_height(*child, width, fragment) + *margin_bottom;
                    self.column_flex_length(
                        *child,
                        width,
                        height.filter(|_| definite),
                        (laid_out, content, crate::flex_column::captions_height(fragment)),
                    )
                })
                .collect();
            let gaps = row_gap * (laid.len().saturating_sub(1) as i32);
            let hypothetical: Au = lengths
                .iter()
                .map(crate::flex_sizing::FlexLength::hypothetical)
                .sum();
            let targets: Vec<Au> = match (height, max_height) {
                (Some(height), _) if definite || hypothetical < height - gaps => {
                    crate::flex_sizing::resolve(&lengths, height - gaps)
                }
                // Items taller together than the maximum height shrink into it (CSS Flexbox 1,
                // §9.7; WPT `css-flexbox/percentage-heights-015`).
                (_, Some(max)) if !definite && hypothetical > max - gaps => {
                    crate::flex_sizing::resolve(&lengths, (max - gaps).clamp_non_negative())
                }
                _ => lengths
                    .iter()
                    .map(crate::flex_sizing::FlexLength::hypothetical)
                    .collect(),
            };
            let mut position = Au::ZERO;
            for (index, (((child, width_override, fragment, margin_bottom), grown), target)) in
                laid.iter_mut().zip(&mut grown).zip(targets).enumerate()
            {
                if index > 0 {
                    position += row_gap;
                }
                let margin_top = self.vertical_margin(&self.tree.get(*child).style, width, true);
                let natural = margin_top + fragment.rect.height + *margin_bottom;
                // The height of an item after flexing in a container of a definite height is
                // definite, as in Chrome: percentage heights of its children refer to it. So is
                // that of an item with a definite flex basis in a column whose height is its
                // content's (Blink's `is_initial_block_size_indefinite`).
                let definite_item = if height.is_some() {
                    definite
                } else {
                    self.definite_column_basis(*child)
                };
                if target != natural
                    || (definite_item
                        && (self.has_percentage_heights(*child)
                            || self.column_items_hold_percentages(*child)))
                {
                    let border_box = (target - margin_top - *margin_bottom).clamp_non_negative();
                    // An item of an indefinite flex basis in a column whose height is its
                    // content's keeps percentages inside indefinite at its flexed height (see
                    // `Layouter::indefinite_basis`).
                    if height.is_none()
                        && !self.definite_column_basis(*child)
                        && matches!(
                            self.tree.get(*child).kind,
                            BoxKind::Block { .. } | BoxKind::Inline { .. }
                        )
                        && !self.tree.get(*child).style.is_multicol()
                    {
                        self.indefinite_basis.set(true);
                    }
                    if let Some(outcome) = self.item(*child, width, *width_override, Some(border_box)) {
                        let x = fragment.rect.x;
                        *fragment = outcome.fragment;
                        // A box no shorter than its content takes the height it is given; a
                        // table is never shorter than its rows and captions.
                        if !matches!(self.tree.get(*child).kind, BoxKind::Table { .. }) {
                            fragment.rect.height = border_box;
                        }
                        fragment.rect.x = x;
                        *grown = true;
                    }
                }
                fragment.rect.y = position + margin_top;
                position = fragment.rect.y + fragment.rect.height + *margin_bottom;
            }
            y = position;
        }
        // Auto margins and alignment across the column.
        for (child, _, fragment, _) in &mut laid {
            fragment.rect.x = self.cross_offset(style, *child, width, fragment.rect.width);
        }
        let boxes: Vec<LineItem> = laid
            .iter()
            .zip(grown)
            .map(|((child, width_override, _, margin_bottom), grown)| LineItem {
                id: *child,
                containing_width: width,
                width: *width_override,
                containing_height: None,
                stretched: grown,
                grows: true,
                margins: (
                    self.vertical_margin(&self.tree.get(*child).style, width, true),
                    *margin_bottom,
                ),
            })
            .collect();
        let mut fragments: Vec<Fragment> = laid.into_iter().map(|(_, _, fragment, _)| fragment).collect();
        // Free room along a fixed or minimum height goes to auto margins, or else is
        // distributed by justify-content (CSS Flexbox 1, §8.1, §9.5).
        if let Some(height) = height {
            let free = height - y;
            let autos: Vec<(bool, bool)> = boxes
                .iter()
                .map(|item| {
                    let margin = &self.tree.get(item.id).style.margin;
                    (margin.top.is_auto(), margin.bottom.is_auto())
                })
                .collect();
            let count: usize = autos
                .iter()
                .map(|&(top, bottom)| usize::from(top) + usize::from(bottom))
                .sum();
            if count > 0 && free > Au::ZERO {
                let share = free / i32::try_from(count).unwrap_or(i32::MAX);
                let mut shift = Au::ZERO;
                for (fragment, (top, bottom)) in fragments.iter_mut().zip(autos) {
                    if top {
                        shift += share;
                    }
                    fragment.rect.y += shift;
                    if bottom {
                        shift += share;
                    }
                }
            } else {
                let (start, between) = justify(style, free, fragments.len());
                for (index, fragment) in fragments.iter_mut().enumerate() {
                    fragment.rect.y += start + between * index as i32;
                }
            }
        }
        (fragments, y, boxes)
    }

    /// The items of a row flex container in flex lines (CSS Flexbox 1, §9.3), with flexible
    /// lengths resolved per line (§9.7).
    #[allow(clippy::too_many_lines, reason = "the steps of the flex layout algorithm")]
    fn flex_rows(
        &self,
        style: &ComputedStyle,
        order: &[BoxId],
        width: Au,
        (height, given, max_height): (Option<Au>, bool, Option<Au>),
        column_gap: Au,
        row_gap: Au,
    ) -> (Vec<Fragment>, Au, Vec<ContainerLine>) {
        // The cross size of a single line in a container of a fixed height (or one with size
        // containment).
        let contained = height.filter(|_| style.size_containment().1 && matches!(style.height, Size::Auto));
        let definite_height = height
            .filter(|_| {
                (given || matches!(style.height, Size::Length(_)) || has_ratio_height(style))
                    && style.flex_wrap == FlexWrap::NoWrap
            })
            .or(contained);
        // Percentage heights of the items refer to the container's height where that is
        // definite: a height of its own, one from its aspect ratio or one its container gave it,
        // not a minimum height.
        let percent_basis =
            height.filter(|_| given || matches!(style.height, Size::Length(_)) || has_ratio_height(style));
        let items: Vec<Item> = order
            .iter()
            .map(|&child| {
                let child_style = &self.tree.get(child).style;
                let model = self.box_model(child_style, width, None);
                let (margin_left, margin_right) = self.flex_margins(child_style, width);
                let frame = model.border[LEFT]
                    + model.border[RIGHT]
                    + model.padding[LEFT]
                    + model.padding[RIGHT]
                    + margin_left
                    + margin_right;
                let cross = self.definite_cross(style, child, width, (definite_height, percent_basis));
                // An item whose lines run across the container's is as long as the container is
                // high, where that is definite.
                let (min_content, max_content) = if self.is_orthogonal(child) {
                    self.with_cross_room(height, || {
                        self.fixed_width(child)
                            .map_or_else(|| self.content_widths(child), |width| (width, width))
                    })
                } else {
                    self.stretched_intrinsic(child, cross)
                };
                let length = self.flex_length(child, width, frame, (min_content, max_content), cross);
                Item {
                    id: child,
                    base: length.hypothetical(),
                    frame,
                    main: length.hypothetical(),
                    length,
                }
            })
            .collect();

        // Items go into lines while they fit; without wrapping all are on one line.
        let mut lines: Vec<Vec<Item>> = Vec::new();
        if style.flex_wrap == FlexWrap::NoWrap {
            lines.push(items);
        } else if let Some(count) = crate::flex_column::balance(style) {
            let bases: Vec<Au> = items.iter().map(|item| item.base).collect();
            for range in crate::flex_balance::balanced_lines(&bases, width, column_gap, count) {
                lines.push(items[range].to_vec());
            }
        } else {
            let mut current: Vec<Item> = Vec::new();
            let mut used = Au::ZERO;
            for item in items {
                let gap = if current.is_empty() { Au::ZERO } else { column_gap };
                if !current.is_empty() && used + gap + item.base > width {
                    lines.push(std::mem::take(&mut current));
                    used = Au::ZERO;
                }
                used += if current.is_empty() { Au::ZERO } else { column_gap } + item.base;
                current.push(item);
            }
            if !current.is_empty() {
                lines.push(current);
            }
        }
        let single = lines.len() == 1 && style.flex_wrap == FlexWrap::NoWrap;

        // What `align-content` adds to the height of each line (see below).
        let mut extra: Vec<Au> = Vec::new();
        let (mut fragments, y, mut boundaries) = loop {
            let mut fragments = Vec::new();
            let mut boundaries = Vec::new();
            let mut y = Au::ZERO;
            for (number, mut items) in lines.iter().cloned().enumerate() {
                if number > 0 {
                    y += row_gap;
                }
                let gaps = column_gap * (items.len().saturating_sub(1) as i32);
                let lengths: Vec<crate::flex_sizing::FlexLength> =
                    items.iter().map(|item| item.length).collect();
                for (item, main) in items
                    .iter_mut()
                    .zip(crate::flex_sizing::resolve(&lengths, width - gaps))
                {
                    item.main = main;
                }

                // Lay out items at their main size; the line is as tall as its tallest item, or as
                // the container if it has a single line.
                let mut laid_out: Vec<(Fragment, Au)> = items
                    .iter()
                    .filter_map(|item| {
                        let child_style = &self.tree.get(item.id).style;
                        let content = (item.main - item.frame).clamp_non_negative();
                        let outcome = self.with_cross_room(height, || {
                            self.item_in(item.id, (width, percent_basis), Some(content), None)
                        })?;
                        let margin_top = self.vertical_margin(child_style, width, true);
                        Some((outcome.fragment, margin_top + outcome.margin_bottom))
                    })
                    .collect();
                // Items aligned by their baselines share one: each one's first baseline below the
                // top of its margin box, or without lines its bottom border edge (CSS Flexbox 1, §8.3).
                let baselines: Vec<Option<Au>> = laid_out
                    .iter()
                    .zip(&items)
                    .map(|((fragment, _), item)| {
                        let child_style = &self.tree.get(item.id).style;
                        (item_alignment(style, child_style) == ItemAlignment::Baseline).then(|| {
                            self.vertical_margin(child_style, width, true)
                                + first_baseline(fragment, self.writing_mode())
                                    .unwrap_or(fragment.rect.height)
                        })
                    })
                    .collect();
                let above = baselines.iter().flatten().copied().max().unwrap_or(Au::ZERO);
                let below = baselines
                    .iter()
                    .zip(&laid_out)
                    .filter_map(|(baseline, (fragment, margins))| {
                        baseline.map(|baseline| fragment.rect.height + *margins - baseline)
                    })
                    .max()
                    .unwrap_or(Au::ZERO);
                // Items aligned by their last baselines share one too, at the end of the line:
                // each one's room below its last baseline, to the bottom of its margin box.
                let lasts: Vec<Option<Au>> = laid_out
                    .iter()
                    .zip(&items)
                    .map(|((fragment, margins), item)| {
                        let child_style = &self.tree.get(item.id).style;
                        (item_alignment(style, child_style) == ItemAlignment::LastBaseline).then(|| {
                            let margin_top = self.vertical_margin(child_style, width, true);
                            fragment.rect.height + *margins
                                - margin_top
                                - last_alignment_baseline(fragment, self.writing_mode())
                                    .unwrap_or(fragment.rect.height)
                        })
                    })
                    .collect();
                let below_last = lasts.iter().flatten().copied().max().unwrap_or(Au::ZERO);
                let above_last = lasts
                    .iter()
                    .zip(&laid_out)
                    .filter_map(|(last, (fragment, margins))| {
                        last.map(|last| fragment.rect.height + *margins - last)
                    })
                    .max()
                    .unwrap_or(Au::ZERO);
                let content_cross = laid_out
                    .iter()
                    .map(|(fragment, margins)| fragment.rect.height + *margins)
                    .max()
                    .unwrap_or(Au::ZERO)
                    .max(above + below)
                    .max(above_last + below_last);
                // A single line is as tall as a container of a definite height, however tall its
                // items (CSS Flexbox 1, §9.4 step 8), and within its minimum and maximum heights.
                // A height from an aspect ratio is at least the content's (the automatic minimum
                // size, CSS Box Sizing 4, §5.1), as in Chrome: the items' heights where they do not
                // follow that height themselves (items with a ratio stretched to it).
                let ratio_minimum = (!given
                    && has_ratio_height(style)
                    && matches!(style.min_height, Size::Auto)
                    && !crate::sizing::is_scroll_container(style))
                .then(|| {
                    laid_out
                        .iter()
                        .zip(&items)
                        .filter(|(_, item)| {
                            let child_style = &self.tree.get(item.id).style;
                            let ratio = crate::ratio::ratio(child_style).is_some()
                                || matches!(self.tree.get(item.id).kind, BoxKind::Replaced { .. });
                            !(ratio
                                && matches!(child_style.height, Size::Auto)
                                && matches!(
                                    item_alignment(style, child_style),
                                    ItemAlignment::Stretch | ItemAlignment::Normal
                                ))
                        })
                        .map(|((fragment, margins), _)| fragment.rect.height + *margins)
                        .max()
                        .unwrap_or(Au::ZERO)
                });
                let line_height = match (single, definite_height) {
                    (true, Some(height)) if let Some(content) = ratio_minimum => {
                        content.min(max_height.unwrap_or(Au::MAX)).max(height)
                    }
                    (true, Some(height)) => height,
                    (true, None) => content_cross
                        .min(max_height.unwrap_or(Au::MAX))
                        .max(height.unwrap_or(Au::ZERO)),
                    (false, _) => content_cross + extra.get(number).copied().unwrap_or(Au::ZERO),
                };
                // Stretch: items with an automatic height take the line's height. A table counts as
                // stretched also where it is as tall as the line: its rows grow where its breaks
                // take room from it (see `parallel::split_line`).
                let mut stretched = vec![false; items.len()];
                for (index, item) in items.iter().enumerate() {
                    let child_style = &self.tree.get(item.id).style;
                    let table = matches!(self.tree.get(item.id).kind, BoxKind::Table { .. });
                    let stretches = index < laid_out.len()
                        && matches!(
                            item_alignment(style, child_style),
                            ItemAlignment::Stretch | ItemAlignment::Normal
                        )
                        && matches!(child_style.height, Size::Auto)
                        && !child_style.margin.top.is_auto()
                        && !child_style.margin.bottom.is_auto();
                    // An item as tall as the line has the height it would be stretched to; it counts
                    // as stretched where it breaks, which a height of its own makes it do below the
                    // bottom margin of its last child, as in Chrome.
                    // Percentage heights of its children refer to the stretched height, as in
                    // Chrome (which lays stretched items out again at it, as a fixed height).
                    let percentages = stretches
                        && (self.has_percentage_heights(item.id)
                            || self.column_items_hold_percentages(item.id));
                    if stretches
                        && !table
                        && !percentages
                        && laid_out[index].0.rect.height + laid_out[index].1 == line_height
                    {
                        stretched[index] = true;
                        continue;
                    }
                    if stretches
                        && (percentages
                            || laid_out[index].0.rect.height + laid_out[index].1 < line_height
                            || (table && laid_out[index].0.rect.height + laid_out[index].1 == line_height)
                            || (single
                                && definite_height.is_some()
                                && laid_out[index].0.rect.height + laid_out[index].1 > line_height))
                    {
                        // In a line of a definite height, a stretched item may also be shorter than
                        // its content, which overflows it. (In a single line that a maximum height
                        // makes shorter than its items, one grows to its content as Chrome's
                        // items do where they break, unless its children's heights are
                        // percentages of its own.)
                        let margins = laid_out[index].1;
                        let content = (item.main - item.frame).clamp_non_negative();
                        let target = (line_height - margins).clamp_non_negative();
                        if let Some(outcome) =
                            self.item_in(item.id, (width, percent_basis), Some(content), Some(target))
                        {
                            laid_out[index].0 = outcome.fragment;
                            // A table is never shorter than its rows and captions.
                            if !table {
                                laid_out[index].0.rect.height = target;
                            }
                            stretched[index] = true;
                        }
                    }
                }
                let used: Au = laid_out
                    .iter()
                    .zip(&items)
                    .map(|((fragment, _), item)| {
                        let (left, right) = self.flex_margins(&self.tree.get(item.id).style, width);
                        fragment.rect.width + left + right
                    })
                    .sum::<Au>()
                    + gaps;
                // Free room goes to auto margins in the main axis, or else is distributed by
                // justify-content (CSS Flexbox 1, §8.1, §9.5 step 12), which also aligns items
                // that overflow the line.
                let free = width - used;
                let autos = items
                    .iter()
                    .map(|item| {
                        let margin = &self.tree.get(item.id).style.margin;
                        usize::from(margin.left.is_auto()) + usize::from(margin.right.is_auto())
                    })
                    .sum::<usize>();
                let autos = if free > Au::ZERO { autos } else { 0 };
                let auto_share = if autos > 0 {
                    free / i32::try_from(autos).unwrap_or(i32::MAX)
                } else {
                    Au::ZERO
                };
                let (mut x, between) = if autos > 0 {
                    (Au::ZERO, Au::ZERO)
                } else {
                    justify(style, free, laid_out.len())
                };
                let first = fragments.len();
                let boxes = items
                    .iter()
                    .zip(&stretched)
                    .map(|(item, &stretched)| {
                        let child_style = &self.tree.get(item.id).style;
                        LineItem {
                            id: item.id,
                            containing_width: width,
                            width: Some((item.main - item.frame).clamp_non_negative()),
                            // Percentage heights refer to the container's definite height where
                            // the item breaks too (a flex or grid container keeps the height it
                            // is stretched to instead, see `Layouter::item_part`).
                            containing_height: percent_basis.filter(|_| {
                                !matches!(
                                    self.tree.get(item.id).kind,
                                    BoxKind::Flex { .. } | BoxKind::Grid { .. }
                                )
                            }),
                            stretched,
                            grows: true,
                            margins: (
                                self.vertical_margin(child_style, width, true),
                                self.vertical_margin(child_style, width, false),
                            ),
                        }
                    })
                    .collect();
                // Main-start is on the right in a reversed row, and in a row of right-to-left text.
                let mirrored = (style.flex_direction == FlexDirection::RowReverse) != style.is_rtl();
                for ((((mut fragment, margins), item), baseline), last) in
                    laid_out.into_iter().zip(&items).zip(baselines).zip(lasts)
                {
                    let child_style = &self.tree.get(item.id).style;
                    let (mut left, mut right) = self.flex_margins(child_style, width);
                    if child_style.margin.left.is_auto() {
                        left = auto_share;
                    }
                    if child_style.margin.right.is_auto() {
                        right = auto_share;
                    }
                    let margin_top = self.vertical_margin(child_style, width, true);
                    fragment.rect.x = x + if mirrored { right } else { left };
                    let free_cross = line_height - fragment.rect.height - margins;
                    // Auto margins take the free room before any alignment, and none where the item
                    // overflows its line (CSS Flexbox 1, §8.1).
                    let auto_margins = (
                        child_style.margin.top.is_auto(),
                        child_style.margin.bottom.is_auto(),
                    );
                    fragment.rect.y = y
                        + margin_top
                        + match (auto_margins, cross_alignment(style, child_style, free_cross)) {
                            ((true, true), _) => free_cross.clamp_non_negative() / 2,
                            ((true, false), _) => free_cross.clamp_non_negative(),
                            ((false, true), _) => Au::ZERO,
                            (_, ItemAlignment::Center) => free_cross / 2,
                            (_, ItemAlignment::End) => free_cross,
                            (_, ItemAlignment::Baseline) => above - baseline.unwrap_or(above),
                            (_, ItemAlignment::LastBaseline) => {
                                free_cross - (below_last - last.unwrap_or(below_last))
                            }
                            _ => Au::ZERO,
                        };
                    x += fragment.rect.width + left + right + column_gap + between;
                    if mirrored {
                        fragment.rect.x = width - fragment.rect.x - fragment.rect.width;
                    }
                    fragments.push(fragment);
                }
                boundaries.push(ContainerLine {
                    items: first..fragments.len(),
                    top: y,
                    bottom: y + line_height,
                    boxes,
                });
                y += line_height;
            }
            // The lines of a container that wraps share out the room its height leaves below them
            // by `align-content` (CSS Flexbox 1, §9.4 step 15, §8.4): they stretch by default (laid
            // out again, their items stretching with them), or move.
            let count = i32::try_from(lines.len()).unwrap_or(i32::MAX);
            let free = height.map_or(Au::ZERO, |height| height - y);
            let wraps = style.flex_wrap != FlexWrap::NoWrap;
            if wraps
                && extra.is_empty()
                && count > 0
                && free > Au::ZERO
                && matches!(
                    style.align_content,
                    ContentAlignment::Normal | ContentAlignment::Stretch
                )
            {
                extra = vec![free / count; lines.len()];
                continue;
            }
            if wraps && count > 0 && free != Au::ZERO {
                let (start, between) = line_shifts(style, free, lines.len());
                for (number, line) in boundaries.iter_mut().enumerate() {
                    let shift = start + between * i32::try_from(number).unwrap_or(i32::MAX);
                    line.top += shift;
                    line.bottom += shift;
                    for fragment in &mut fragments[line.items.clone()] {
                        fragment.rect.y += shift;
                    }
                }
            }
            break (fragments, y, boundaries);
        };
        if style.flex_wrap == FlexWrap::WrapReverse {
            // Cross-start is at the bottom (of the container's height, where that is definite):
            // the lines stack upwards, and so do the margin boxes of the items in them.
            // Items aligned by their baselines keep their places to each other, as in Chrome:
            // those aligned by their first baselines at cross-start (the bottom), those by their
            // last ones at cross-end.
            let end = height.unwrap_or(y);
            for line in &boundaries {
                let alignment = |item: &LineItem| item_alignment(style, &self.tree.get(item.id).style);
                let laid = || fragments[line.items.clone()].iter().zip(&line.boxes);
                let first_extent = laid()
                    .filter(|(_, item)| alignment(item) == ItemAlignment::Baseline)
                    .map(|(fragment, item)| {
                        fragment.rect.y + fragment.rect.height + item.margins.1 - line.top
                    })
                    .max()
                    .unwrap_or(Au::ZERO);
                let last_start = laid()
                    .filter(|(_, item)| alignment(item) == ItemAlignment::LastBaseline)
                    .map(|(fragment, item)| fragment.rect.y - item.margins.0 - line.top)
                    .min()
                    .unwrap_or(Au::ZERO);
                for (fragment, item) in fragments[line.items.clone()].iter_mut().zip(&line.boxes) {
                    let (top, bottom) = item.margins;
                    let offset = fragment.rect.y - line.top;
                    fragment.rect.y = match alignment(item) {
                        ItemAlignment::Baseline => end - line.top - first_extent + offset,
                        ItemAlignment::LastBaseline => end - line.bottom + offset - last_start,
                        _ => end - fragment.rect.y - fragment.rect.height - bottom + top,
                    };
                }
            }
            for line in &mut boundaries {
                (line.top, line.bottom) = (end - line.bottom, end - line.top);
            }
            boundaries.reverse();
        }
        (fragments, y, boundaries)
    }

    /// The left and right margins of a flex item, auto margins as zero: they take free room
    /// only once the items are sized (CSS Flexbox 1, §8.1).
    pub(crate) fn flex_margins(&self, style: &ComputedStyle, containing_width: Au) -> (Au, Au) {
        let resolve =
            |margin: &LengthPercentageAuto| Au::from_px(margin.resolve_or(containing_width.to_px(), 0.0));
        (resolve(&style.margin.left), resolve(&style.margin.right))
    }

    pub(crate) fn vertical_margin(&self, style: &ComputedStyle, containing_width: Au, top: bool) -> Au {
        let margin = if top {
            &style.margin.top
        } else {
            &style.margin.bottom
        };
        Au::from_px(margin.resolve_or(containing_width.to_px(), 0.0))
    }

    /// The width a table whose content box is `content` wide takes with its border, padding and
    /// margins, where auto margins are zero: the containing block it fills as a float, inline
    /// box, flex or grid item (see `Layouter::table_fills`). Its box model would give an auto
    /// margin what a `max-width` leaves over, and the table would fill that too.
    fn table_outer_width(&self, style: &ComputedStyle, containing_width: Au, content: Au) -> Au {
        let model = self.box_model(style, containing_width, Some(content));
        let basis = containing_width.to_px();
        let margin = |margin: &LengthPercentageAuto| Au::from_px(margin.resolve_or(basis, 0.0));
        content
            + model.border[LEFT]
            + model.border[RIGHT]
            + model.padding[LEFT]
            + model.padding[RIGHT]
            + margin(&style.margin.left)
            + margin(&style.margin.right)
    }

    /// Lays out a flex or grid item or table cell with a fixed content width.
    pub(crate) fn item(
        &self,
        id: BoxId,
        containing_width: Au,
        content_width: Option<Au>,
        height: Option<Au>,
    ) -> Option<Outcome> {
        self.item_in(id, (containing_width, None), content_width, height)
    }

    /// Like [`Layouter::item`], in a containing block whose height may be definite (a grid
    /// area), which percentage heights of the item refer to.
    pub(crate) fn item_in(
        &self,
        id: BoxId,
        (containing_width, containing_height): (Au, Option<Au>),
        content_width: Option<Au>,
        height: Option<Au>,
    ) -> Option<Outcome> {
        let _whole = self.enter_box(&self.tree.get(id).style);
        let space = Space {
            containing_height,
            ..Space::unfragmented(containing_width)
        };
        match &self.tree.get(id).kind {
            BoxKind::Block { .. } if self.tree.get(id).style.is_multicol() => {
                self.multicol_sized(id, space, None, (content_width, height))
            }
            BoxKind::Block { .. } | BoxKind::Inline { .. } => {
                self.block_container_with(id, space, None, content_width, height, true)
            }
            // A table's width is that of the table box: an item width becomes its containing
            // block, so that `width: 100%` fills the item.
            BoxKind::Table { .. } => {
                let width = content_width.map(|content| {
                    self.table_outer_width(&self.tree.get(id).style, containing_width, content)
                });
                let space = width.map_or(space, Space::unfragmented);
                // A table given a height takes it instead of its own, and is no shorter than
                // its rows and captions.
                self.table_fills.set(width.is_some());
                self.table_unsized.set(height.is_some());
                let outcome = self.table(id, space, None)?;
                // A table given more height than its rows take grows its rows to fill it.
                match height {
                    Some(height) if height > outcome.fragment.rect.height => {
                        self.table_fills.set(width.is_some());
                        self.table_unsized.set(true);
                        self.table_grow.set(height - outcome.fragment.rect.height);
                        self.table(id, space, None)
                    }
                    _ => Some(outcome),
                }
            }
            BoxKind::Replaced { .. } => {
                Some(self.replaced(id, containing_width, content_width, height, containing_height))
            }
            _ => {
                // A flex or grid container that the item's container leaves its width keeps a
                // sizing keyword of its own (`width: max-content`), as in block flow.
                let content_width =
                    content_width.or_else(|| self.sized_width(id, containing_width, containing_width, false));
                // Whether the item keeps the height it is given is a block container's to know:
                // it does not pass on to the first block inside a flex or grid container.
                self.fixed_item_height.set(false);
                let mut outcome = self.item_container(id, space, content_width, height);
                if let Some(height) = height {
                    outcome.fragment.rect.height = outcome.fragment.rect.height.max(height);
                }
                Some(outcome)
            }
        }
    }
}

/// The `justify-content` of a flex container from where its items are placed from, main-start:
/// `start` and `end` go by the inline or block axis, which a reversed row turns around. The
/// items of a reversed column are stacked from the top in their reversed order, so its
/// main-start, where `flex-start` and `normal` put them, is at the end. `left` and `right` go
/// by the sides of a row, whose main-start is on the right where it is reversed or right to
/// left (but not both); in a column they are `start`.
pub(crate) fn main_justification(style: &ComputedStyle) -> ContentAlignment {
    main_alignment(style, style.justify_content)
}

/// [`main_justification`] of `justify-content: value`.
fn main_alignment(style: &ComputedStyle, value: ContentAlignment) -> ContentAlignment {
    let row_reverse = style.flex_direction == FlexDirection::RowReverse;
    let column_reverse = style.flex_direction == FlexDirection::ColumnReverse;
    let column = column_reverse || style.flex_direction == FlexDirection::Column;
    match value {
        ContentAlignment::Left | ContentAlignment::Right if column => ContentAlignment::Start,
        alignment @ (ContentAlignment::Left | ContentAlignment::Right) => {
            alignment.sided(row_reverse == style.is_rtl())
        }
        ContentAlignment::Start if row_reverse => ContentAlignment::End,
        ContentAlignment::End if row_reverse => ContentAlignment::Start,
        ContentAlignment::FlexStart | ContentAlignment::Normal if column_reverse => ContentAlignment::End,
        ContentAlignment::FlexEnd if column_reverse => ContentAlignment::Start,
        ContentAlignment::FlexStart => ContentAlignment::Start,
        ContentAlignment::FlexEnd => ContentAlignment::End,
        other => other,
    }
}

/// Where the `justify-content` of flex container `style` puts the first of `count` items of a
/// line in `free` room, which is negative where they overflow the line, and the space it adds
/// between them, as in Blink (`InitialContentPositionOffset`): `space-between` falls back to
/// `flex-start` without room or with one item, `space-around` and `space-evenly` to `safe
/// center`, `stretch` to `flex-start`; the others overflow at both ends or at the start.
pub(crate) fn justify(style: &ComputedStyle, free: Au, count: usize) -> (Au, Au) {
    let alignment = main_justification(style);
    let roomless = free <= Au::ZERO;
    // `safe` alignment of items that overflow: at the start.
    if roomless && style.justify_content_safe {
        return distribute(
            main_alignment(style, ContentAlignment::FlexStart),
            free.clamp_non_negative(),
            count,
        );
    }
    match alignment {
        ContentAlignment::SpaceBetween if roomless || count <= 1 => {
            distribute(main_alignment(style, ContentAlignment::FlexStart), free, count)
        }
        ContentAlignment::SpaceAround | ContentAlignment::SpaceEvenly if roomless || count == 0 => {
            (free.clamp_non_negative() / 2, Au::ZERO)
        }
        ContentAlignment::Stretch => {
            distribute(main_alignment(style, ContentAlignment::FlexStart), free, count)
        }
        alignment => distribute(alignment, free, count),
    }
}

/// Where `justify-content` or `align-content` puts the first of `count` items or tracks in `free`
/// space, and the space it adds between them.
pub(crate) fn distribute(alignment: ContentAlignment, free: Au, count: usize) -> (Au, Au) {
    let count = i32::try_from(count).unwrap_or(i32::MAX);
    match alignment {
        ContentAlignment::End | ContentAlignment::FlexEnd => (free, Au::ZERO),
        ContentAlignment::Center => (free / 2, Au::ZERO),
        ContentAlignment::SpaceBetween if count > 1 => (Au::ZERO, free / (count - 1)),
        ContentAlignment::SpaceAround if count > 0 => (free / (count * 2), free / count),
        ContentAlignment::SpaceEvenly if count > 0 => (free / (count + 1), free / (count + 1)),
        _ => (Au::ZERO, Au::ZERO),
    }
}

/// Where the first line of a flex container that wraps starts and how far apart the lines are,
/// from cross-start (the bottom with `wrap-reverse`), where its height leaves `free` room below
/// `count` lines or they overflow it (`free` below zero), by `align-content`, as in Chrome: the
/// lines overflow towards cross-start, the container's top for `start`, or both ways for
/// `center`, unless the alignment is `safe`; the distributed alignments fall back to
/// `flex-start` (`space-between`) and `safe center` (CSS Box Alignment 3, §5.3 and §4.4).
fn line_shifts(style: &ComputedStyle, free: Au, count: usize) -> (Au, Au) {
    // `start` and `end` are the container's top and bottom, at cross-end and cross-start with
    // `wrap-reverse`.
    let reverse = style.flex_wrap == FlexWrap::WrapReverse;
    let (top, bottom) = if reverse {
        (free, Au::ZERO)
    } else {
        (Au::ZERO, free)
    };
    let alignment = style.align_content;
    if free < Au::ZERO {
        return match alignment {
            _ if style.align_content_safe => (top, Au::ZERO),
            ContentAlignment::SpaceAround | ContentAlignment::SpaceEvenly => (top, Au::ZERO),
            ContentAlignment::Start | ContentAlignment::Left | ContentAlignment::Right => (top, Au::ZERO),
            ContentAlignment::End => (bottom, Au::ZERO),
            ContentAlignment::FlexEnd => (free, Au::ZERO),
            ContentAlignment::Center => (free / 2, Au::ZERO),
            _ => (Au::ZERO, Au::ZERO),
        };
    }
    match alignment {
        ContentAlignment::Start | ContentAlignment::Left | ContentAlignment::Right => (top, Au::ZERO),
        ContentAlignment::End => (bottom, Au::ZERO),
        ContentAlignment::FlexStart | ContentAlignment::Normal | ContentAlignment::Stretch => {
            (Au::ZERO, Au::ZERO)
        }
        _ => distribute(alignment, free, count),
    }
}

/// A line of a flex or grid container: the fragments of its items and its extent, from the top
/// of the container's content box. A container breaks between its lines, and inside a line
/// whose items are in `boxes` (one per fragment), where it reaches past the end of a page.
pub(crate) struct ContainerLine {
    pub(crate) items: std::ops::Range<usize>,
    pub(crate) top: Au,
    pub(crate) bottom: Au,
    pub(crate) boxes: Vec<LineItem>,
}

/// How an item of a flex or grid line is laid out again where the line breaks across pages.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineItem {
    pub(crate) id: BoxId,
    /// The width of its containing block (the container's content box or its grid area) and
    /// its fixed content width, if it has one.
    pub(crate) containing_width: Au,
    pub(crate) width: Option<Au>,
    /// The height of its containing block where percentage heights of the item refer to it (a
    /// grid area, the height of a column flex container that wraps).
    pub(crate) containing_height: Option<Au>,
    /// Whether it was stretched to the height of its line or area.
    pub(crate) stretched: bool,
    /// Whether its line grows where it needs more room after a break: not for a grid item
    /// that spans several rows, as in Chrome.
    pub(crate) grows: bool,
    /// Its top and bottom margins.
    pub(crate) margins: (Au, Au),
}

/// An atomic inline, laid out: its fragment at its margins, the size of its margin box and its
/// baseline from the top of the margin box.
#[derive(Clone)]
pub(crate) struct Atomic {
    pub fragment: Fragment,
    pub width: Au,
    pub height: Au,
    pub baseline: Au,
}

/// The baseline of the first line box in a fragment, from its top, in writing mode `mode`: the
/// lines of boxes whose lines run the other way do not count. A table, also in the wrapper box of
/// its captions, has the baseline of its first row (see [`crate::table::table_baseline`]); its
/// captions do not count, as in Chrome.
pub(crate) fn first_baseline(fragment: &Fragment, mode: WritingMode) -> Option<Au> {
    let baseline = first_baseline_within(fragment, mode)?;
    // A scroll container's baseline lies within its border box: one of content further down
    // is its bottom border edge, as in Blink (WPT `contain-size-block-002`).
    Some(match &fragment.kind {
        FragmentKind::Box { style, .. }
            if crate::sizing::is_scroll_container(style)
                && style.display
                    != fagus_style::values::Display::Internal(
                        fagus_style::values::InternalDisplay::TableCell,
                    ) =>
        {
            baseline.min(fragment.rect.height)
        }
        _ => baseline,
    })
}

/// [`first_baseline`] before a scroll container keeps it within its border box.
fn first_baseline_within(fragment: &Fragment, mode: WritingMode) -> Option<Au> {
    if contains_layout(fragment) {
        return None;
    }
    if is_table(fragment) {
        return crate::table::table_baseline(fragment, mode);
    }
    // The wrapper box holds the table and the boxes of its captions (blocks of their own).
    if matches!(fragment.kind, FragmentKind::Anonymous)
        && fragment.children.iter().any(is_table)
        && fragment
            .children
            .iter()
            .all(|child| matches!(child.kind, FragmentKind::Box { .. }))
    {
        let table = fragment.children.iter().find(|child| is_table(child))?;
        return crate::table::table_baseline(table, mode).map(|baseline| table.rect.y + baseline);
    }
    if let Some((style, grid)) = baseline_container(fragment, mode) {
        return flex_baseline(fragment, style, grid, mode);
    }
    let child_baseline = |child: &Fragment| match &child.kind {
        FragmentKind::Line { .. } if is_phantom_line(child) => None,
        FragmentKind::Line { baseline } => Some(child.rect.y + line_baseline(child, *baseline, mode)),
        FragmentKind::Box { style, .. }
            if !style.is_out_of_flow() && !style.is_floating() && style.writing_mode.is_parallel(mode) =>
        {
            first_baseline(child, mode).map(|baseline| child.rect.y + baseline)
        }
        FragmentKind::Anonymous => first_baseline(child, mode).map(|baseline| child.rect.y + baseline),
        _ => None,
    };
    // The columns and spanners of a multi-column container lie side by side: its first baseline
    // is the highest of theirs, as in Blink (`ColumnLayoutAlgorithm::PropagateBaselineFromChild`).
    if is_multicol(fragment) {
        return fragment.children.iter().filter_map(child_baseline).min();
    }
    fragment.children.iter().find_map(child_baseline)
}

/// Where the baseline of line `line` lies from its top in writing mode `mode`, which places it
/// `baseline` from its over side: in `vertical-lr` that is its bottom, the content of lines being
/// mirrored there (see `crate::writing::physical_rect`).
fn line_baseline(line: &Fragment, baseline: Au, mode: WritingMode) -> Au {
    if mode == WritingMode::VerticalLr {
        line.rect.height - baseline
    } else {
        baseline
    }
}

/// The min-content and max-content widths of the columns of a multi-column container with
/// `style` whose content has the widths `content`: columns side by side, each at least as wide
/// as the content's widest part (CSS Multi-column Layout 1, §3.4), as Blink sizes them: with a
/// `column-width`, the min-content width is no wider than it and takes no column count, and the
/// max-content width is one column of it where the count is `auto`.
fn multicol_widths(style: &ComputedStyle, (mut min, mut max): (Au, Au)) -> (Au, Au) {
    let gap = Au::from_px(
        style
            .column_gap
            .as_ref()
            .map_or(style.font_size, |gap| gap.resolve(0.0))
            .max(0.0),
    );
    let count = style.column_count.map_or(1, |count| count as i32);
    let gaps = gap * (count - 1);
    if let Some(width) = style.column_width {
        let width = Au::from_px(width);
        min = min.min(width);
        max = if style.column_count.is_none() {
            width
        } else {
            max.max(width)
        }
        .max(min);
    } else {
        min = min * count + gaps;
    }
    (min, max * count + gaps)
}

/// Whether a fragment is that of a box with layout containment, which has no baseline: those
/// aligning by it synthesize one from its edges (CSS Contain 2, §3.2).
fn contains_layout(fragment: &Fragment) -> bool {
    matches!(&fragment.kind, FragmentKind::Box { style, .. } if style.layout_containment)
}

/// Whether a fragment is that of a multi-column container, whose children are its columns and
/// spanners.
fn is_multicol(fragment: &Fragment) -> bool {
    matches!(&fragment.kind, FragmentKind::Box { style, .. } if style.is_multicol())
}

/// The style of a fragment that is a flex or grid container in horizontal lines, and whether it
/// is a grid: one whose baseline [`flex_baseline`] gives.
fn baseline_container(fragment: &Fragment, mode: WritingMode) -> Option<(&ComputedStyle, bool)> {
    let FragmentKind::Box { style, .. } = &fragment.kind else {
        return None;
    };
    let fagus_style::values::Display::Normal { inside, .. } = style.display else {
        return None;
    };
    let grid = inside == fagus_style::values::DisplayInside::Grid;
    let container = grid || inside == fagus_style::values::DisplayInside::Flex;
    (container && !mode.is_vertical() && style.writing_mode.is_parallel(mode)).then_some((&**style, grid))
}

/// The first baseline of a flex or grid container's fragment (of style `style`): that of its
/// first flex item aligned by its baseline (in the first line, the items of a fragment coming
/// line by line; not in a column), else of its first item (a reversed row's taken from its end;
/// a grid's fragments come by rows), synthesized from the bottom of the item's border box where
/// it has no line (Blink's `BaselineAccumulator`); `None` without items. An empty icon before a
/// label thus puts the container's baseline at the icon's bottom, as in Chrome.
fn flex_baseline(fragment: &Fragment, style: &ComputedStyle, grid: bool, mode: WritingMode) -> Option<Au> {
    let mut items: Vec<&Fragment> = fragment
        .children
        .iter()
        .filter(|child| match &child.kind {
            FragmentKind::Box { style, .. } => !style.is_out_of_flow(),
            FragmentKind::Anonymous => true,
            _ => false,
        })
        .collect();
    // Blink goes through the items of a reversed row from its end (those of a reversed column
    // come reversed already), and a column's items aligned by their baselines share none.
    if !grid && style.flex_direction == FlexDirection::RowReverse {
        items.reverse();
    }
    let row = !grid
        && matches!(
            style.flex_direction,
            FlexDirection::Row | FlexDirection::RowReverse
        );
    let item_baseline = |item: &Fragment| {
        let parallel = match &item.kind {
            FragmentKind::Box { style, .. } => style.writing_mode.is_parallel(mode),
            _ => true,
        };
        let own = if parallel {
            first_baseline(item, mode)
        } else {
            None
        };
        item.rect.y + own.unwrap_or(item.rect.height)
    };
    let aligned = |item: &&&Fragment| {
        row && matches!(&item.kind, FragmentKind::Box { style: item_style, .. }
            if item_alignment(style, item_style) == ItemAlignment::Baseline)
    };
    let chosen = items.iter().find(aligned).or(items.first());
    chosen.map(|item| item_baseline(item))
}

/// Whether a fragment is the box of a table (not its wrapper box with captions).
pub(crate) fn is_table(fragment: &Fragment) -> bool {
    matches!(&fragment.kind, FragmentKind::Box { style, .. }
    if matches!(style.display, fagus_style::values::Display::Normal {
        inside: fagus_style::values::DisplayInside::Table,
        ..
    }))
}

/// Whether a line fragment holds only floats and absolutely positioned boxes, without height: a
/// line that is not there for baselines (CSS 2.1, §9.4.2).
pub(crate) fn is_phantom_line(line: &Fragment) -> bool {
    line.rect.height == Au::ZERO
        && line.children.iter().all(|child| match &child.kind {
            FragmentKind::Box { style, .. } => style.is_out_of_flow() || style.is_floating(),
            FragmentKind::Placeholder(_) => true,
            _ => false,
        })
}

/// The last baseline of a fragment that items aligned by `last baseline` share, from its top:
/// that of the last item of a flex or grid container (CSS Flexbox 1, §8.5; CSS Grid 2,
/// §10.7), else that of its last line box as [`last_baseline`] finds it.
pub(crate) fn last_alignment_baseline(fragment: &Fragment, mode: WritingMode) -> Option<Au> {
    if let FragmentKind::Box { style, .. } = &fragment.kind
        && matches!(
            style.display,
            fagus_style::values::Display::Normal {
                inside: fagus_style::values::DisplayInside::Flex | fagus_style::values::DisplayInside::Grid,
                ..
            }
        )
    {
        let last = fragment.children.iter().rev().find(|child| match &child.kind {
            FragmentKind::Box { style, .. } => !style.is_out_of_flow(),
            FragmentKind::Anonymous => true,
            _ => false,
        })?;
        return Some(last.rect.y + last_alignment_baseline(last, mode).unwrap_or(last.rect.height));
    }
    last_baseline(fragment, mode)
}

/// The baseline of the last in-flow line box in a fragment, from the fragment's top, in writing
/// mode `mode`, as an inline block takes it: a scroll container (`overflow: hidden`) among the
/// blocks in it gives the bottom of its border box instead of its lines, a flex or grid
/// container its first baseline, and a table none, as in Chrome (Blink's
/// `PropagateBaselineFromBlockChild` for `BaselineAlgorithmType::kInlineBlock`).
fn last_baseline(fragment: &Fragment, mode: WritingMode) -> Option<Au> {
    if contains_layout(fragment) {
        return None;
    }
    let child_baseline = |child: &Fragment| match &child.kind {
        FragmentKind::Line { .. } if is_phantom_line(child) => None,
        FragmentKind::Line { baseline } => Some(child.rect.y + line_baseline(child, *baseline, mode)),
        FragmentKind::Box { style, .. }
            if !style.is_out_of_flow() && !style.is_floating() && style.writing_mode.is_parallel(mode) =>
        {
            if !mode.is_vertical()
                && (style.overflow_x == fagus_style::values::Overflow::Hidden
                    || style.overflow_y == fagus_style::values::Overflow::Hidden)
            {
                return Some(child.rect.y + child.rect.height);
            }
            let own = match style.display {
                fagus_style::values::Display::Normal {
                    inside: fagus_style::values::DisplayInside::Table,
                    ..
                } => None,
                fagus_style::values::Display::Normal {
                    inside:
                        fagus_style::values::DisplayInside::Flex | fagus_style::values::DisplayInside::Grid,
                    ..
                } => first_baseline(child, mode),
                _ => last_baseline(child, mode),
            };
            own.map(|baseline| child.rect.y + baseline)
        }
        FragmentKind::Anonymous => last_baseline(child, mode).map(|baseline| child.rect.y + baseline),
        _ => None,
    };
    // That of a multi-column container is the lowest of its columns and spanners, as in Blink.
    if is_multicol(fragment) {
        return fragment.children.iter().filter_map(child_baseline).max();
    }
    fragment.children.iter().rev().find_map(child_baseline)
}

/// The extents of the text, images and atomic inlines of `fragment` along the line, from
/// `origin`, and the starts of inline boxes with a margin, border or padding there (which count
/// as content before what they hold, as in Blink's `has_non_empty_inline_box_start`): just
/// before their start edge, the left one or in `rtl` lines the right one.
fn line_content(fragment: &Fragment, origin: Au, rtl: bool, out: &mut Vec<(Au, Au)>) {
    for child in &fragment.children {
        let x = origin + child.rect.x;
        match &child.kind {
            FragmentKind::Text(_) | FragmentKind::Image(_) => out.push((x, x + child.rect.width)),
            FragmentKind::Box { .. } if child.children.is_empty() && child.rect.width > Au::ZERO => {
                out.push((x, x + child.rect.width));
            }
            FragmentKind::Box { style, sides, .. } => {
                if style.display == fagus_style::values::Display::INLINE
                    && non_empty_start(style, *sides, rtl)
                {
                    let edge = if rtl {
                        x + child.rect.width + Au::EPSILON * 2
                    } else {
                        x - Au::EPSILON * 2
                    };
                    out.push((edge, edge));
                }
                line_content(child, x, rtl, out);
            }
            FragmentKind::Anonymous => line_content(child, x, rtl, out),
            _ => {}
        }
    }
}

/// Whether an inline box of `style` starts on its line (`sides`) with a margin, border or
/// padding at its start edge, the left one or in `rtl` lines the right one.
fn non_empty_start(style: &ComputedStyle, sides: VisibleSides, rtl: bool) -> bool {
    let (shown, margin, border, padding) = if rtl {
        (
            sides.right,
            &style.margin.right,
            style.border_width.right,
            &style.padding.right,
        )
    } else {
        (
            sides.left,
            &style.margin.left,
            style.border_width.left,
            &style.padding.left,
        )
    };
    let nonzero = |value: &LengthPercentage| *value != LengthPercentage::ZERO;
    shown
        && (border != 0.0
            || nonzero(padding)
            || matches!(margin, LengthPercentageAuto::Length(length) if nonzero(length)))
}

/// Takes the placeholders of boxes that were block-level and follow `content` on the line out
/// of `fragment` (at `origin` in the line, which is `height` tall) into `out`, and moves the
/// others to `edge` along the line, noting those outside positioned inline boxes in `kept`.
/// Inside a positioned inline box (`inside`), their containing block, those that follow content
/// stay in it, moved to the start of the next line: `edge`, at the bottom of the line.
fn take_block_level(
    tree: &BoxTree,
    fragment: &mut Fragment,
    (origin, height, inside): ((Au, Au), Au, bool),
    (content, rtl, edge): (&[(Au, Au)], bool, Au),
    (out, kept): (&mut Vec<BoxId>, &mut Vec<BoxId>),
) {
    fragment.children.retain_mut(|child| {
        let FragmentKind::Placeholder(id) = child.kind else {
            return true;
        };
        if tree.get(id).style.originally_inline {
            return true;
        }
        let x = origin.0 + child.rect.x;
        let follows = content.iter().any(|&(start, end)| {
            if rtl {
                end > x + Au::EPSILON
            } else {
                start + Au::EPSILON < x
            }
        });
        if follows {
            if !inside {
                out.push(id);
                return false;
            }
            child.rect.y = height - origin.1;
        } else if !inside {
            kept.push(id);
        }
        child.rect.x = edge - origin.0;
        true
    });
    for child in &mut fragment.children {
        let descend = match &child.kind {
            FragmentKind::Box { style, .. } => (!style.contains_absolute()
                || crate::inline_containing::is_inline_containing(style))
            .then(|| inside || crate::inline_containing::is_inline_containing(style)),
            FragmentKind::Anonymous => Some(inside),
            _ => None,
        };
        if let Some(inside) = descend {
            let origin = (origin.0 + child.rect.x, origin.1 + child.rect.y);
            take_block_level(
                tree,
                child,
                (origin, height, inside),
                (content, rtl, edge),
                (out, kept),
            );
        }
    }
}

/// What monolithic content of the items of a grid line that go on to the next page (`fragments`,
/// whose tokens and the place in the line at the top of the next page are `inside`) does where
/// it reaches past the end of the page, `available` below the top of the line's part: the grid
/// (Blink's, laid out in an imaginary container of all its parts stitched together) moves on in
/// it past that content, while the line and the rows after it (`rest` below the line, with the
/// grid's bottom padding and border) go on from the end of the page. The grid's part on this
/// page reaches as far past its end as that rest does, which moves the content after the grid
/// on the next page down by as much (the second value), and its part on the next page is
/// taller than its content by what is left of the overflow (the first), while its rows go on
/// that much higher, at the top of the page beside the overflow, as in Chrome. The place in the
/// line in `inside` moves on past the overflow.
fn grid_item_overflow(
    fragments: &[(usize, Fragment)],
    inside: &mut (Vec<Option<BreakToken>>, Au),
    line_height: Au,
    available: Au,
    rest: Au,
) -> (Au, Au) {
    let reach = fragments
        .iter()
        .filter(|(item, _)| inside.0.get(*item).is_some_and(Option::is_some))
        .filter_map(|(_, fragment)| crate::monolithic::item_content_bottom(fragment, Au::ZERO))
        .max();
    let overflow = reach.map_or(Au::ZERO, |reach| reach - available);
    if overflow <= Au::ZERO {
        return (Au::ZERO, Au::ZERO);
    }
    let remainder = (line_height - inside.1).clamp_non_negative() + rest;
    let push = remainder.min(overflow);
    inside.1 += overflow;
    (overflow - push, push)
}

/// Whether the part of `line` on a page (`portion`) holds all of its items, each ending there,
/// while the line goes on only because monolithic content of them (an image, a box with size
/// containment) reaches past the end of the page, down to the end of the line. `originals` are
/// the fragments of the container's items in the unbroken layout.
fn line_ends_with_overflow(
    line: &ContainerLine,
    originals: &[Fragment],
    portion: &crate::parallel::Portion,
    flex: bool,
) -> bool {
    let Some((tokens, _)) = &portion.next else {
        return false;
    };
    // (The items' content is walked only where they all end on the page.)
    if !tokens.iter().all(Option::is_none) || portion.fragments.len() != line.boxes.len() {
        return false;
    }
    let reach = originals
        .get(line.items.clone())
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, fragment)| {
            let bottom = crate::monolithic::content_bottom(fragment, Au::ZERO)?;
            // The line of a flex container holds an item that ends with the content with its
            // bottom margin, as in Chrome.
            let margin = line.boxes.get(index).map_or(Au::ZERO, |item| item.margins.1);
            Some(if flex && bottom == fragment.rect.y + fragment.rect.height {
                bottom + margin
            } else {
                bottom
            })
        })
        .max();
    reach == Some(line.bottom)
}

/// Whether every break before the child placed as `before` (after the children placed on the
/// page as `history`) is avoided up to the top of the page, where moving back along breaks that
/// `break-before` and `break-after` avoid finds no child placed below another.
impl Layouter<'_> {
    /// Lays out again the child of a block container that `last` placed (of `boxes`, its
    /// fragment in `children`), breaking it inside at its last break above its end, where it has
    /// one with content above it on the page (see the block layout loop).
    fn break_inside_before(
        &self,
        boxes: &[BoxId],
        last: Placement,
        children: &[Fragment],
    ) -> Option<Outcome> {
        let (space, limit) = last.space?;
        let child = *boxes.get(last.index)?;
        let placed = children.get(last.children)?;
        // (A table's fragment holds its captions and the table box.)
        if !matches!(placed.kind, FragmentKind::Box { .. } | FragmentKind::Anonymous)
            || children.len() != last.children + 1
        {
            return None;
        }
        let style = &self.tree.get(child).style;
        // A break inside a box that avoids them is worse than the avoided one before the child,
        // as in Chrome.
        // (A table breaks between its captions and rows as well, as in Chrome.)
        if style.is_floating()
            || style.is_out_of_flow()
            || self.avoids_breaks_inside(style.break_inside)
            || !matches!(
                self.tree.get(child).kind,
                BoxKind::Block { .. } | BoxKind::Table { .. }
            )
        {
            return None;
        }
        let bottom = placed.rect.y + placed.rect.height;
        let mut remaining = space.remaining? - (limit - bottom) - Au::EPSILON;
        let floats = self.float_count();
        let mut outcome = None;
        for _ in 0..4 {
            outcome = self.block_level(
                child,
                Space {
                    remaining: Some(remaining),
                    ..space
                },
                None,
            );
            // A break in the empty space at the end of a box taller than its content is no
            // breakpoint, as in Chrome (Blink finds breaks between boxes and lines): the break
            // goes before that box instead, where it can.
            let Some(top) = outcome
                .as_ref()
                .filter(|outcome| outcome.token.as_ref().is_some_and(breaks_in_empty_space))
                .and_then(|outcome| broken_box_top(&outcome.fragment))
                .filter(|&top| top > Au::ZERO && top < remaining)
            else {
                break;
            };
            self.truncate_floats(floats);
            remaining = top;
        }
        let outcome = outcome.filter(|outcome| {
            outcome.fragment.children.iter().any(crate::monolithic::in_flow)
                && match &outcome.token {
                    Some(BreakToken::Block { index, .. }) => *index > 0,
                    // Some of the table's captions or rows stay on the page.
                    Some(BreakToken::Captions { index, inner }) => *index > 0 || inner.is_some(),
                    Some(BreakToken::Table { row, cells, .. }) => *row > 0 || !cells.is_empty(),
                    _ => false,
                }
        });
        let Some(mut outcome) = outcome else {
            self.truncate_floats(floats);
            return None;
        };
        outcome.fragment.rect.x = placed.rect.x;
        outcome.fragment.rect.y = placed.rect.y;
        Some(outcome)
    }
}

/// Whether `token` breaks a box in the empty space below its content (see
/// [`BreakToken::splits_empty_space`]), inside the captions of a table too.
fn breaks_in_empty_space(token: &BreakToken) -> bool {
    match token {
        BreakToken::Rest { .. } => true,
        BreakToken::Block {
            inner: Some(inner), ..
        }
        | BreakToken::Captions {
            inner: Some(inner), ..
        } => breaks_in_empty_space(inner),
        _ => false,
    }
}

/// Where the innermost box that `fragment` breaks in (the last of its in-flow children that
/// goes on, and the last of that one's, and so on) starts below the top of `fragment`.
fn broken_box_top(fragment: &Fragment) -> Option<Au> {
    let child = fragment.children.iter().rev().find(|child| {
        crate::monolithic::in_flow(child)
            && matches!(&child.kind, FragmentKind::Box { sides, .. } if !sides.bottom)
    })?;
    Some(child.rect.y + broken_box_top(child).unwrap_or(Au::ZERO))
}

fn only_avoided_breaks(before: &Placement, history: &[Placement]) -> bool {
    if !(before.avoid_before || history.last().is_some_and(|last| last.avoid_after)) {
        return false;
    }
    for (index, entry) in history.iter().enumerate().rev() {
        let avoided = entry.avoid_before
            || index
                .checked_sub(1)
                .is_some_and(|earlier| history[earlier].avoid_after);
        if !avoided {
            return !entry.placed;
        }
    }
    true
}

/// Whether a fragment is or holds a placeholder.
fn has_placeholder(fragment: &Fragment) -> bool {
    matches!(fragment.kind, FragmentKind::Placeholder(_)) || fragment.children.iter().any(has_placeholder)
}

/// The placeholder of an out-of-flow box at its static position.
fn placeholder(id: BoxId, x: Au, y: Au) -> Fragment {
    Fragment {
        rect: Rect::new(x, y, Au::ZERO, Au::ZERO),
        kind: FragmentKind::Placeholder(id),
        children: Children::new(),
    }
}

/// The items of a flex or grid line, laid out again where the line breaks across pages.
struct LineItems<'l, 'a, 'f> {
    layouter: &'l Layouter<'a>,
    items: &'f [LineItem],
    /// Their fragments in the unbroken layout.
    originals: &'f [Fragment],
    /// Whether the line starts at the very top of the page (see [`Space::flush`]).
    flush: bool,
    /// Whether the break before the line is avoided: its items break inside even where they
    /// avoid it (see `LineBoxes::avoids_breaking`), as in Chrome, where the break inside the
    /// line is then no worse.
    kept: bool,
    /// Whether stretched items keep the height of their line where their content breaks,
    /// which then overflows them (see [`Layouter::fixed_item_height`]): those of a row flex
    /// container of a definite height, as in Chrome.
    fixed_stretch: bool,
}

impl LineItems<'_, '_, '_> {
    /// Lays out the part of item `index` from `token` on that fits in `remaining`; a stretched
    /// table grows its rows by `more` beyond the height its line gives it.
    fn part(
        &self,
        index: usize,
        token: Option<&BreakToken>,
        remaining: Au,
        at_top: bool,
        more: Au,
    ) -> Option<(Fragment, Option<BreakToken>)> {
        let item = self.items.get(index)?;
        let original = self.originals.get(index)?;
        let margin_top = if token.is_none() { item.margins.0 } else { Au::ZERO };
        // A stretched item keeps its height, which breaks in the empty space below its content;
        // a stretched table gives it to its rows on every page.
        let table = matches!(self.layouter.tree.get(item.id).kind, BoxKind::Table { .. });
        let height = item
            .stretched
            .then(|| {
                self.layouter
                    .stretched_rest(item.id, original.rect.height, item.containing_width, token)
            })
            .flatten();
        if table && item.stretched {
            // Its rows and captions, whatever its own heights (see `Layouter::table_unsized`).
            let natural = self
                .layouter
                .item(item.id, item.containing_width, item.width, Some(Au::ZERO))
                .map_or(original.rect.height, |outcome| outcome.fragment.rect.height);
            self.layouter
                .table_grow
                .set((original.rect.height - natural).clamp_non_negative() + more);
            self.layouter.table_unsized.set(true);
        }
        // Where the break before the line is avoided, an item that avoids breaks inside it
        // breaks in the empty space below its content all the same (its content ends on the
        // page), as in Chrome, but not inside its content.
        let kept_whole = self.kept
            && token.is_none()
            && !self.layouter.breaks_avoided_items.get()
            && self
                .layouter
                .avoids_breaks_inside(self.layouter.tree.get(item.id).style.break_inside);
        let outer = self
            .layouter
            .breaks_avoided_items
            .replace(kept_whole || self.layouter.breaks_avoided_items.get());
        // An item whose children's heights are percentages of its own does not grow either, as
        // in Chrome (`MinBlockSizeShouldEncompassIntrinsicSize`).
        self.layouter.fixed_item_height.set(
            height.is_some()
                && (self.fixed_stretch && item.stretched || self.layouter.has_percentage_heights(item.id)),
        );
        let outcome = self.layouter.item_part(
            item,
            remaining + margin_top,
            (at_top, self.flush),
            token,
            (height, item.stretched.then_some(original.rect.height)),
        );
        self.layouter.fixed_item_height.set(false);
        self.layouter.breaks_avoided_items.set(outer);
        let outcome = outcome.filter(|outcome| {
            !kept_whole || outcome.token.as_ref().is_none_or(BreakToken::splits_empty_space)
        })?;
        let mut fragment = outcome.fragment;
        fragment.rect.x = original.rect.x;
        Some((fragment, outcome.token))
    }
}

impl LineItems<'_, '_, '_> {
    /// Whether item `index` is stretched taller than its content with its bottom padding and
    /// border.
    fn taller_than_content(&self, index: usize) -> bool {
        let (Some(item), Some(original)) = (self.items.get(index), self.originals.get(index)) else {
            return false;
        };
        if !item.stretched {
            return false;
        }
        let model = self.layouter.box_model(
            &self.layouter.tree.get(item.id).style,
            item.containing_width,
            None,
        );
        let content_end = original
            .children
            .iter()
            .map(|child| child.rect.y + child.rect.height)
            .max()
            .unwrap_or(model.border[TOP] + model.padding[TOP]);
        content_end + model.padding[BOTTOM] + model.border[BOTTOM] < original.rect.height
    }
}

impl crate::parallel::LineBoxes for LineItems<'_, '_, '_> {
    fn lay(
        &mut self,
        index: usize,
        token: Option<&BreakToken>,
        remaining: Au,
        at_top: bool,
    ) -> Option<(Fragment, Option<BreakToken>)> {
        let item = self.items.get(index)?;
        let original = self.originals.get(index)?;
        // An item that fits as it is, with the content that overflows it, stays as it is,
        // stretched and aligned, unless a forced break inside it breaks it.
        if token.is_none()
            && (original.rect.height + item.margins.1).max(crate::parallel::flow_extent(original))
                <= remaining
            && !self.layouter.breaks_inside(item.id)
        {
            return Some((original.clone(), None));
        }
        self.part(index, token, remaining, at_top, Au::ZERO)
    }

    fn expand(
        &mut self,
        index: usize,
        token: &BreakToken,
        remaining: Au,
        at_top: bool,
        by: Au,
    ) -> Option<(Fragment, Option<BreakToken>)> {
        let item = self.items.get(index)?;
        if !matches!(self.layouter.tree.get(item.id).kind, BoxKind::Table { .. }) {
            return None;
        }
        self.part(index, Some(token), remaining, at_top, by)
    }

    /// A break in an item's bottom padding and border is the last resort, unless the item is
    /// stretched taller than its content: it then breaks in the room below its content, as in
    /// Chrome (Blink's `FinishFragmentation` makes only a box no taller than its content the
    /// last resort).
    fn last_resort(&self, index: usize, token: &BreakToken) -> bool {
        self.items.get(index).is_some_and(|item| {
            self.layouter
                .breaks_in_bottom_frame(item.id, Some(token), item.containing_width)
                && !self.taller_than_content(index)
        })
    }

    fn frame_cut(&self, index: usize, token: &BreakToken) -> Au {
        let Some(item) = self.items.get(index) else {
            return Au::ZERO;
        };
        if !self
            .layouter
            .breaks_in_bottom_frame(item.id, Some(token), item.containing_width)
            || !self.taller_than_content(index)
        {
            return Au::ZERO;
        }
        let model = self.layouter.box_model(
            &self.layouter.tree.get(item.id).style,
            item.containing_width,
            None,
        );
        model.padding[BOTTOM] + model.border[BOTTOM]
    }

    fn in_columns(&self) -> bool {
        self.layouter.columns.get() > 0
    }

    fn avoids_breaking(&self, index: usize) -> bool {
        self.items.get(index).is_some_and(|item| {
            !self.kept
                && !self.layouter.breaks_avoided_items.get()
                && self
                    .layouter
                    .avoids_breaks_inside(self.layouter.tree.get(item.id).style.break_inside)
        })
    }

    fn filler(&mut self, index: usize) -> Fragment {
        let (x, width, kind) = self.originals.get(index).map_or_else(
            || (Au::ZERO, Au::ZERO, FragmentKind::Anonymous),
            |original| (original.rect.x, original.rect.width, original.kind.clone()),
        );
        Fragment {
            rect: Rect::new(x, Au::ZERO, width, Au::ZERO),
            kind,
            children: Children::new(),
        }
    }
}

/// A flex item while resolving flexible lengths.
#[derive(Clone)]
struct Item {
    id: BoxId,
    /// The hypothetical main size, which decides the lines.
    base: Au,
    frame: Au,
    main: Au,
    length: crate::flex_sizing::FlexLength,
}

/// The self-alignment of an absolutely positioned box along one axis, where it is `start`,
/// `end` or `center`, or `last baseline`, which falls back to `end` (CSS Box Alignment 3, §4.4,
/// as in Chrome; WPT `css-align/abspos/align-self-static-position-008`). `normal`, `stretch`,
/// `auto` and first baselines leave the box as CSS 2.1 places it.
fn absolute_alignment(alignment: ItemAlignment) -> Option<ItemAlignment> {
    match alignment {
        ItemAlignment::Start | ItemAlignment::End | ItemAlignment::Center => Some(alignment),
        ItemAlignment::LastBaseline => Some(ItemAlignment::End),
        _ => None,
    }
}

/// How far `alignment` (see [`absolute_alignment`]) moves a box from the start of room in which
/// `free` is left over; the start is on the right where `rtl`.
fn aligned_shift(alignment: Option<ItemAlignment>, free: Au, rtl: bool) -> Au {
    match (alignment, rtl) {
        (Some(ItemAlignment::Center), _) => free / 2,
        (Some(ItemAlignment::End), false) | (Some(ItemAlignment::Start), true) => free,
        _ => Au::ZERO,
    }
}

/// The `shift` of an aligned box whose margin box, `outer` long, starts at `start` before it and
/// overflows the room between its insets: moved so that it stays inside its containing block,
/// `room` long, or else to the start of it (CSS Positioned Layout 3, §4.4, as in Chrome).
fn kept_in(start: Au, shift: Au, outer: Au, room: Au) -> Au {
    let last = room - outer;
    let at = if last < Au::ZERO {
        Au::ZERO
    } else {
        (start + shift).clamp(Au::ZERO, last)
    };
    at - start
}

/// Whether a box in normal flow establishes a block formatting context for its contents
/// (CSS 2.1 §9.4.1, CSS Display 3 `flow-root`, CSS Multi-column Layout 1 §6.1), also where its
/// `align-content` is not `normal` (CSS Box Alignment 3, §5.1.1), as in Chrome, and where layout
/// containment makes it an independent formatting context (CSS Contain 2, §3.2; query containers
/// have it too). `overflow: clip` makes none (CSS Overflow 3, §3.1), as in Chrome.
pub(crate) fn establishes_formatting_context(style: &ComputedStyle) -> bool {
    style.contain_layout
        || style.container_type != fagus_style::values::ContainerType::Normal
        || style.overflow_x == fagus_style::values::Overflow::Hidden
        || style.overflow_y == fagus_style::values::Overflow::Hidden
        || style.align_content != ContentAlignment::Normal
        || style.is_floating()
        || style.is_out_of_flow()
        || style.is_multicol()
        || style.column_span == fagus_style::values::ColumnSpan::All
        || matches!(
            style.display,
            fagus_style::values::Display::Normal {
                inside: fagus_style::values::DisplayInside::FlowRoot,
                ..
            }
        )
}

/// How far `-webkit-left`, `-webkit-center` or `-webkit-right` in the `text-align` of `container`
/// (from `align` attributes and `<center>`) move a block-level child laid out as `fragment` across
/// a containing block `width` wide: into the free room beside it unless it has an auto margin,
/// never past the start where it overflows (as Blink's `WebkitTextAlignAndJustifySelfOffset`).
fn legacy_align_offset(
    container: &ComputedStyle,
    child: &ComputedStyle,
    width: Au,
    fragment: &Fragment,
) -> Au {
    use fagus_style::values::TextAlign;
    if !container.align_blocks || child.margin.left.is_auto() || child.margin.right.is_auto() {
        return Au::ZERO;
    }
    let basis = width.to_px();
    let margins =
        Au::from_px(child.margin.left.resolve_or(basis, 0.0) + child.margin.right.resolve_or(basis, 0.0));
    let free = (width - fragment.rect.width - margins).clamp_non_negative();
    // A box in a right-to-left containing block already sits at the right.
    let rtl = container.is_rtl();
    match container.text_align {
        TextAlign::Center if rtl => -(free / 2),
        TextAlign::Center => free / 2,
        TextAlign::Left if rtl => -free,
        TextAlign::Right if !rtl => free,
        _ => Au::ZERO,
    }
}

/// Where an absolutely positioned child of a flex container has its static position: as the
/// container's only item, aligned by `justify-content` and `align-items` in its content box
/// (CSS Flexbox 1, §4.1).
#[derive(Clone, Copy)]
pub(crate) struct FlexStatic {
    /// The width and height of the container's content box.
    size: (Au, Au),
    /// How much of the room left beside and above the box goes before it, across and down.
    fraction: (f32, f32),
    /// Where `safe` center or end alignment keeps the box from starting before its containing
    /// block (CSS Box Alignment 3, §4.4), across and down: whether that starts on the right
    /// (across only; else on the left or top).
    safe: (Option<bool>, Option<bool>),
}

impl FlexStatic {
    /// The alignment of a child with style `item` in a flex container with `style` whose content
    /// box is `size`.
    fn of(style: &ComputedStyle, item: &ComputedStyle, size: (Au, Au)) -> Self {
        let column = matches!(
            style.flex_direction,
            FlexDirection::Column | FlexDirection::ColumnReverse
        );
        let main = match distribute(main_justification(style), Au(64), 1).0 {
            Au(0) => 0.0,
            Au(64) => 1.0,
            _ => 0.5,
        };
        let alignment = item_alignment(style, item);
        let cross = match alignment {
            ItemAlignment::End => 1.0,
            ItemAlignment::Center => 0.5,
            _ => 0.0,
        };
        let rtl = style.is_rtl();
        let fraction = if column {
            (if rtl { 1.0 - cross } else { cross }, main)
        } else {
            let from_right = (style.flex_direction == FlexDirection::RowReverse) != rtl;
            (if from_right { 1.0 - main } else { main }, cross)
        };
        // `safe` alignment across the line keeps a box from starting before its containing
        // block at the cross start.
        let safe = (item_flags(style, item).safe
            && matches!(alignment, ItemAlignment::Center | ItemAlignment::End))
        .then_some(rtl);
        Self {
            size,
            fraction,
            safe: if column {
                (safe, None)
            } else {
                (None, safe.map(|_| false))
            },
        }
    }

    /// The alignment of a child with style `item` of a grid container with `style` whose content
    /// box is `size`, which is not its containing block: by its `justify-self` and `align-self`
    /// in the content box, whatever the tracks (CSS Grid 2, §10.1, as in Chrome).
    pub(crate) fn of_grid(style: &ComputedStyle, item: &ComputedStyle, size: (Au, Au)) -> Self {
        let justified = crate::grid::safe_justify(style, item, Au::ZERO);
        let across = match justified {
            ItemAlignment::End => 1.0,
            ItemAlignment::Center => 0.5,
            _ => 0.0,
        };
        let aligned = item_alignment(style, item);
        let down = match aligned {
            ItemAlignment::End => 1.0,
            ItemAlignment::Center => 0.5,
            _ => 0.0,
        };
        let across = if style.is_rtl() { 1.0 - across } else { across };
        // `safe` alignment keeps a box from starting before its containing block.
        let moves =
            |alignment: ItemAlignment| matches!(alignment, ItemAlignment::Center | ItemAlignment::End);
        let justify_safe = match item.justify_self {
            ItemAlignment::Auto => style.justify_items_block.safe,
            _ => item.justify_self_block.safe,
        } == Some(true);
        Self {
            size,
            fraction: (across, down),
            safe: (
                (justify_safe && moves(justified)).then_some(style.is_rtl()),
                (item_flags(style, item).safe && moves(aligned)).then_some(false),
            ),
        }
    }
}

pub(crate) fn item_alignment(container: &ComputedStyle, item: &ComputedStyle) -> ItemAlignment {
    match item.align_self {
        ItemAlignment::Auto => container.align_items,
        other => other,
    }
}

/// The flags of an item's `align-self`, or of its container's `align-items` for `auto`.
pub(crate) fn item_flags(container: &ComputedStyle, item: &ComputedStyle) -> fagus_style::values::SelfFlags {
    match item.align_self {
        ItemAlignment::Auto => container.align_items_flags,
        _ => item.align_self_flags,
    }
}

/// The `align-self` of an item of a row flex container with `free` room across its line (see
/// [`safe_alignment`]). `wrap-reverse` turns the lines over at the end, flex-start with them;
/// `start` and `end` go by the writing mode and are turned over before (CSS Box Alignment 3,
/// §5.1).
fn cross_alignment(container: &ComputedStyle, item: &ComputedStyle, free: Au) -> ItemAlignment {
    let alignment = safe_alignment(container, item, free);
    if container.flex_wrap != FlexWrap::WrapReverse || item_flags(container, item).flex {
        return alignment;
    }
    match alignment {
        ItemAlignment::Start => ItemAlignment::End,
        ItemAlignment::End => ItemAlignment::Start,
        other => other,
    }
}

/// The `align-self` of an item of a column flex container with `free` room across it (see
/// [`safe_alignment`]), as `start`, `end` or `center` from the container's start side: a
/// baseline, which a column cannot share, is `flex-start` (`last baseline` is `flex-end`), and
/// `wrap-reverse` turns `flex-start` and `flex-end` over, as in Chrome.
fn column_alignment(container: &ComputedStyle, item: &ComputedStyle, free: Au) -> ItemAlignment {
    let (alignment, flex) = match safe_alignment(container, item, free) {
        ItemAlignment::Baseline => (ItemAlignment::Start, true),
        ItemAlignment::LastBaseline => (ItemAlignment::End, true),
        other => (other, item_flags(container, item).flex),
    };
    match alignment {
        _ if container.flex_wrap != FlexWrap::WrapReverse || !flex => alignment,
        ItemAlignment::Start => ItemAlignment::End,
        ItemAlignment::End => ItemAlignment::Start,
        other => other,
    }
}

/// An item's `align-self` (see [`item_alignment`]) where it has `free` room, which is negative
/// where it overflows: `safe` alignment of an item that overflows is `start` (CSS Box Alignment
/// 3, §4.4).
pub(crate) fn safe_alignment(container: &ComputedStyle, item: &ComputedStyle, free: Au) -> ItemAlignment {
    let alignment = item_alignment(container, item);
    if free < Au::ZERO
        && item_flags(container, item).safe
        && matches!(alignment, ItemAlignment::Center | ItemAlignment::End)
    {
        ItemAlignment::Start
    } else {
        alignment
    }
}

/// The token of a block container that continues at `token` with the content of its children
/// `overflow` (which ended on the page but overflow), past its `count` children if `token` is
/// `None`.
fn with_overflow(
    token: Option<BreakToken>,
    overflow: Vec<(usize, BreakToken)>,
    count: usize,
) -> Option<BreakToken> {
    if overflow.is_empty() {
        return token;
    }
    match token {
        Some(BreakToken::Block {
            index,
            inner,
            consumed,
            floats,
            overflow: mut earlier,
        }) => {
            earlier.extend(overflow);
            Some(BreakToken::Block {
                index,
                inner,
                consumed,
                floats,
                overflow: earlier,
            })
        }
        _ => Some(BreakToken::Block {
            index: count,
            inner: None,
            consumed: Consumed::default(),
            floats: token.map(|token| token.floats().to_vec()).unwrap_or_default(),
            overflow,
        }),
    }
}

/// Whether a box's automatic height follows its width through a preferred aspect ratio.
fn has_ratio_height(style: &ComputedStyle) -> bool {
    matches!(style.height, Size::Auto) && style.aspect_ratio.is_some_and(|ratio| ratio > 0.0)
}
