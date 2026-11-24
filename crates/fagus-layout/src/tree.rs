//! The box tree: block containers and inline content, built from the styled DOM.
//!
//! A block container holds either block-level boxes or inline content. Inline content between
//! block-level siblings is wrapped in anonymous block boxes (CSS 2.1 §9.2.1.1).

use std::cell::RefCell;
use std::sync::Arc;

use fagus_dom::{Document, NodeId};
use fagus_style::values::{
    BreakBetween, CaptionSide, ColumnSpan, Content, ContentItem, ContentTarget, Display, DisplayInside,
    DisplayOutside, LengthPercentage, ListStylePosition, ListStyleType, Position, Rgba, RubyDisplay, Sides,
    TextTransform,
};
use fagus_style::{ComputedStyle, ComputedValues, PseudoKey, StyledDocument};
use fagus_text::FontDb;
use rustc_hash::FxHashMap;

use crate::block::establishes_formatting_context;
use crate::counter_style::CounterStyles;
use crate::counters::{Counters, counter_text, marker_text};
use crate::fragment::{FragmentKind, SymbolShape, VisibleSides};
use crate::image::{Image, Images};

mod alt;
mod details;
mod editable;
mod foreign;
pub(crate) mod forms;
mod scroll_markers;
mod tables;

use tables::{OpenParts, Parent, TablePart};

/// How many lines an initial letter raises the text after it by at most (see
/// [`Builder::first_letter`]).
const MAX_RAISED_LINES: usize = 1000;

/// Index of a box in the [`BoxTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BoxId(pub u32);

#[derive(Debug)]
pub struct LayoutBox {
    pub style: Arc<ComputedStyle>,
    /// The element that generated the box; `None` for anonymous boxes and pseudo-elements.
    pub node: Option<NodeId>,
    /// Whether the box is a `::before` or `::after` pseudo-element: a box with a style of its
    /// own, unlike anonymous boxes.
    pub generated: bool,
    pub kind: BoxKind,
}

impl LayoutBox {
    /// The kind of the box's fragments, with `sides` of its border visible. An anonymous box has
    /// only inherited properties and draws nothing itself.
    pub(crate) fn fragment_kind(&self, sides: VisibleSides) -> FragmentKind {
        if self.node.is_none() && !self.generated {
            return FragmentKind::Anonymous;
        }
        FragmentKind::Box {
            style: Arc::clone(&self.style),
            node: self.node,
            sides,
        }
    }
}

#[derive(Debug)]
pub enum BoxKind {
    /// A block container whose children are block-level boxes.
    Block { children: Vec<BoxId> },
    /// A block container with an inline formatting context.
    Inline { items: Vec<InlineItem> },
    /// A table: children are row groups (header, body, footer) and rows.
    Table {
        header: Option<BoxId>,
        bodies: Vec<BoxId>,
        footer: Option<BoxId>,
        /// `<caption>` boxes, laid out above or below the table by `caption-side`.
        captions: Vec<BoxId>,
        /// The style of each column (`<col>`, `<colgroup>`), one entry per column.
        columns: Vec<Arc<ComputedStyle>>,
        /// The columns and column groups with a background, each group before its columns.
        column_backgrounds: Vec<ColumnBackground>,
    },
    /// A group of table rows.
    RowGroup { rows: Vec<BoxId> },
    /// A table row: children are cells.
    Row { cells: Vec<TableCell> },
    /// A flex container.
    Flex { children: Vec<BoxId> },
    /// A grid container.
    Grid { children: Vec<BoxId> },
    /// A replaced element (`<img>`): its content comes from outside the document. `None` if the
    /// image could not be loaded; the box keeps the size its attributes and style give it.
    Replaced {
        image: Option<Arc<Image>>,
        /// The `width` and `height` attributes, which act like CSS sizes of the lowest priority.
        attributes: (Option<f32>, Option<f32>),
    },
}

/// A column or column group with a background, which is drawn in the cells that start in its
/// columns (CSS 2.1, §17.5.1).
#[derive(Clone, Debug)]
pub struct ColumnBackground {
    /// The first of its columns, counted in the source's order, and how many it spans.
    pub start: usize,
    pub span: usize,
    pub style: Arc<ComputedStyle>,
}

/// A cell of a table row: a block container with its spans.
#[derive(Clone, Copy, Debug)]
pub struct TableCell {
    pub id: BoxId,
    pub colspan: usize,
    /// 0 spans to the end of the row group (HTML, `rowspan="0"`).
    pub rowspan: usize,
}

/// A piece of inline content.
#[derive(Clone, Debug)]
pub enum InlineItem {
    /// Text after white space processing and `text-transform`.
    Text { text: String, style: Arc<ComputedStyle> },
    /// A forced line break (`<br>`, or a newline in preformatted text), with the style of the
    /// line it ends when nothing else is on it.
    LineBreak { style: Arc<ComputedStyle> },
    /// An absolutely positioned box: it takes no space, but its static position is here.
    OutOfFlow(BoxId),
    /// A float: placed beside the line it is on, or below it if it does not fit.
    Float(BoxId),
    /// The start of an inline box (an element with `display: inline`).
    Open {
        style: Arc<ComputedStyle>,
        node: Option<NodeId>,
    },
    /// The end of the inline box opened last.
    Close,
    /// An atomic inline: `inline-block`, `inline-flex`, `inline-grid` or `inline-table`, laid out
    /// as a block and placed on a line like a glyph.
    Atomic(BoxId),
    /// The marker of a list item outside its box, drawn before the start of the first line and
    /// taking no space on it, or a symbol inside it. A symbol (`disc`, `circle`, `square`,
    /// `disclosure-*`) is drawn as a shape where browsers draw theirs.
    Marker {
        text: String,
        style: Arc<ComputedStyle>,
        symbol: Option<SymbolShape>,
        inside: bool,
        /// The image of `list-style-image`, in place of the text.
        image: Option<Arc<Image>>,
    },
    /// `string(name)` in generated content: the value the named string has at `position` in
    /// [`BoxTree::string_log`], or, in a row of a table's header or footer, where the row is
    /// placed on a page (after the last row before it).
    NamedString {
        name: Arc<str>,
        position: usize,
        style: Arc<ComputedStyle>,
    },
    /// `leader()` (CSS GCPM 3, §4): its pattern, repeated over the room left on the line.
    Leader { fill: String, style: Arc<ComputedStyle> },
    /// `target-counter(url, page)`: the number of the page the element with the id is on, as
    /// a first layout found it.
    TargetPage {
        id: Arc<str>,
        format: ListStyleType,
        style: Arc<ComputedStyle>,
    },
    /// `counter(page)` or `counter(pages)` in generated content: the number is known only once
    /// the box is laid out on a page.
    PageNumber {
        counter: PageCounter,
        format: ListStyleType,
        style: Arc<ComputedStyle>,
    },
}

/// A counter of pages in generated content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageCounter {
    /// `counter(page)`: the number of the page.
    Page,
    /// `counter(pages)`: the page count.
    Pages,
    /// `counter(-fagus-next-page)`, a Fagus extension: the number of the next page, where a
    /// table that breaks continues ("continued on page 3").
    Next,
    /// `counter(-fagus-previous-page)`, a Fagus extension: the number of the page before ("carried
    /// over from page 2").
    Previous,
    /// `counter(footnote)` in the call or the marker of the footnote numbered `ordinal` (from 0,
    /// in document order): `value` where no `@page` rule resets the counter, else counted from
    /// the first footnote called on the page (see `Layouter::footnote_numbering`).
    Footnote { value: i32, ordinal: u32 },
}

impl PageCounter {
    /// The page counter named `name`, if it is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "page" => Self::Page,
            "pages" => Self::Pages,
            "-fagus-next-page" => Self::Next,
            "-fagus-previous-page" => Self::Previous,
            _ => return None,
        })
    }
}

/// The named strings each element sets: names with their values.
pub type NamedStrings = FxHashMap<NodeId, Vec<(Arc<str>, StringValue)>>;

/// The value of a named string, set by `string-set`: text, with the page counters in it
/// (`counter(page)`, `counter(pages)`) left until the page of the element that sets it is known.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StringValue(Vec<StringPart>);

/// A part of a [`StringValue`].
#[derive(Clone, Debug, PartialEq)]
enum StringPart {
    Text(String),
    Page {
        counter: PageCounter,
        format: ListStyleType,
    },
}

impl StringValue {
    fn push_text(&mut self, text: &str) {
        match self.0.last_mut() {
            Some(StringPart::Text(last)) => last.push_str(text),
            _ if text.is_empty() => {}
            _ => self.0.push(StringPart::Text(text.to_owned())),
        }
    }

    /// The text of the value, with each page counter given its value by `page`.
    pub(crate) fn resolve(&self, page: impl Fn(PageCounter) -> i32, styles: &CounterStyles) -> String {
        self.0
            .iter()
            .map(|part| match part {
                StringPart::Text(text) => text.clone(),
                StringPart::Page { counter, format } => counter_text(page(*counter), format, styles),
            })
            .collect()
    }
}

/// The value of the page counter `counter` (other than that of footnotes, which is `here`) on
/// page `page` (from 1) of `total`, where the values of the page counter on the pages are
/// `values` (see `page::PageNumbering`) and its value on this page is `here`, or that of `values`.
pub(crate) fn page_counter_value(
    counter: PageCounter,
    (page, total): (usize, usize),
    values: &[i32],
    here: Option<i32>,
) -> i32 {
    let value_of = |page: usize| {
        page.checked_sub(1)
            .and_then(|index| values.get(index))
            .copied()
            .unwrap_or_else(|| i32::try_from(page).unwrap_or(i32::MAX))
    };
    let here = here.unwrap_or_else(|| value_of(page));
    match counter {
        PageCounter::Page | PageCounter::Footnote { .. } => here,
        PageCounter::Pages => i32::try_from(total).unwrap_or(i32::MAX),
        PageCounter::Next => values
            .get(page)
            .copied()
            .unwrap_or_else(|| here.saturating_add(1)),
        PageCounter::Previous if page > 1 => value_of(page - 1),
        PageCounter::Previous => here.saturating_sub(1),
    }
}

/// The counters in scope at an element: their names and values from the outermost in.
pub type CounterValues = Vec<(String, Vec<i32>)>;

/// What the `target-*()` functions of generated content refer to (CSS GCPM 3, §2): the pages
/// that elements with an id are on and their counters, as a first layout found them.
#[derive(Clone, Debug, Default)]
pub struct Targets {
    /// The value of the page counter (`counter(page)`) on the page each element with an id
    /// starts on: its number, where no page resets, sets or increments the counter.
    pub pages: FxHashMap<String, i32>,
    /// The counters at each element with an id: their names and values from the outermost in.
    pub counters: FxHashMap<String, CounterValues>,
    /// Whether generated content asks for them, and so the document is to be laid out again
    /// with them.
    pub asked: bool,
}

/// The boxes of a document.
#[derive(Debug, Default)]
pub struct BoxTree {
    boxes: Vec<LayoutBox>,
    /// The boxes, charged to the budget of the render while they live.
    held: fagus_dom::budget::Held,
    pub root: Option<BoxId>,
    /// Boxes with `position: fixed`, which are repeated on every page.
    pub fixed: Vec<BoxId>,
    /// Boxes with `position: fixed` inside a box that contains them, laid out as absolutely
    /// positioned boxes in it (see [`crate::contained_fixed`]).
    pub(crate) contained_fixed: rustc_hash::FxHashSet<BoxId>,
    /// The values elements assign to named strings with `string-set`, in the order given.
    pub strings: NamedStrings,
    /// The elements whose named strings are assigned where a cell starts: a table row's and its
    /// row group's, at the row's first cell, since rows have no fragments of their own.
    pub(crate) leading_strings: FxHashMap<NodeId, Vec<NodeId>>,
    /// Every assignment to a named string, in document order: where `string()` in the content of
    /// an element finds its value.
    pub(crate) string_log: Vec<(Arc<str>, StringValue)>,
    /// Where the assignments of each element in [`Self::strings`] start in the string log.
    pub(crate) string_starts: FxHashMap<NodeId, usize>,
    /// The named strings of elements without a box of their own after the last element with
    /// one, assigned where the root element ends.
    pub(crate) trailing_strings: Vec<(Arc<str>, StringValue)>,
    /// For each table row, the length of [`Self::string_log`] before and after the row's own
    /// assignments; for each table, its length after the table.
    pub(crate) string_positions: FxHashMap<BoxId, (usize, usize)>,
    /// Where each named string is assigned in the string log, in order.
    string_index: FxHashMap<Arc<str>, Vec<usize>>,
    /// The used page name of each box (`page`), empty if the document names no pages.
    page_names: Vec<Option<Arc<str>>>,
    /// The boxes that span the columns of a multi-column container (`column-span: all`), with
    /// the anonymous box of the content they interrupt.
    spanners: FxHashMap<BoxId, BoxId>,
    /// The boxes between a multi-column container's content box and the spanners in it.
    spanner_holders: rustc_hash::FxHashSet<BoxId>,
    /// The `break-before` and `break-after` of boxes that differ from their own, with the
    /// values of their first and last children passed on.
    breaks: FxHashMap<BoxId, (BreakBetween, BreakBetween)>,
    /// The `::first-line` style of the block container whose first line a box of inline
    /// content holds.
    pub(crate) first_lines: FxHashMap<BoxId, Arc<ComputedStyle>>,
    /// Whether the document is in quirks or limited-quirks mode, where the line height
    /// calculation quirk applies (Quirks Mode, §3.3).
    pub(crate) line_height_quirk: bool,
    /// In quirks mode, the root and `body` elements, which fill the page area and resolve
    /// percentage heights inside against that (Quirks Mode, §3.6, §3.7), and whose auto-height
    /// descendants pass percentage heights on to their containing blocks (§3.5).
    pub(crate) quirky: Option<(NodeId, Option<NodeId>)>,
    /// What the `target-*()` functions refer to, as known when the tree was built.
    pub(crate) targets: Targets,
    /// The counters at the elements with an id, and whether `target-*()` functions were met.
    pub(crate) found: Targets,
    /// The counter styles counters are written in.
    pub(crate) counter_styles: CounterStyles,
    /// The background of the root element, or of the body, which covers the canvas.
    pub canvas: Option<Arc<ComputedStyle>>,
    /// The atomic inlines whose baseline is the bottom of their border box, not of their margin
    /// box, raised by a number of pixels: checkboxes and radio buttons, as in Chrome, and color
    /// wells, whose baseline is the bottom of their swatch.
    pub(crate) border_box_baselines: FxHashMap<BoxId, f32>,
    /// The fieldsets with a rendered legend, with the legend's element: the legend sits on the
    /// top border, which has a gap for it.
    pub(crate) fieldsets: FxHashMap<BoxId, NodeId>,
    /// The block-level boxes that shrink to fit their content like inline blocks: buttons, and
    /// text fields, date fields and selects (to the width of [`Self::control_widths`]).
    pub(crate) shrinks_to_fit: rustc_hash::FxHashSet<BoxId>,
    /// The out-of-flow boxes that come right after text that white space processing removed
    /// whole (Blink's empty text items): a line may break after them where it may break there
    /// in the text (see `crate::inline`).
    pub(crate) after_collapsed: rustc_hash::FxHashSet<BoxId>,
    /// The form controls whose content is as wide as with `width: auto`, whatever their width
    /// says, with that width in pixels: checkboxes, radio buttons, text fields, text areas and
    /// selects. It is their automatic minimum size in a flex row, as in Chrome, and the
    /// min-content and max-content width of the fields, whose `width` stays auto.
    pub(crate) control_widths: FxHashMap<BoxId, f32>,
    /// The marks of checkboxes, sliders and progress bars, which are drawn at the size their boxes
    /// get in layout (stretched in a flex column, say), in place of their images.
    pub(crate) marks: FxHashMap<BoxId, forms::Mark>,
    /// The alpha of the images of floats with `shape-outside: url(…)` that loaded.
    shape_images: FxHashMap<BoxId, Arc<crate::shape::AlphaMask>>,
    /// The annotations of ruby columns, by the atomic inline box of their base.
    pub(crate) ruby_columns: FxHashMap<BoxId, crate::ruby::RubyColumn>,
    /// The boxes whose content flows in another writing mode, and the styles of boxes in
    /// vertical writing modes.
    pub(crate) flows: crate::writing::Flows,
    /// The boxes of the `<foreignObject>` elements of inline `<svg>` boxes, by the `<svg>` box.
    pub(crate) foreign: FxHashMap<BoxId, crate::foreign::ForeignBoxes>,
    /// The formulas of `<math>` boxes, which are laid out like replaced elements.
    pub(crate) formulas: FxHashMap<BoxId, Arc<crate::math::MathBox>>,
    /// The replaced boxes of the images in the `content` of page margin boxes, by URL.
    pub(crate) margin_images: FxHashMap<Arc<str>, BoxId>,
    /// The boxes of footnotes (`float: footnote`), which go into the footnote area of the page
    /// their call is on, with their numbers in document order (from 0).
    pub(crate) footnotes: FxHashMap<BoxId, u32>,
}

impl BoxTree {
    /// The alpha of the `shape-outside` image of a float.
    pub(crate) fn shape_image(&self, id: BoxId) -> Option<&crate::shape::AlphaMask> {
        self.shape_images.get(&id).map(|alpha| &**alpha)
    }

    /// Finds the alpha of the `shape-outside` images of floats.
    fn find_shape_images(&mut self, images: &Images) {
        for (index, layout_box) in self.boxes.iter().enumerate() {
            if let Some(fagus_style::values::ShapeOutside::Image(fagus_style::values::Image::Url(url, _))) =
                layout_box.style.shape_outside.as_deref()
                && layout_box.style.is_floating()
                && let Some(alpha) = images.alpha(url)
                && let Ok(index) = u32::try_from(index)
            {
                self.shape_images.insert(BoxId(index), Arc::clone(alpha));
            }
        }
    }

    /// The `::first-line` style of the first line of a box of inline content.
    pub(crate) fn first_line(&self, id: BoxId) -> Option<&Arc<ComputedStyle>> {
        self.first_lines.get(&id)
    }

    /// Whether a box is a replaced element as CSS alignment takes it: an image, but not a form
    /// control Fagus draws as an image (a checkbox, a slider), which stretches in flex and grid
    /// layout as Chrome's controls do.
    pub(crate) fn is_replaced(&self, id: BoxId) -> bool {
        matches!(self.get(id).kind, BoxKind::Replaced { .. })
            && !self.marks.contains_key(&id)
            && !self.control_widths.contains_key(&id)
    }

    /// The styles of all boxes.
    pub(crate) fn styles(&self) -> impl Iterator<Item = &Arc<ComputedStyle>> {
        self.boxes.iter().map(|layout_box| &layout_box.style)
    }

    /// The ids of all boxes.
    pub(crate) fn ids(&self) -> impl Iterator<Item = BoxId> + use<> {
        (0..u32::try_from(self.boxes.len()).expect("fewer than u32::MAX boxes")).map(BoxId)
    }

    pub fn get(&self, id: BoxId) -> &LayoutBox {
        &self.boxes[id.0 as usize]
    }

    /// Whether any box of the tree is as `test` asks.
    pub(crate) fn any_box(&self, test: impl FnMut(&LayoutBox) -> bool) -> bool {
        self.boxes.iter().any(test)
    }

    pub(crate) fn box_mut(&mut self, id: BoxId) -> &mut LayoutBox {
        &mut self.boxes[id.0 as usize]
    }

    pub(crate) fn push(&mut self, layout_box: LayoutBox) -> BoxId {
        self.boxes.push(layout_box);
        BoxId(u32::try_from(self.boxes.len() - 1).expect("fewer than u32::MAX boxes"))
    }

    /// The boxes inside a box, in document order.
    pub(crate) fn children(&self, id: BoxId) -> Vec<BoxId> {
        match &self.get(id).kind {
            BoxKind::Block { children } | BoxKind::Flex { children } | BoxKind::Grid { children } => {
                children.clone()
            }
            BoxKind::Inline { items } => items
                .iter()
                .filter_map(|item| match item {
                    InlineItem::Atomic(id) | InlineItem::OutOfFlow(id) | InlineItem::Float(id) => Some(*id),
                    _ => None,
                })
                .collect(),
            BoxKind::Table {
                header,
                bodies,
                footer,
                captions,
                ..
            } => header
                .iter()
                .chain(bodies)
                .chain(footer)
                .chain(captions)
                .copied()
                .collect(),
            BoxKind::RowGroup { rows } => rows.clone(),
            BoxKind::Row { cells } => cells.iter().map(|cell| cell.id).collect(),
            BoxKind::Replaced { .. } => Vec::new(),
        }
    }

    /// The value of the named string `name` at `position` in the string log: the last one
    /// assigned before it, with where the log has it, or nothing.
    pub(crate) fn string_at(&self, name: &str, position: usize) -> Option<(usize, &StringValue)> {
        let positions = self.string_index.get(name)?;
        match positions.partition_point(|&assigned| assigned < position) {
            0 => None,
            count => {
                let at = positions[count - 1];
                Some((at, &self.string_log[at].1))
            }
        }
    }

    /// The used page name of a box: its `page` value, or its parent's if that is `auto`.
    pub(crate) fn page_name(&self, id: BoxId) -> Option<&Arc<str>> {
        self.page_names.get(id.0 as usize)?.as_ref()
    }

    /// Whether the document names pages.
    pub(crate) fn names_pages(&self) -> bool {
        !self.page_names.is_empty()
    }

    /// Whether any box spans columns.
    pub(crate) fn has_spanners(&self) -> bool {
        !self.spanners.is_empty()
    }

    /// The content box of the multi-column container whose columns a box spans.
    pub(crate) fn spanned_flow(&self, id: BoxId) -> Option<BoxId> {
        self.spanners.get(&id).copied()
    }

    /// Whether a spanner lies inside box `id`, which is in the content of columns.
    pub(crate) fn holds_spanner(&self, id: BoxId) -> bool {
        self.spanner_holders.contains(&id)
    }

    /// The boxes that span the columns whose content box is `flow`.
    pub(crate) fn spanners_of(&self, flow: BoxId) -> impl Iterator<Item = BoxId> + '_ {
        self.spanners
            .iter()
            .filter(move |&(_, &spanned)| spanned == flow)
            .map(|(&spanner, _)| spanner)
    }

    /// Finds the boxes with `column-span: all` that span columns: in-flow block-level boxes in
    /// the block formatting context of a multi-column container's content, inside block
    /// containers that do not establish one of their own (CSS Multi-column Layout 1, §6.1), nor
    /// contain fixed boxes (a transform or filter, say), as in Blink
    /// (`CanContainSpannerInParentFragmentationContext`).
    fn find_spanners(&mut self) {
        let mut spanners = FxHashMap::default();
        let mut holders = rustc_hash::FxHashSet::default();
        // The box around each box on the way to a spanner.
        let mut parent_of = FxHashMap::default();
        for layout_box in &self.boxes {
            let BoxKind::Block { children } = &layout_box.kind else {
                continue;
            };
            let &[flow] = &children[..] else {
                continue;
            };
            if !layout_box.style.is_multicol() {
                continue;
            }
            let mut parents = vec![flow];
            while let Some(parent) = parents.pop() {
                let BoxKind::Block { children } = &self.get(parent).kind else {
                    continue;
                };
                for &child in children {
                    let style = &self.get(child).style;
                    if style.is_out_of_flow() || style.is_floating() {
                        continue;
                    }
                    if style.column_span == ColumnSpan::All {
                        spanners.insert(child, flow);
                        let mut holder = parent;
                        while holder != flow && holders.insert(holder) {
                            let Some(&up) = parent_of.get(&holder) else {
                                break;
                            };
                            holder = up;
                        }
                    } else if !establishes_formatting_context(style)
                        && !crate::contained_fixed::contains_fixed(style)
                    {
                        parents.push(child);
                        parent_of.insert(child, parent);
                    }
                }
            }
        }
        self.spanners = spanners;
        self.spanner_holders = holders;
    }

    /// The `break-before` of a box, with that of its first child passed on.
    pub(crate) fn break_before(&self, id: BoxId) -> BreakBetween {
        self.breaks
            .get(&id)
            .map_or(self.get(id).style.break_before, |breaks| breaks.0)
    }

    /// The `break-after` of a box, with that of its last child passed on.
    pub(crate) fn break_after(&self, id: BoxId) -> BreakBetween {
        self.breaks
            .get(&id)
            .map_or(self.get(id).style.break_after, |breaks| breaks.1)
    }

    /// Passes the `break-before` of first children and the `break-after` of last children on to
    /// their block containers, since breaks fall only between siblings (CSS Fragmentation 3,
    /// §3.1); those of the items of the first and last lines of a flex or grid container to
    /// it, as in Chrome (all items of a single-line row; layout finds the lines of others, see
    /// `Outcome::passed_breaks`). In tables, as in Chrome, those of all cells of a row pass on
    /// to the row, of the first and last rows of a group to the group, and of the first and
    /// last groups to the table, but not past a caption above or below it.
    fn propagate_breaks(&mut self) {
        // Boxes mostly come after their children, but table parts may come after their table:
        // passes in order repeat until nothing changes.
        for _ in 0..8 {
            let mut changed = false;
            for index in 0..self.boxes.len() {
                let id = BoxId(u32::try_from(index).expect("fewer than u32::MAX boxes"));
                let Some((first, last)) = self.passing_children(id) else {
                    continue;
                };
                let style = &self.boxes[index].style;
                let own = (style.break_before, style.break_after);
                let breaks = (
                    first.iter().fold(own.0, |value, &first| {
                        combine_breaks(value, self.break_before(first))
                    }),
                    last.iter().fold(own.1, |value, &last| {
                        combine_breaks(value, self.break_after(last))
                    }),
                );
                if breaks != own && self.breaks.get(&id) != Some(&breaks) {
                    self.breaks.insert(id, breaks);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// The children of a box that pass their `break-before` (first) and `break-after` (last) on
    /// to it; `None` for boxes that take none from their children.
    fn passing_children(&self, id: BoxId) -> Option<(Vec<BoxId>, Vec<BoxId>)> {
        let in_flow = |child: &&BoxId| {
            let style = &self.get(**child).style;
            !style.is_out_of_flow() && !style.is_floating()
        };
        let ends = |children: &[BoxId]| {
            (
                children.iter().find(in_flow).copied().into_iter().collect(),
                children.iter().rev().find(in_flow).copied().into_iter().collect(),
            )
        };
        let layout_box = self.get(id);
        // Layout containment keeps them inside, where those at the start or the end of its
        // content fall at no break point, as in Chrome (WPT `contain-layout-breaks-002`).
        if layout_box.style.layout_containment {
            return None;
        }
        // Nor do the forced breaks inside a box laid out whole (size containment) break the
        // flow around it, as in Chrome (WPT css-break monolithic-content-with-forced-break-001).
        let (inline, block) = layout_box.style.size_containment();
        if inline && block {
            return None;
        }
        Some(match &layout_box.kind {
            BoxKind::Block { children } | BoxKind::Grid { children } => ends(children),
            BoxKind::Flex { children } => {
                let style = &layout_box.style;
                let row = matches!(
                    style.flex_direction,
                    fagus_style::values::FlexDirection::Row | fagus_style::values::FlexDirection::RowReverse
                );
                if row && style.flex_wrap == fagus_style::values::FlexWrap::NoWrap {
                    let items: Vec<BoxId> = children.iter().filter(in_flow).copied().collect();
                    (items.clone(), items)
                } else {
                    ends(children)
                }
            }
            BoxKind::Row { cells } => {
                let cells: Vec<BoxId> = cells.iter().map(|cell| cell.id).collect();
                (cells.clone(), cells)
            }
            BoxKind::RowGroup { rows } => ends(rows),
            BoxKind::Table {
                header,
                bodies,
                footer,
                captions,
                ..
            } => {
                let groups: Vec<BoxId> = header.iter().chain(bodies).chain(footer).copied().collect();
                let (first, last) = ends(&groups);
                let side = |side: CaptionSide| {
                    captions
                        .iter()
                        .any(|&caption| self.get(caption).style.caption_side == side)
                };
                (
                    if side(CaptionSide::Top) { Vec::new() } else { first },
                    if side(CaptionSide::Bottom) {
                        Vec::new()
                    } else {
                        last
                    },
                )
            }
            _ => return None,
        })
    }

    /// The break between two rows of a table that follow each other, each with its row group:
    /// the `break-after` of the upper row (of its group, if the groups differ) joined with the
    /// `break-before` of the lower one (of its group).
    pub(crate) fn break_between_rows(&self, above: (BoxId, BoxId), below: (BoxId, BoxId)) -> BreakBetween {
        if above.1 == below.1 {
            combine_breaks(self.break_after(above.0), self.break_before(below.0))
        } else {
            combine_breaks(self.break_after(above.1), self.break_before(below.1))
        }
    }

    fn name_pages(&mut self) {
        let mut names = vec![None; self.boxes.len()];
        let mut stack: Vec<(BoxId, Option<Arc<str>>)> =
            self.root.map(|root| (root, None)).into_iter().collect();
        while let Some((id, inherited)) = stack.pop() {
            let name = self.get(id).style.page.clone().or(inherited);
            for child in self.children(id) {
                stack.push((child, name.clone()));
            }
            names[id.0 as usize] = name;
        }
        self.page_names = names;
    }

    /// Builds the box tree for the document element.
    pub fn build(document: &Document, styled: &StyledDocument, images: &Images) -> Self {
        Self::build_with(
            document,
            styled,
            images,
            Targets::default(),
            CounterStyles::default(),
            None,
            None,
        )
    }

    /// Builds the tree with what the `target-*()` functions refer to, writing counters in
    /// `counter_styles`. Form controls outside any `lang` write their texts in the language of
    /// `locale` (a language tag), else in American English.
    pub fn build_with(
        document: &Document,
        styled: &StyledDocument,
        images: &Images,
        targets: Targets,
        counter_styles: CounterStyles,
        fonts: Option<&FontDb>,
        locale: Option<&str>,
    ) -> Self {
        let mut tree = Self {
            line_height_quirk: document.quirks_mode() || document.limited_quirks_mode(),
            quirky: document
                .document_element()
                .filter(|_| document.quirks_mode())
                .map(|root| {
                    let body = document.child_elements(root).find(|&child| {
                        document
                            .element(child)
                            .is_some_and(|data| data.is_html() && data.is("body"))
                    });
                    (root, body)
                }),
            ..Self::default()
        };
        // Counters are kept at the elements with an id where generated content may ask for them.
        let snapshots = styled.all().any(|style| {
            let targeted = |items: &[ContentItem]| {
                items.iter().any(|item| {
                    matches!(
                        item,
                        ContentItem::TargetCounter { .. } | ContentItem::TargetCounters { .. }
                    )
                })
            };
            matches!(&style.content, Content::Items(items) if targeted(items))
        });
        if let Some(root) = document.document_element()
            && let Some(style) = styled.style(root)
            && style.display != Display::None
        {
            let builder = Builder {
                document,
                styled,
                images,
                fonts,
                locale,
                counters: RefCell::default(),
                pending_marker: RefCell::default(),
                pending_first_letter: RefCell::default(),
                pending_first_line: RefCell::default(),
                targets: &targets,
                counter_styles: &counter_styles,
                snapshots: snapshots.then(RefCell::default),
                asked: std::cell::Cell::new(false),
                quote_depth: std::cell::Cell::new(0),
                ids: std::cell::OnceCell::new(),
                strings: RefCell::default(),
                string_log: RefCell::default(),
                string_starts: RefCell::default(),
                deferred_strings: RefCell::default(),
                parent: std::cell::Cell::default(),
                anonymous_tables: RefCell::default(),
                contents_parents: RefCell::default(),
                has_footnotes: styled.all().any(|style| style.footnote),
                footnote: std::cell::Cell::new(None),
                next_footnote: std::cell::Cell::new(0),
                footnote_element: std::cell::Cell::new(None),
            };
            builder.enter(root, style);
            let root_box = builder.block_level(&mut tree, root, Arc::clone(style));
            tree.root = Some(root_box);
            // An absolutely positioned or fixed root shrinks to fit, as such boxes do, unless
            // its `left` and `right` both say where its sides are (CSS 2.1 §10.3.7).
            if matches!(style.position, Position::Absolute | Position::Fixed)
                && (style.inset.left.is_auto() || style.inset.right.is_auto())
            {
                tree.shrinks_to_fit.insert(root_box);
            }
            // One with an automatic height whose `top` and `bottom` both say where its edges are
            // fills the height they leave (§10.6.4): its margin box stretches over the initial
            // containing block less them, which the pages give it (see
            // `crate::positioned::root_block_room`).
            if crate::positioned::root_fills_height(style) {
                tree.box_mut(root_box).style = Arc::new(ComputedStyle::from(ComputedValues {
                    height: fagus_style::values::Size::Stretch,
                    ..ComputedValues::clone(style)
                }));
            }
            tree.trailing_strings = builder.deferred_strings.take();
            builder
                .string_log
                .borrow_mut()
                .extend(tree.trailing_strings.iter().cloned());
            tree.strings = builder.strings.take();
            tree.string_log = builder.string_log.take();
            tree.string_starts = builder.string_starts.take();
            tree.found = Targets {
                pages: FxHashMap::default(),
                counters: builder.snapshots.map(RefCell::into_inner).unwrap_or_default(),
                asked: builder.asked.get(),
            };
            for (position, (name, _)) in tree.string_log.iter().enumerate() {
                tree.string_index
                    .entry(Arc::clone(name))
                    .or_default()
                    .push(position);
            }
            if styled.all().any(|style| style.page.is_some()) {
                tree.name_pages();
            }
            if styled.all().any(|style| style.column_span == ColumnSpan::All) {
                tree.find_spanners();
            }
            if styled.all().any(|style| {
                style.break_before != BreakBetween::Auto || style.break_after != BreakBetween::Auto
            }) {
                tree.propagate_breaks();
            }
            if !tree.fixed.is_empty()
                && styled
                    .all()
                    .any(|style| crate::contained_fixed::contains_fixed(style))
            {
                tree.contain_fixed(document, styled);
            }
            tree.propagate_background(document);
            if styled.all().any(|style| style.shape_outside.is_some()) {
                tree.find_shape_images(images);
            }
            tree.propagate_writing_mode(document);
            if styled.all().any(|style| style.writing_mode.is_vertical()) {
                crate::writing::prepare(&mut tree);
            }
            if tree.quirky.is_some() {
                tree.drop_quirky_margins();
            }
        }
        tree.targets = targets;
        tree.counter_styles = counter_styles;
        tree.held = fagus_dom::budget::hold(
            fagus_dom::budget::Kind::Layout,
            (tree.boxes.len() * size_of::<LayoutBox>()) as u64,
        );
        tree
    }

    /// Quirks mode: the default margins of the user agent stylesheet (see
    /// `ComputedStyle::quirky_margins`) count for nothing at the start and the end of `body` and
    /// of table cells, as in Blink: those of the first in-flow child and of the first children
    /// inside it that its top margin adjoins, and likewise at the end.
    fn drop_quirky_margins(&mut self) {
        let body = self.quirky.and_then(|(_, body)| body);
        for index in 0..self.boxes.len() {
            let layout_box = &self.boxes[index];
            let cell = layout_box.node.is_some()
                && layout_box.style.display
                    == Display::Internal(fagus_style::values::InternalDisplay::TableCell);
            if !cell && (layout_box.node.is_none() || layout_box.node != body) {
                continue;
            }
            let container = BoxId(u32::try_from(index).expect("fewer than u32::MAX boxes"));
            self.drop_edge_margins(container, true);
            self.drop_edge_margins(container, false);
        }
    }

    /// Drops the quirky top (`start`) or bottom margins of the first or last in-flow children of
    /// `container`, down as far as their margins adjoin; past empty boxes, whose margins collapse
    /// through them, on to the next.
    fn drop_edge_margins(&mut self, container: BoxId, start: bool) {
        let mut parent = container;
        loop {
            let BoxKind::Block { children } = &self.get(parent).kind else {
                return;
            };
            let mut in_flow: Vec<BoxId> = children
                .iter()
                .copied()
                .filter(|&child| {
                    let style = &self.get(child).style;
                    !style.is_floating() && !style.is_out_of_flow()
                })
                .collect();
            if !start {
                in_flow.reverse();
            }
            // Empty boxes at the edge, which are quirky at both ends (as in Blink).
            let mut index = 0;
            while index + 1 < in_flow.len() && self.is_empty_quirky(in_flow[index]) {
                let style = Arc::make_mut(&mut self.box_mut(in_flow[index]).style);
                style.margin.top = fagus_style::values::LengthPercentageAuto::ZERO;
                style.margin.bottom = fagus_style::values::LengthPercentageAuto::ZERO;
                index += 1;
            }
            let Some(&child) = in_flow.get(index) else {
                return;
            };
            let style = &self.get(child).style;
            let quirky = if start {
                style.quirky_margins.0
            } else {
                style.quirky_margins.1
            };
            let (border, padding) = if start {
                (style.border_width.top, &style.padding.top)
            } else {
                (style.border_width.bottom, &style.padding.bottom)
            };
            let adjoins =
                border == 0.0 && padding.fixed() == Some(0.0) && !establishes_formatting_context(style);
            if quirky {
                let style = Arc::make_mut(&mut self.box_mut(child).style);
                if start {
                    style.margin.top = fagus_style::values::LengthPercentageAuto::ZERO;
                } else {
                    style.margin.bottom = fagus_style::values::LengthPercentageAuto::ZERO;
                }
            }
            if !adjoins {
                return;
            }
            parent = child;
        }
    }

    /// Whether a box is empty with a quirky top margin, so that its margins collapse through it
    /// at the start of a quirky container.
    fn is_empty_quirky(&self, id: BoxId) -> bool {
        let layout_box = self.get(id);
        let style = &layout_box.style;
        style.quirky_margins.0
            && match &layout_box.kind {
                BoxKind::Block { children } => children.is_empty(),
                BoxKind::Inline { items } => items.is_empty(),
                _ => false,
            }
            && matches!(style.height, fagus_style::values::Size::Auto)
            && style.border_width.top == 0.0
            && style.border_width.bottom == 0.0
            && style.padding.top.fixed() == Some(0.0)
            && style.padding.bottom.fixed() == Some(0.0)
    }

    /// Moves the background of the root element to the canvas, or, where it has none and is
    /// the `html` element, that of its `body` (CSS Backgrounds 3, §2.11.2) unless either has
    /// containment (CSS Contain 2, §3.2 and §3.3 name layout and paint; Chrome 157 keeps it for
    /// any, `style`, size and query containers too, measured). The canvas has no
    /// border or padding: its layers are placed in a box as large as the root element's
    /// border box, at the corner of the canvas, as Chrome places them.
    fn propagate_background(&mut self, document: &Document) {
        let Some(root) = self.root else { return };
        let is = |tree: &Self, id: BoxId, name: &str| {
            tree.get(id)
                .node
                .and_then(|node| document.element(node))
                .is_some_and(|element| element.is(name))
        };
        let mut source = Some(root).filter(|&root| has_background(&self.get(root).style));
        if source.is_none() && is(self, root, "html") && !self.get(root).style.has_containment() {
            // The `body` element's box, wherever the root's display put it (a table root wraps
            // its children in anonymous rows and cells, and an absolutely positioned `body` is
            // none of them).
            let body = self.get(root).node.and_then(|html| {
                document
                    .children(html)
                    .find(|&node| document.element(node).is_some_and(|element| element.is("body")))
            });
            source = body
                .and_then(|body| {
                    self.boxes
                        .iter()
                        .position(|layout_box| layout_box.node == Some(body) && !layout_box.generated)
                })
                .and_then(|index| u32::try_from(index).ok())
                .map(BoxId)
                .filter(|&body| !self.get(body).style.has_containment());
        }
        let Some(source) = source.filter(|&source| has_background(&self.get(source).style)) else {
            // A canvas without layers, whose color scheme still counts: a root element in the
            // dark one has the page painted dark.
            let style = &self.get(root).style;
            if style.dark {
                let mut canvas = ComputedStyle::clone(style);
                canvas.border_width = Sides::all(0.0);
                canvas.padding = Sides::all(LengthPercentage::ZERO);
                self.canvas = Some(Arc::new(canvas));
            }
            return;
        };
        let style = &self.get(source).style;
        let mut canvas = ComputedStyle::clone(style);
        canvas.border_width = Sides::all(0.0);
        canvas.padding = Sides::all(LengthPercentage::ZERO);
        // An inline `body` split around blocks has a box for each part.
        let node = self.get(source).node;
        for (index, layout_box) in self.boxes.iter_mut().enumerate() {
            if index == source.0 as usize
                || (source != root && !layout_box.generated && node.is_some() && layout_box.node == node)
            {
                let mut own = ComputedStyle::clone(&layout_box.style);
                own.background_color = Rgba::TRANSPARENT;
                own.background_image = Arc::from([]);
                layout_box.style = Arc::new(own);
            }
        }
        self.canvas = Some(Arc::new(canvas));
    }

    /// Lays out the `html` element in the writing mode and direction of its `body`, as Chrome
    /// does: Chrome gives them to the viewport (CSS Writing Modes 4, §8, the principal writing
    /// mode), and the root box and the anonymous boxes in it take them, while the computed values
    /// that the root's other children inherit stay the root's own. The document's lines then run
    /// as the body's do, its pages break between them, and a body of a fixed width sits on the
    /// right of a right-to-left page. Not where either has containment (CSS Contain 2, §3.2 and
    /// §3.3; Chrome 157 for any containment).
    fn propagate_writing_mode(&mut self, document: &Document) {
        let Some(root) = self.root else { return };
        let is = |tree: &Self, id: BoxId, name: &str| {
            tree.get(id)
                .node
                .and_then(|node| document.element(node))
                .is_some_and(|element| element.is(name))
        };
        if !is(self, root, "html") || self.get(root).style.has_containment() {
            return;
        }
        let children = self.children(root);
        let Some(body) = children.iter().copied().find(|&child| is(self, child, "body")) else {
            return;
        };
        let body_style = &self.get(body).style;
        let (mode, direction) = (body_style.writing_mode, body_style.direction);
        let root_style = &self.get(root).style;
        if body_style.has_containment()
            || (root_style.writing_mode == mode && root_style.direction == direction)
        {
            return;
        }
        let principal = |style: &Arc<ComputedStyle>| {
            Arc::new(ComputedStyle::from(ComputedValues {
                writing_mode: mode,
                direction,
                ..ComputedValues::clone(style)
            }))
        };
        self.boxes[root.0 as usize].style = principal(&self.get(root).style);
        for child in children {
            let anonymous = self.get(child);
            if anonymous.node.is_none() && !anonymous.generated {
                self.boxes[child.0 as usize].style = principal(&anonymous.style);
            }
        }
    }

    /// The value of the page counter on the page the element with `id` starts on, as a first
    /// layout found it.
    pub(crate) fn target_page(&self, id: &str) -> Option<i32> {
        self.targets.pages.get(id).copied()
    }

    /// The counters at the elements with an id and whether `target-*()` functions were met,
    /// as the tree was built.
    pub fn found_targets(&self) -> &Targets {
        &self.found
    }
}

/// The style of a non-atomic inline box (or a ruby annotation) without the size, layout and
/// paint containment that do not apply to it (CSS Contain 2, §3), nor `content-visibility:
/// hidden` (§4): it is no containing block of positioned boxes, clips nothing and shows its
/// contents, as in Chrome (overflow does not apply to it either).
fn uncontained(style: &Arc<ComputedStyle>) -> Arc<ComputedStyle> {
    if !style.contain_layout && !style.contain_size && !style.contain_inline_size && !style.content_hidden {
        return Arc::clone(style);
    }
    Arc::new(ComputedStyle::from(ComputedValues {
        contain_layout: false,
        layout_containment: false,
        contain_size: false,
        contain_inline_size: false,
        content_hidden: false,
        overflow_x: fagus_style::values::Overflow::Visible,
        overflow_y: fagus_style::values::Overflow::Visible,
        ..ComputedValues::clone(style)
    }))
}

/// Puts the text of an element with `display: contents` (`style`), collected into `out` from
/// `start`, into inline boxes without decorations of their own: the text's font and line height
/// then count for the height of its lines, as Blink measures text by its own style (WPT
/// `display-contents-line-height`). Only the element's own text, not that of the boxes in it.
fn wrap_contents_text(out: &mut Vec<Collected>, start: usize, style: &Arc<ComputedStyle>) {
    let mut depth = 0_usize;
    let mut index = start;
    let mut wrapper: Option<Arc<ComputedStyle>> = None;
    while index < out.len() {
        match &out[index] {
            Collected::Inline(InlineItem::Open { .. }) => depth += 1,
            Collected::Inline(InlineItem::Close) => depth = depth.saturating_sub(1),
            // (White space alone, which may collapse away, makes no line of its own.)
            Collected::Inline(InlineItem::Text {
                style: text,
                text: content,
            }) if depth == 0 && Arc::ptr_eq(text, style) && !content.trim().is_empty() => {
                let wrapper = wrapper
                    .get_or_insert_with(|| {
                        Arc::new(ComputedStyle::from(ComputedValues {
                            display: Display::INLINE,
                            ..ComputedValues::from(style.inherit())
                        }))
                    })
                    .clone();
                out.insert(
                    index,
                    Collected::Inline(InlineItem::Open {
                        style: wrapper,
                        node: None,
                    }),
                );
                out.insert(index + 2, Collected::Inline(InlineItem::Close));
                index += 2;
            }
            _ => {}
        }
        index += 1;
    }
}

/// Whether a box draws a background: a color or an image.
pub(crate) fn has_background(style: &ComputedStyle) -> bool {
    !style.background_color.is_transparent()
        || style
            .background_image
            .iter()
            .any(|image| !matches!(image, fagus_style::values::Image::None))
}

struct Builder<'a> {
    document: &'a Document,
    styled: &'a StyledDocument,
    images: &'a Images,
    /// The fonts, which size text controls; without them, sizes are estimated from the font size.
    fonts: Option<&'a FontDb>,
    /// The language of form controls whose `lang` Fagus has no texts for, or that have none.
    locale: Option<&'a str>,
    counters: RefCell<Counters>,
    /// An outside marker that goes to the first line inside its list item, when that line lies in
    /// a descendant block.
    pending_marker: RefCell<Option<InlineItem>>,
    /// The style of the `::first-letter` of a block container whose first letter is still to
    /// come, in it or in a descendant block.
    pending_first_letter: RefCell<Option<Arc<ComputedStyle>>>,
    /// The same for `::first-line`.
    pending_first_line: RefCell<Option<Arc<ComputedStyle>>>,
    /// What the `target-*()` functions refer to, from a first layout.
    targets: &'a Targets,
    counter_styles: &'a CounterStyles,
    /// The counters at each element with an id, where generated content may ask for them.
    snapshots: Option<RefCell<FxHashMap<String, CounterValues>>>,
    /// Whether a `target-*()` function was met.
    asked: std::cell::Cell<bool>,
    /// How many quotes are open where the tree is built (CSS Generated Content 3, §4.3.1).
    quote_depth: std::cell::Cell<usize>,
    /// The elements with an id, made when a `target-text()` asks.
    ids: std::cell::OnceCell<FxHashMap<String, NodeId>>,
    /// Named strings set by elements (`string-set`), evaluated when the element is entered.
    strings: RefCell<NamedStrings>,
    /// The same assignments in document order.
    string_log: RefCell<Vec<(Arc<str>, StringValue)>>,
    /// Where the assignments of each element start in the string log.
    string_starts: RefCell<FxHashMap<NodeId, usize>>,
    /// The named strings of elements without a box of their own (`display: none` or
    /// `contents`, line breaks) since the last element with one, which the next one assigns
    /// (see `defer_strings`).
    deferred_strings: RefCell<Vec<(Arc<str>, StringValue)>>,
    /// What the children being collected are in: a block container, an inline box, or a table,
    /// row group or row.
    parent: std::cell::Cell<Parent>,
    /// The anonymous tables that further table parts may still join, with what they hold open.
    anonymous_tables: RefCell<FxHashMap<BoxId, OpenParts>>,
    /// The `display: contents` elements whose children are being collected, by style, with the
    /// style of the box their children go into, which anonymous tables take theirs from.
    contents_parents: RefCell<Vec<(Arc<ComputedStyle>, Arc<ComputedStyle>)>>,
    /// Whether the document has footnotes, which count with the `footnote` counter from its
    /// root.
    has_footnotes: bool,
    /// The number of the footnote whose call or marker is being built, and of the next.
    footnote: std::cell::Cell<Option<u32>>,
    next_footnote: std::cell::Cell<u32>,
    /// The footnote whose box is being built, with its number: its content starts with its
    /// marker.
    footnote_element: std::cell::Cell<Option<(NodeId, u32)>>,
}

/// Where the first letter of a text lies, as byte offsets: the first typographic character unit
/// after any space, with the marks that combine with it, the punctuation before it (with
/// typographic spaces between) and the punctuation after it that is neither opening nor a dash
/// (with typographic spaces between that are no word separators), as CSS Pseudo-Elements 4,
/// §2.2, has it and Chrome takes it; `None` if the text has none.
fn letter_span(text: &str) -> Option<(usize, usize)> {
    use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
    let preceding = |character: char| {
        matches!(
            character.general_category(),
            GeneralCategory::ConnectorPunctuation
                | GeneralCategory::DashPunctuation
                | GeneralCategory::OpenPunctuation
                | GeneralCategory::ClosePunctuation
                | GeneralCategory::InitialPunctuation
                | GeneralCategory::FinalPunctuation
                | GeneralCategory::OtherPunctuation
        )
    };
    let following = |character: char| {
        preceding(character)
            && !matches!(
                character.general_category(),
                GeneralCategory::DashPunctuation | GeneralCategory::OpenPunctuation
            )
    };
    let typographic_space = |character: char| {
        character.general_category() == GeneralCategory::SpaceSeparator && character != '\u{3000}'
    };
    let word_separator = |character: char| {
        matches!(
            character,
            ' ' | '\u{A0}' | '\u{1361}' | '\u{10100}' | '\u{10101}' | '\u{1039F}' | '\u{1091F}'
        )
    };
    let combining = |character: char| matches!(character as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F);
    let start = text.find(|character: char| !character.is_whitespace())?;
    let mut characters = text[start..]
        .char_indices()
        .map(|(offset, character)| (start + offset, character))
        .peekable();
    let mut punctuated = false;
    while characters
        .next_if(|(_, character)| preceding(*character) || (punctuated && typographic_space(*character)))
        .is_some()
    {
        punctuated = true;
    }
    let (_, letter) = characters.next()?;
    if letter.is_whitespace() {
        return None;
    }
    while characters
        .next_if(|(_, character)| combining(*character))
        .is_some()
    {}
    let next = |characters: &mut std::iter::Peekable<_>| {
        characters
            .peek()
            .map_or(text.len(), |(offset, _): &(usize, char)| *offset)
    };
    let mut end = next(&mut characters);
    // Spaces after the letter count only with punctuation after them.
    while let Some(&(_, character)) = characters.peek() {
        if typographic_space(character) && !word_separator(character) {
            characters.next();
        } else if following(character) {
            characters.next();
            end = next(&mut characters);
        } else {
            break;
        }
    }
    Some((start, end))
}

/// Whether an element is replaced by content from outside the document's flow: an image, an
/// inline `<svg>`, drawn as an SVG image, a `MathML` `<math>`, whose formula is laid out as a
/// whole (see [`crate::math`]), or an `<iframe>`, which shows a document of its own or none
/// (see [`crate::frame`]).
fn is_replaced(data: &fagus_dom::ElementData) -> bool {
    data.is("img")
        || data.is("canvas")
        || data.is("iframe")
        || data.is("video")
        || data.is_svg("svg")
        || crate::math::is_math(data)
}

/// The URL of the image an `<object>` (its `data`) or an `<embed>` (its `src`) shows, where
/// its `type`, if it has one, is that of an image.
pub fn embedded_image_url(data: &fagus_dom::ElementData) -> Option<&str> {
    let url = if data.is("object") {
        data.attribute("data")
    } else if data.is("embed") {
        data.attribute("src")
    } else {
        None
    };
    url.filter(|_| object_shows_image(data)).map(str::trim)
}

/// Whether the `type` of an `<object>`, if it has one, is that of an image, or of a PDF file,
/// whose page Fagus shows as an image.
fn object_shows_image(data: &fagus_dom::ElementData) -> bool {
    data.attribute("type").is_none_or(|kind| {
        let kind = kind.trim();
        kind.get(..6)
            .is_some_and(|start| start.eq_ignore_ascii_case("image/"))
            || kind
                .split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/pdf"))
    })
}

/// The picture of a `<canvas>`, which no script draws in print: transparent, of the size its
/// `width` and `height` attributes give it (300 × 150 by default, HTML §4.12.5).
fn blank_canvas(data: &fagus_dom::ElementData) -> Image {
    let size = |name: &str, default: u32| {
        data.attribute(name)
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(default)
    };
    let (width, height) = (size("width", 300), size("height", 150));
    let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\"/>");
    #[allow(clippy::cast_precision_loss, reason = "canvas sizes are far below 2^24")]
    let (width, height) = (width as f32, height as f32);
    Image {
        data: Arc::new(svg.into_bytes()),
        format: crate::image::ImageFormat::Svg,
        width,
        height,
        natural: (true, true),
        ratio: (height > 0.0).then(|| width / height),
        inline: false,
        orientation: 1,
        filters: Arc::from([]),
        pad: 0,
    }
}

/// What an `<iframe>` or a `<video>` without a poster shows, its content not being loaded: 300
/// by 150 pixels (the natural size in HTML, §15.4.1), without a ratio, so a `width` alone leaves
/// it 150 pixels tall, as in Chrome; filled with `fill` if given (stretched over the box, see
/// `preserveAspectRatio`), nothing otherwise.
fn frame(fill: Option<&str>) -> Image {
    let svg = match fill {
        Some(fill) => format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"300\" height=\"150\" viewBox=\"0 0 300 150\" \
             preserveAspectRatio=\"none\"><rect width=\"300\" height=\"150\" fill=\"{fill}\"/></svg>"
        ),
        None => "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"300\" height=\"150\"/>".to_owned(),
    };
    Image {
        data: Arc::new(svg.into_bytes()),
        format: crate::image::ImageFormat::Svg,
        width: 300.0,
        height: 150.0,
        natural: (true, true),
        ratio: None,
        inline: false,
        orientation: 1,
        filters: Arc::from([]),
        pad: 0,
    }
}

/// Content collected for a block container: block-level boxes and runs of inline items, or,
/// among the children of a table, a row group or a row, the table parts that go into it.
enum Collected {
    Block(BoxId),
    Inline(InlineItem),
    Part(TablePart),
}

impl Builder<'_> {
    /// Whether an element is replaced (see [`is_replaced`]), an `<object>` that shows an image
    /// and an element replaced by the image of its `content` included.
    fn is_replaced(&self, element: NodeId, data: &fagus_dom::ElementData) -> bool {
        is_replaced(data)
            || self.object_image(data).is_some()
            || self.images.frame(element).is_some()
            || self.content_image(element).is_some()
    }

    /// The image that replaces an element whose `content` is a single image that loads (CSS
    /// Content 3, §1.1), as in Chrome: the element is a replaced element showing it, whatever
    /// it holds or shows otherwise.
    pub(super) fn content_image(&self, element: NodeId) -> Option<&Arc<Image>> {
        let style = self.styled.style(element)?;
        let Content::Items(items) = &style.content else {
            return None;
        };
        let [ContentItem::Image(url)] = items.as_slice() else {
            return None;
        };
        self.images.get(url.trim())
    }

    /// The image an `<object>` or `<embed>` shows (see [`embedded_image_url`]), where that is
    /// an image that loads. Otherwise an `<object>` shows its content, as in Chrome.
    fn object_image(&self, data: &fagus_dom::ElementData) -> Option<&Arc<Image>> {
        self.images.get(embedded_image_url(data)?)
    }

    /// Creates the box for a block-level element.
    fn block_level(&self, tree: &mut BoxTree, element: NodeId, style: Arc<ComputedStyle>) -> BoxId {
        let data = self.document.element(element);
        // An image that does not load shows its text instead.
        if let Some(data) = data
            && let Some(id) = self.image_fallback(tree, element, data, &style)
        {
            return id;
        }
        if data.is_some_and(|data| self.is_replaced(element, data)) {
            return self.replaced(tree, element, &style);
        }
        if let Some(control) = data.and_then(forms::Control::of) {
            return self.control(tree, element, &control, &style);
        }
        if let Some(data) = data.filter(|data| data.is("button")) {
            let style = forms::button_style(&style, data.attribute("disabled").is_some());
            let id = self.block_level_box(tree, element, style);
            tree.shrinks_to_fit.insert(id);
            return id;
        }
        self.block_level_box(tree, element, style)
    }

    /// Creates the box for a block-level element that is neither replaced nor a form control.
    fn block_level_box(&self, tree: &mut BoxTree, element: NodeId, style: Arc<ComputedStyle>) -> BoxId {
        match style.display {
            Display::Normal {
                inside: DisplayInside::Table,
                ..
            } => self.table(tree, element, style),
            Display::Normal {
                inside: DisplayInside::Flex,
                ..
            } => {
                let children = self.container_items(tree, element, &style);
                tree.push(LayoutBox {
                    style,
                    node: Some(element),
                    generated: false,
                    kind: BoxKind::Flex { children },
                })
            }
            Display::Normal {
                inside: DisplayInside::Grid,
                ..
            } => {
                let children = self.container_items(tree, element, &style);
                tree.push(LayoutBox {
                    style,
                    node: Some(element),
                    generated: false,
                    kind: BoxKind::Grid { children },
                })
            }
            _ => self.block_container(tree, element, style),
        }
    }

    /// The items of a flex or grid container: the first line and letter of a block it is in
    /// do not go on into them, as a flex or grid container is no block container (CSS
    /// Pseudo-Elements 4, §2.1). An in-flow block-level one takes the place of that first
    /// line, which then has none; an inline-level, floated or absolutely positioned one leaves
    /// them to the content after it, as in Chrome.
    fn container_items(&self, tree: &mut BoxTree, element: NodeId, style: &Arc<ComputedStyle>) -> Vec<BoxId> {
        let line = self.pending_first_line.borrow_mut().take();
        let letter = self.pending_first_letter.borrow_mut().take();
        let children = self.item_children(tree, element, style);
        let inline = matches!(
            style.display,
            Display::Normal {
                outside: DisplayOutside::Inline,
                ..
            }
        );
        if inline || style.is_out_of_flow() || style.is_floating() {
            *self.pending_first_line.borrow_mut() = line;
            *self.pending_first_letter.borrow_mut() = letter;
        }
        children
    }

    /// A block container: its children become block-level boxes or inline content. Its first
    /// letter, in it or in the first line of a descendant, takes the style of its
    /// `::first-letter`.
    pub(super) fn block_container(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: Arc<ComputedStyle>,
    ) -> BoxId {
        let first_letter = self.styled.pseudo(element, PseudoKey::FirstLetter);
        if let Some(first_letter) = first_letter {
            *self.pending_first_letter.borrow_mut() = Some(Arc::clone(first_letter));
        }
        // The first line of a box goes on into the first in-flow box in it, not into a float or
        // an absolutely positioned box, which keep it for the boxes after them.
        let kept_first_line = if style.is_out_of_flow() || style.is_floating() {
            self.pending_first_line.borrow_mut().take()
        } else {
            None
        };
        let first_line = self.styled.pseudo(element, PseudoKey::FirstLine);
        if let Some(first_line) = first_line {
            *self.pending_first_line.borrow_mut() = Some(Arc::clone(first_line));
        }
        // The outside marker of a list item goes on the first line in its flow, not into a
        // float or positioned box before that line (as in Chrome).
        let outside_flow = style.is_floating() || style.is_out_of_flow();
        let marker = outside_flow
            .then(|| self.pending_marker.borrow_mut().take())
            .flatten();
        let in_flow_block = !outside_flow
            && matches!(
                style.display,
                Display::Normal {
                    outside: DisplayOutside::Block,
                    ..
                }
            );
        let id = self.block_container_content(tree, element, style);
        // The first formatted line of a box lies in its first in-flow block child, or nowhere
        // if that has none: a first letter that block did not take is not taken after it, as in
        // Chrome.
        if in_flow_block && first_letter.is_none() {
            *self.pending_first_letter.borrow_mut() = None;
        }
        if kept_first_line.is_some() {
            *self.pending_first_line.borrow_mut() = kept_first_line;
        }
        if let Some(marker) = marker {
            let mut pending = self.pending_marker.borrow_mut();
            if pending.is_none() {
                *pending = Some(marker);
            }
        }
        // The first letter and line lie in the box or nowhere.
        if first_letter.is_some() {
            *self.pending_first_letter.borrow_mut() = None;
        }
        if first_line.is_some() {
            *self.pending_first_line.borrow_mut() = None;
        }
        id
    }

    fn block_container_content(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: Arc<ComputedStyle>,
    ) -> BoxId {
        let is_fieldset = self
            .document
            .element(element)
            .is_some_and(|data| data.is("fieldset"));
        // A fieldset lays out its content in a formatting context of its own, which keeps the
        // margins of its first and last children inside it (Chrome's `FieldsetLayoutAlgorithm`).
        // Chrome does not trim the lines of that content (`text-box-trim`).
        let style = match style.display {
            Display::Normal {
                outside,
                inside: DisplayInside::Flow,
                list_item,
            } if is_fieldset => {
                let mut own = (*style).clone();
                own.display = Display::Normal {
                    outside,
                    inside: DisplayInside::FlowRoot,
                    list_item,
                };
                own.text_box_trim = fagus_style::values::TextBoxTrim::default();
                Arc::new(own)
            }
            _ => style,
        };
        let mut collected = Vec::new();
        if matches!(style.display, Display::Normal { list_item: true, .. }) {
            self.marker(element, &style, &mut collected);
        }
        self.collect_children(tree, element, &style, &mut collected);
        // An empty editing host has a line all the same, as in Chrome.
        if editable::is_editing_root(self.document, element) && editable::makes_no_line(&collected) {
            collected.push(Collected::Inline(InlineItem::Text {
                text: "\u{200B}".to_owned(),
                style: Arc::clone(&style),
            }));
        }
        let has_blocks = collected.iter().any(|item| matches!(item, Collected::Block(_)));
        if !has_blocks {
            let items = self.inline_content(tree, collected);
            let id = self.columns_or_box(tree, element, style, BoxKind::Inline { items });
            if let Some(first_line) = self.pending_first_line.borrow_mut().take() {
                tree.first_lines.insert(id, first_line);
            }
            return id;
        }
        let children = self.block_children(tree, &style, collected);
        if is_fieldset && let Some((style, children, legend)) = self.fieldset(tree, &style, &children) {
            let id = self.columns_or_box(tree, element, style, BoxKind::Block { children });
            tree.fieldsets.insert(id, legend);
            return id;
        }
        self.columns_or_box(tree, element, style, BoxKind::Block { children })
    }

    /// The inline items of a block container whose collected content is all inline, with the
    /// marker and first letter that are still to come.
    fn inline_content(&self, tree: &mut BoxTree, collected: Vec<Collected>) -> Vec<InlineItem> {
        let mut items: Vec<InlineItem> = collected
            .into_iter()
            .filter_map(|item| match item {
                Collected::Inline(item) => Some(item),
                Collected::Block(_) | Collected::Part(_) => None,
            })
            .collect();
        if let Some(marker) = self.pending_marker.borrow_mut().take() {
            items.insert(0, marker);
        }
        let items = process_white_space_noting(items, &mut tree.after_collapsed);
        let items = self.first_letter(tree, items);
        tree.link_ruby_columns(&items);
        items
    }

    /// The block-level children of a block container from its collected content: runs of inline
    /// items between blocks go into anonymous blocks.
    fn block_children(
        &self,
        tree: &mut BoxTree,
        style: &Arc<ComputedStyle>,
        collected: Vec<Collected>,
    ) -> Vec<BoxId> {
        let mut children = Vec::new();
        let mut run = Vec::new();
        // The inline boxes open around the items, which blocks inside them split (CSS 2.1
        // §9.2.1.1): the blocks move with them, and they go on after the blocks.
        let mut open: Vec<(Arc<ComputedStyle>, Option<NodeId>)> = Vec::new();
        // How many inline boxes the run opens again, after a block.
        let mut reopened = 0;
        for item in collected {
            match item {
                Collected::Inline(item) => {
                    match &item {
                        InlineItem::Open { style, node } => open.push((Arc::clone(style), *node)),
                        InlineItem::Close => {
                            open.pop();
                        }
                        _ => {}
                    }
                    run.push(item);
                }
                Collected::Block(child) => {
                    self.flush_run_reopened(tree, style, &mut run, &mut children, reopened);
                    let styles: Vec<&Arc<ComputedStyle>> = open.iter().map(|(style, _)| style).collect();
                    children.push(relative_in_inlines(tree, style, &styles, child));
                    run.extend(open.iter().map(|(style, node)| InlineItem::Open {
                        style: continued_inline(style),
                        node: *node,
                    }));
                    reopened = open.len();
                }
                Collected::Part(_) => {}
            }
        }
        self.flush_run_reopened(tree, style, &mut run, &mut children, reopened);
        children
    }

    /// The box of a block container with its content. In a multi-column container, the content
    /// goes into an anonymous box of its own, the flow that layout breaks into columns.
    fn columns_or_box(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: Arc<ComputedStyle>,
        kind: BoxKind,
    ) -> BoxId {
        let kind = if style.is_multicol() {
            let flow = tree.push(LayoutBox {
                style: Arc::new(anonymous_style(&style)),
                node: None,
                generated: false,
                kind,
            });
            BoxKind::Block { children: vec![flow] }
        } else {
            kind
        };
        tree.push(LayoutBox {
            style,
            node: Some(element),
            generated: false,
            kind,
        })
    }

    /// Wraps a run of inline items in an anonymous block, unless it is only collapsible space.
    fn flush_run(
        &self,
        tree: &mut BoxTree,
        parent: &Arc<ComputedStyle>,
        run: &mut Vec<InlineItem>,
        children: &mut Vec<BoxId>,
    ) {
        self.flush_run_reopened(tree, parent, run, children, 0);
    }

    /// [`Self::flush_run`] for a run whose first `reopened` inline boxes go on from before a
    /// block: their start edges being before the block, they make a line box only with an end
    /// edge in the run.
    fn flush_run_reopened(
        &self,
        tree: &mut BoxTree,
        parent: &Arc<ComputedStyle>,
        run: &mut Vec<InlineItem>,
        children: &mut Vec<BoxId>,
        reopened: usize,
    ) {
        let mut items = process_white_space_noting(std::mem::take(run), &mut tree.after_collapsed);
        let has_content = |items: &[InlineItem]| {
            let mut opened = 0;
            // The styles of the inline boxes open. An inline box makes a line box with a
            // margin, border or padding at its start, or at its end where it ends here, as in
            // Blink (`IsInlineBoxStartEmpty`, `IsInlineBoxEndEmpty`), also with a negative
            // margin; one that goes on from before a block has no start here.
            let mut open: Vec<&Arc<ComputedStyle>> = Vec::new();
            items.iter().any(|item| match item {
                InlineItem::Text { text, .. } => !text.is_empty(),
                InlineItem::LineBreak { .. }
                | InlineItem::Atomic(_)
                | InlineItem::Marker { .. }
                | InlineItem::NamedString { .. }
                | InlineItem::PageNumber { .. }
                | InlineItem::TargetPage { .. }
                | InlineItem::Leader { .. } => true,
                InlineItem::Open { style, .. } => {
                    opened += 1;
                    open.push(style);
                    opened > reopened && has_inline_edge(style, true)
                }
                InlineItem::Close => open.pop().is_some_and(|style| has_inline_edge(style, false)),
                InlineItem::OutOfFlow(_) | InlineItem::Float(_) => false,
            })
        };
        if has_content(&items) {
            items = self.first_letter(tree, items);
        }
        if has_content(&items)
            && let Some(marker) = self.pending_marker.borrow_mut().take()
        {
            items.insert(0, marker);
        }
        if has_content(&items) {
            tree.link_ruby_columns(&items);
            let mut style = anonymous_style(parent);
            // An anonymous item of a `-webkit-box` is sized like its other items.
            fagus_style::webkit_box::finish_item(&mut style, parent);
            // Only the first line of the element is indented: that of an anonymous block after
            // a block in the flow is not (CSS 2.1, §16.1; as in Chrome).
            if children.iter().any(|&child| {
                let child = &tree.get(child).style;
                !child.is_out_of_flow() && !child.is_floating()
            }) {
                style.text_indent = LengthPercentage::ZERO;
            }
            let style = Arc::new(style);
            let id = tree.push(LayoutBox {
                style,
                node: None,
                generated: false,
                kind: BoxKind::Inline { items },
            });
            if let Some(first_line) = self.pending_first_line.borrow_mut().take() {
                tree.first_lines.insert(id, first_line);
            }
            children.push(id);
        } else {
            // Without inline content around them, out-of-flow boxes and floats are children of
            // the block. An absolutely positioned box inside relatively positioned inline boxes
            // keeps them as its containing block, as the blocks inside them do (see
            // `relative_in_inlines`): it goes into the box around the blocks just before it in
            // the inline boxes they continue, or into one of its own.
            let mut open: Vec<Arc<ComputedStyle>> = Vec::new();
            for item in items {
                match item {
                    InlineItem::Open { style, .. } => open.push(style),
                    InlineItem::Close => {
                        open.pop();
                    }
                    InlineItem::OutOfFlow(id)
                        if open.iter().any(|inline| inline.position == Position::Relative) =>
                    {
                        let continued = open.len() <= reopened;
                        let around = children
                            .last()
                            .filter(|_| continued)
                            .and_then(|&last| relative_wrapper(tree, last, &open));
                        if let Some(wrapper) = around {
                            if let BoxKind::Block { children } = &mut tree.box_mut(wrapper).kind {
                                children.push(id);
                            }
                        } else {
                            let styles: Vec<&Arc<ComputedStyle>> = open.iter().collect();
                            children.push(relative_in_inlines(tree, parent, &styles, id));
                        }
                    }
                    InlineItem::OutOfFlow(id) | InlineItem::Float(id) => children.push(id),
                    _ => {}
                }
            }
        }
    }

    /// Inline content of the first line of a block container whose `::first-letter` is pending,
    /// with its first letter in a box of that style: inline, or a float for a drop cap. The
    /// letter takes the punctuation before and after it along (CSS Pseudo-Elements 4, §2.2).
    fn first_letter(&self, tree: &mut BoxTree, mut items: Vec<InlineItem>) -> Vec<InlineItem> {
        if self.pending_first_letter.borrow().is_none() {
            return items;
        }
        let Some(index) = items.iter().position(|item| match item {
            InlineItem::Text { text, .. } => !text.trim().is_empty(),
            InlineItem::Open { .. }
            | InlineItem::Close
            | InlineItem::OutOfFlow(_)
            | InlineItem::Float(_)
            | InlineItem::Marker { .. } => false,
            _ => true,
        }) else {
            return items;
        };
        // Whatever comes first, the first letter is decided.
        let Some(style) = self.pending_first_letter.borrow_mut().take() else {
            return items;
        };
        let InlineItem::Text {
            text,
            style: text_style,
        } = &items[index]
        else {
            return items;
        };
        let Some((mut start, end)) = letter_span(text) else {
            return items;
        };
        // Preserved spaces and tabs before the letter are part of it, as are the spaces that
        // never collapse (no-break, en, em and thin spaces), as in Chrome
        // (`FirstLetterPseudoElement::FirstLetterLength` takes the leading spaces).
        let preserved = !text_style.white_space.collapses_spaces();
        if text[..start].chars().all(|c| {
            if matches!(c, ' ' | '\t') {
                preserved
            } else {
                c.is_whitespace() && !matches!(c, '\n' | '\r')
            }
        }) {
            start = 0;
        }
        let (before, letter, mut after) = (&text[..start], &text[start..end], &text[end..]);
        // A letter taken out of the line (a float or an initial letter) that starts it leaves
        // the collapsible spaces after it at the start of the line, where they are removed, as
        // Chrome removes them.
        let floated = style.is_floating() || style.initial_letter.is_some();
        if floated
            && text_style.white_space.collapses_spaces()
            && before.trim_matches(' ').is_empty()
            && items[..index].iter().all(|item| {
                matches!(
                    item,
                    InlineItem::Open { .. }
                        | InlineItem::Close
                        | InlineItem::OutOfFlow(_)
                        | InlineItem::Float(_)
                )
            })
        {
            after = after.trim_start_matches(' ');
        }
        let mut replacement = Vec::new();
        if !before.is_empty() {
            replacement.push(InlineItem::Text {
                text: before.to_owned(),
                style: Arc::clone(text_style),
            });
        }
        let letter = InlineItem::Text {
            text: letter.to_owned(),
            style: Arc::clone(&style),
        };
        // An initial letter is placed like a float at the start of the line (CSS Inline 3,
        // §7.5), on the right in right-to-left text; one raised above the first line pushes the
        // text down by as many lines.
        if floated {
            let float = if style.initial_letter.is_some() {
                if style.is_rtl() {
                    fagus_style::values::Float::Right
                } else {
                    fagus_style::values::Float::Left
                }
            } else {
                style.float
            };
            let id = tree.push(LayoutBox {
                style: Arc::new(ComputedStyle::from(ComputedValues {
                    display: style.display.blockify(),
                    float,
                    ..ComputedValues::clone(&style)
                })),
                node: None,
                generated: true,
                kind: BoxKind::Inline { items: vec![letter] },
            });
            replacement.push(InlineItem::Float(id));
            if let Some(lines) = style.initial_letter {
                // At most `MAX_RAISED_LINES`: a letter raised by billions of lines
                // took as many breaks (and gigabytes).
                let raised = (crate::inline::raised_lines(lines) as usize).min(MAX_RAISED_LINES);
                for _ in 0..raised {
                    replacement.push(InlineItem::LineBreak {
                        style: Arc::clone(text_style),
                    });
                }
            }
        } else {
            replacement.extend([
                InlineItem::Open {
                    style: Arc::clone(&style),
                    node: None,
                },
                letter,
                InlineItem::Close,
            ]);
        }
        if !after.is_empty() {
            replacement.push(InlineItem::Text {
                text: after.to_owned(),
                style: Arc::clone(text_style),
            });
        }
        items.splice(index..=index, replacement);
        items
    }

    /// Collects the boxes and inline items of an element's children, including `::before` and
    /// `::after`.
    fn collect_children(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: &Arc<ComputedStyle>,
        out: &mut Vec<Collected>,
    ) {
        let outer = self.parent.replace(Parent::Block);
        self.collect_children_with(tree, element, style, out, None);
        self.parent.set(outer);
    }

    /// Like `collect_children`, for the children of an inline box.
    fn collect_inline_children(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: &Arc<ComputedStyle>,
        out: &mut Vec<Collected>,
        ruby: bool,
    ) {
        let outer = self.parent.replace(Parent::Inline);
        if ruby {
            self.collect_ruby(tree, element, style, out);
        } else {
            self.collect_children_with(tree, element, style, out, None);
        }
        self.parent.set(outer);
    }

    /// Like `collect_children`, or for the one child `only`, without `::before` and `::after`.
    fn collect_children_with(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: &Arc<ComputedStyle>,
        out: &mut Vec<Collected>,
        only: Option<NodeId>,
    ) {
        // `content-visibility: hidden` skips the contents of the box.
        if style.content_hidden {
            return;
        }
        if only.is_none() {
            // A footnote starts with its marker (CSS Generated Content for Paged Media 3, §2.4).
            if let Some((footnote, ordinal)) = self.footnote_element.get()
                && footnote == element
            {
                let outer = self.footnote.replace(Some(ordinal));
                self.pseudo(tree, element, PseudoKey::FootnoteMarker, false, out);
                self.footnote.set(outer);
            }
            self.pseudo(tree, element, PseudoKey::Before, false, out);
        }
        let details = self.details(element);
        if only.is_none()
            && let Some(details) = details
            && details.summary.is_none()
        {
            out.push(Collected::Block(self.default_summary(tree, style, details.open)));
        }
        // The element before, whose scroll buttons and scroll marker group after it are still to
        // come (CSS Overflow 5).
        let mut scrolled: Option<NodeId> = None;
        for child in self.document.children(element) {
            if let Some(before) = scrolled.take() {
                self.scroll_boxes_after(tree, before, false, out);
            }
            if only.is_some_and(|only| only != child) || details.is_some_and(|details| !details.shows(child))
            {
                continue;
            }
            if let Some(text) = self.document.text(child) {
                if !self.drops_white_space(out, text, style) {
                    out.push(Collected::Inline(InlineItem::Text {
                        text: text.to_owned(),
                        style: Arc::clone(style),
                    }));
                }
                continue;
            }
            let Some(data) = self.document.element(child) else {
                continue;
            };
            let Some(child_style) = self.styled.style(child) else {
                continue;
            };
            if self.is_demoted_form(element, data) {
                continue;
            }
            // `display: contents` on replaced elements and form controls is `none` (HTML,
            // §15.3.1; CSS Display 3, Appendix B), as in Chrome.
            if child_style.display == Display::Contents
                && [
                    "img", "canvas", "embed", "object", "audio", "video", "iframe", "input", "textarea",
                    "select", "meter", "progress", "frame", "frameset",
                ]
                .iter()
                .any(|name| data.is(name))
            {
                continue;
            }
            // (An element with `display: contents` has its buttons after its content.)
            if child_style.display != Display::None {
                self.scroll_boxes_before(tree, child, child_style, false, out);
                scrolled = Some(child);
            }
            let strings_before = self.string_log.borrow().len();
            if child_style.display == Display::None {
                self.defer_strings(child, child_style);
            } else {
                self.enter(child, child_style);
            }
            if child_style.footnote {
                self.footnote(tree, child, child_style, out);
                continue;
            }
            if child_style.is_out_of_flow() && child_style.display != Display::None {
                let id = self.block_level(tree, child, Arc::clone(child_style));
                if child_style.position == Position::Fixed {
                    tree.fixed.push(id);
                }
                out.push(Collected::Inline(InlineItem::OutOfFlow(id)));
                continue;
            }
            if child_style.is_floating() && child_style.display != Display::None {
                let id = self.block_level(tree, child, Arc::clone(child_style));
                out.push(Collected::Inline(InlineItem::Float(id)));
                continue;
            }
            // An image that does not load shows its text instead.
            if child_style.display != Display::None
                && let Some(id) = self.image_fallback(tree, child, data, child_style)
            {
                if child_style.display.is_block_level() {
                    let carried = self.delegate_marker(out, element, child);
                    out.push(Collected::Block(id));
                    out.extend(carried.map(Collected::Inline));
                } else {
                    out.push(Collected::Inline(InlineItem::Atomic(id)));
                }
                continue;
            }
            if (self.is_replaced(child, data) || forms::Control::of(data).is_some())
                && child_style.display != Display::None
            {
                let control = forms::Control::of(data);
                let widget = control.is_some() && tables::widget_inline(child_style);
                let id = match control {
                    Some(control) => self.control(tree, child, &control, child_style),
                    None => self.replaced(tree, child, child_style),
                };
                if child_style.display.is_block_level() && !widget {
                    // `justify-self: stretch` stretches a block-level form control that would
                    // shrink to fit.
                    if self.parent.get() == Parent::Block
                        && crate::block_justify::stretches(&self.box_parent_style(style), child_style)
                    {
                        tree.shrinks_to_fit.remove(&id);
                    }
                    // An outside marker goes past a block-level image to the first line after
                    // it, if there is one.
                    let carried = (self.marker_ahead(out) && self.rest_makes_lines(element, child))
                        .then(|| self.marker_here(out))
                        .flatten();
                    out.push(Collected::Block(id));
                    out.extend(carried.map(Collected::Inline));
                } else {
                    out.push(Collected::Inline(InlineItem::Atomic(id)));
                }
                continue;
            }
            match child_style.display {
                Display::None => {}
                Display::Contents => {
                    let parent = self.box_parent_style(style);
                    self.contents_parents
                        .borrow_mut()
                        .push((Arc::clone(child_style), parent));
                    let start = out.len();
                    self.collect_children_with(tree, child, child_style, out, None);
                    self.contents_parents.borrow_mut().pop();
                    wrap_contents_text(out, start, child_style);
                }
                _ if data.is("br") => out.push(Collected::Inline(InlineItem::LineBreak {
                    style: Arc::clone(child_style),
                })),
                // `<wbr>` is a zero width space where lines may break, as in Blink, even where
                // they do not wrap otherwise.
                _ if data.is("wbr") => {
                    let mut style = (**child_style).clone();
                    style.white_space.nowrap = false;
                    out.push(Collected::Inline(InlineItem::Text {
                        text: "\u{200B}".to_owned(),
                        style: Arc::new(style),
                    }));
                }
                // A fieldset or a button is always a block container: `display: inline` makes
                // it an inline-block (HTML, §15.3.13 and §15.5.3), as in Chrome.
                Display::Normal {
                    outside: DisplayOutside::Inline,
                    inside: DisplayInside::Flow,
                    list_item,
                } if data.is("fieldset") || data.is("button") => {
                    let block_style = Arc::new(ComputedStyle::from(ComputedValues {
                        display: Display::Normal {
                            outside: DisplayOutside::Block,
                            inside: DisplayInside::FlowRoot,
                            list_item,
                        },
                        ..ComputedValues::clone(child_style)
                    }));
                    let id = self.block_level(tree, child, block_style);
                    out.push(Collected::Inline(InlineItem::Atomic(id)));
                }
                Display::Normal {
                    outside: DisplayOutside::Inline,
                    inside: DisplayInside::Flow,
                    ..
                } => {
                    let child_style = &uncontained(child_style);
                    out.push(Collected::Inline(InlineItem::Open {
                        style: Arc::clone(child_style),
                        node: Some(child),
                    }));
                    // An inline list item has its marker inside, at its start, as in Chrome.
                    if matches!(child_style.display, Display::Normal { list_item: true, .. }) {
                        let inside = ComputedStyle::from(ComputedValues {
                            list_style_position: ListStylePosition::Inside,
                            ..ComputedValues::clone(child_style)
                        });
                        self.marker(child, &inside, out);
                    }
                    let ruby = child_style.ruby_display == RubyDisplay::Container;
                    self.collect_inline_children(tree, child, child_style, out, ruby);
                    out.push(Collected::Inline(InlineItem::Close));
                }
                Display::Normal {
                    outside: DisplayOutside::Inline,
                    ..
                } => {
                    // Laid out as a block container of its own kind, placed like a glyph.
                    let block_style = Arc::new(ComputedStyle::from(ComputedValues {
                        display: child_style.display.blockify(),
                        ..ComputedValues::clone(child_style)
                    }));
                    let id = self.block_level(tree, child, block_style);
                    out.push(Collected::Inline(InlineItem::Atomic(id)));
                }
                // A button is no table part: with its native look it is laid out as an
                // inline-block, as in Chrome, and as a block without.
                Display::Internal(_) if data.is("button") => {
                    let block_style = Arc::new(ComputedStyle::from(ComputedValues {
                        display: Display::BLOCK,
                        ..ComputedValues::clone(child_style)
                    }));
                    let id = self.block_level(tree, child, block_style);
                    if tables::widget_inline(child_style) {
                        out.push(Collected::Inline(InlineItem::Atomic(id)));
                    } else {
                        let carried = self.delegate_marker(out, element, child);
                        out.push(Collected::Block(id));
                        out.extend(carried.map(Collected::Inline));
                    }
                }
                // A fieldset is no table part either: it is a block (HTML, §15.3.13), as in
                // Chrome (WPT `table-display-on-fieldset`).
                Display::Internal(_) if data.is("fieldset") => {
                    let block_style = Arc::new(ComputedStyle::from(ComputedValues {
                        display: Display::BLOCK,
                        ..ComputedValues::clone(child_style)
                    }));
                    let id = self.block_level(tree, child, block_style);
                    let carried = self.delegate_marker(out, element, child);
                    out.push(Collected::Block(id));
                    out.extend(carried.map(Collected::Inline));
                }
                Display::Internal(_) => {
                    let part = self.table_part(tree, child, child_style, strings_before);
                    self.add_to_parent(tree, style, out, part);
                }
                Display::Normal { .. } => {
                    let carried = self.delegate_marker(out, element, child);
                    let id = self.block_level(tree, child, Arc::clone(child_style));
                    // `justify-self` other than `normal` and `stretch` shrinks it to fit.
                    if self.parent.get() == Parent::Block
                        && crate::block_justify::shrinks(&self.box_parent_style(style), child_style)
                    {
                        tree.shrinks_to_fit.insert(id);
                    }
                    out.push(Collected::Block(id));
                    out.extend(carried.map(Collected::Inline));
                }
            }
        }
        if let Some(before) = scrolled {
            self.scroll_boxes_after(tree, before, false, out);
        }
        if only.is_none() {
            self.pseudo(tree, element, PseudoKey::After, false, out);
            self.counters.borrow_mut().leave(self.depth(element));
        }
    }

    /// Collects the content of a ruby container (CSS Ruby 1, §2): each annotation (`rt`) pairs
    /// with the content before it since the last one, its base, into a ruby column, which is
    /// laid out as one atomic inline. As in Chrome, `rb` elements are part of the base like any
    /// other inline, and an annotation right after another has an empty base. Content after the
    /// last annotation stays inline.
    fn collect_ruby(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: &Arc<ComputedStyle>,
        out: &mut Vec<Collected>,
    ) {
        let mut base = Vec::new();
        self.pseudo(tree, element, PseudoKey::Before, false, &mut base);
        for child in self.document.children(element) {
            let annotation_style = self
                .styled
                .style(child)
                .filter(|child_style| {
                    child_style.ruby_display == RubyDisplay::Text && child_style.display != Display::None
                })
                .filter(|_| self.document.element(child).is_some());
            let Some(annotation_style) = annotation_style else {
                self.collect_children_with(tree, element, style, &mut base, Some(child));
                continue;
            };
            // Containment does not apply to internal ruby boxes (CSS Contain 2, §3).
            let annotation_style = &uncontained(annotation_style);
            self.enter(child, annotation_style);
            let mut annotation = vec![Collected::Inline(InlineItem::Open {
                style: Arc::clone(annotation_style),
                node: Some(child),
            })];
            self.collect_children_with(tree, child, annotation_style, &mut annotation, None);
            annotation.push(Collected::Inline(InlineItem::Close));
            let base = std::mem::take(&mut base);
            if base
                .iter()
                .chain(&annotation)
                .any(|item| matches!(item, Collected::Block(_)))
            {
                // Blocks in ruby break it up: what it holds goes on inline.
                out.extend(base);
                out.extend(annotation);
                continue;
            }
            let inline = |items: Vec<Collected>| -> Vec<InlineItem> {
                items
                    .into_iter()
                    .filter_map(|item| match item {
                        Collected::Inline(item) => Some(item),
                        Collected::Block(_) | Collected::Part(_) => None,
                    })
                    .collect()
            };
            let column_style = Arc::new(ComputedStyle::from(ComputedValues {
                display: Display::Normal {
                    outside: DisplayOutside::Inline,
                    inside: DisplayInside::FlowRoot,
                    list_item: false,
                },
                ruby_display: RubyDisplay::Column,
                ..ComputedValues::from(style.inherit())
            }));
            let id = tree.push(LayoutBox {
                style: column_style,
                node: None,
                generated: true,
                kind: BoxKind::Inline {
                    items: process_white_space(inline(base)),
                },
            });
            tree.ruby_columns.insert(
                id,
                crate::ruby::RubyColumn {
                    style: Arc::clone(annotation_style),
                    items: crate::ruby::annotation_items(process_white_space(inline(annotation))),
                    overhang: (None, None),
                },
            );
            out.push(Collected::Inline(InlineItem::Atomic(id)));
        }
        self.pseudo(tree, element, PseudoKey::After, false, &mut base);
        out.extend(base);
        self.counters.borrow_mut().leave(self.depth(element));
    }

    /// The depth of an element in the document (the root element is 1).
    fn depth(&self, element: NodeId) -> usize {
        let mut depth = 0;
        let mut current = Some(element);
        while let Some(node) = current {
            depth += 1;
            current = self.document.parent_element(node);
        }
        depth
    }

    /// Applies an element's counter properties when the box tree reaches it in document order.
    /// Lists also take their `start`, `reversed` and `value` attributes (HTML, §4.4.5).
    fn enter(&self, element: NodeId, style: &ComputedStyle) {
        let depth = self.depth(element);
        let data = self.document.element(element);
        let mut counters = self.counters.borrow_mut();
        let names = |directives: &[(String, i32)]| directives.iter().any(|(name, _)| name == "footnote");
        // Footnotes count from the root (CSS Generated Content for Paged Media 3, §2.5), and
        // each counts itself unless it says otherwise, as list items count `list-item`.
        if self.has_footnotes
            && self.document.document_element() == Some(element)
            && !names(&style.counter_reset)
        {
            counters.reset("footnote", 0, depth);
        }
        for (name, value) in &style.counter_reset {
            counters.reset(name, *value, depth);
        }
        if style.footnote && !names(&style.counter_increment) && !names(&style.counter_set) {
            counters.increment("footnote", 1, depth);
        }
        // Lists and their items count `list-item` unless the element sets it itself, as Chrome
        // does instead of a user agent rule (CSS Lists 3, §4.6).
        let own_list_item = [&style.counter_reset, &style.counter_increment, &style.counter_set]
            .iter()
            .any(|directives| directives.iter().any(|(name, _)| name == "list-item"));
        let resets_list_item = style.counter_reset.iter().any(|(name, _)| name == "list-item");
        if let Some(data) = data
            && data.is("ol")
        {
            let reversed = data.attribute("reversed").is_some();
            let start = data
                .attribute("start")
                .and_then(|start| start.trim().parse::<i32>().ok());
            if !own_list_item {
                let value = if reversed {
                    start.unwrap_or(0).saturating_add(1)
                } else {
                    start.unwrap_or(1).saturating_sub(1)
                };
                counters.reset("list-item", value, depth);
            }
            if !own_list_item || resets_list_item {
                // Markers count from `start`, or down from the number of items.
                let base = if reversed {
                    let items = self
                        .document
                        .child_elements(element)
                        .filter(|&child| self.document.element(child).is_some_and(|child| child.is("li")))
                        .count();
                    start
                        .unwrap_or(i32::try_from(items).unwrap_or(i32::MAX))
                        .saturating_add(1)
                } else {
                    start.unwrap_or(1).saturating_sub(1)
                };
                counters.number_list(base, depth);
                if reversed {
                    counters.reverse_list_item(depth);
                }
            }
        } else if !own_list_item
            && data.is_some_and(|data| ["ul", "menu", "dir"].iter().any(|name| data.is(name)))
        {
            counters.reset("list-item", 0, depth);
        }
        let list_item = matches!(style.display, Display::Normal { list_item: true, .. });
        if list_item && !own_list_item {
            counters.increment_list_item(depth);
        }
        for (name, by) in &style.counter_increment {
            counters.increment(name, *by, depth);
        }
        if list_item
            && let Some(value) = data
                .and_then(|data| data.attribute("value"))
                .and_then(|value| value.trim().parse::<i32>().ok())
        {
            counters.number_item(value, depth);
        }
        for (name, value) in &style.counter_set {
            counters.set(name, *value, depth);
        }
        // The counters at an element with an id, for `target-counter()`.
        if let Some(snapshots) = &self.snapshots
            && let Some(id) = data.and_then(|data| data.id.as_deref())
        {
            snapshots
                .borrow_mut()
                .entry(id.to_owned())
                .or_insert_with(|| counters.snapshot());
        }
        drop(counters);
        // Named strings take the values the element has here, its counters included, after
        // those of the elements without a box before it.
        let mut values = self.deferred_strings.take();
        values.extend(self.string_values(element, style));
        // An element with `display: contents` and a line break have no box of their own: the
        // next element's box assigns their strings.
        if style.display == Display::Contents || data.is_some_and(|data| data.is("br") || data.is("wbr")) {
            *self.deferred_strings.borrow_mut() = values;
            return;
        }
        if !values.is_empty() {
            let mut log = self.string_log.borrow_mut();
            self.string_starts.borrow_mut().insert(element, log.len());
            log.extend(values.iter().cloned());
            self.strings.borrow_mut().insert(element, values);
        }
    }

    /// The named strings an element with `display: none` sets: they are assigned where its box
    /// would have been (CSS GCPM 3, §1.1.1), which is where the next element's box starts. Its
    /// counters do not change (CSS Display 3, §2.5).
    fn defer_strings(&self, element: NodeId, style: &ComputedStyle) {
        let values = self.string_values(element, style);
        self.deferred_strings.borrow_mut().extend(values);
    }

    /// The values of the named strings `element` sets with `string-set`, taken where the box tree
    /// has come to: the counters as they are there, the page counters left for the page the
    /// element goes on.
    fn string_values(&self, element: NodeId, style: &ComputedStyle) -> Vec<(Arc<str>, StringValue)> {
        style
            .string_set
            .iter()
            .map(|(name, items)| {
                let mut value = StringValue::default();
                for item in items {
                    match item {
                        ContentItem::Counter { name, style: format }
                            if let Some(counter) = PageCounter::from_name(name) =>
                        {
                            value.0.push(StringPart::Page {
                                counter,
                                format: format.clone(),
                            });
                        }
                        item => value.push_text(&self.content_text(element, std::slice::from_ref(item))),
                    }
                }
                (Arc::from(name.as_str()), value)
            })
            .collect()
    }

    /// Applies the counter properties of a pseudo-element at `depth`.
    fn apply_counters(&self, style: &ComputedStyle, depth: usize) {
        let mut counters = self.counters.borrow_mut();
        for (name, value) in &style.counter_reset {
            counters.reset(name, *value, depth);
        }
        for (name, by) in &style.counter_increment {
            counters.increment(name, *by, depth);
        }
        for (name, value) in &style.counter_set {
            counters.set(name, *value, depth);
        }
    }

    /// The text of generated content.
    /// The text of a quote in generated content, as deep as the quotes before it in the
    /// document: one of the element's `quotes`, or for `auto` of the language of its parent,
    /// as Chrome takes it.
    fn quote(&self, element: NodeId, item: &ContentItem) -> String {
        let depth = self.quote_depth.get();
        let open = matches!(item, ContentItem::OpenQuote | ContentItem::NoOpenQuote);
        let at = if open {
            self.quote_depth.set(depth + 1);
            depth
        } else if depth > 0 {
            self.quote_depth.set(depth - 1);
            depth - 1
        } else {
            return String::new();
        };
        if matches!(item, ContentItem::NoOpenQuote | ContentItem::NoCloseQuote) {
            return String::new();
        }
        let lang = self
            .document
            .parent_element(element)
            .and_then(|parent| self.styled.style(parent))
            .and_then(|style| style.lang.clone());
        let quotes = self.styled.style(element).and_then(|style| style.quotes.clone());
        crate::quotes::quote(quotes.as_deref(), lang.as_deref(), at, open)
    }

    fn content_text(&self, element: NodeId, items: &[ContentItem]) -> String {
        let counters = self.counters.borrow();
        items
            .iter()
            .map(|item| match item {
                ContentItem::String(text) => text.clone(),
                ContentItem::Attr(name) => self
                    .document
                    .element(element)
                    .and_then(|data| data.attribute(name))
                    .unwrap_or_default()
                    .to_owned(),
                ContentItem::Counter { name, style } => {
                    counter_text(counters.value(name), style, self.counter_styles)
                }
                ContentItem::Counters {
                    name,
                    separator,
                    style,
                } => counters
                    .values(name)
                    .iter()
                    .map(|value| counter_text(*value, style, self.counter_styles))
                    .collect::<Vec<_>>()
                    .join(separator),
                ContentItem::OpenQuote
                | ContentItem::CloseQuote
                | ContentItem::NoOpenQuote
                | ContentItem::NoCloseQuote => self.quote(element, item),
                ContentItem::Content(target) => self.content_of(element, *target),
                ContentItem::TargetCounter { target, name, style } => self
                    .target_counters(element, target, name)
                    .and_then(|values| values.last().copied())
                    .map(|value| counter_text(value, style, self.counter_styles))
                    .unwrap_or_default(),
                ContentItem::TargetCounters {
                    target,
                    name,
                    separator,
                    style,
                } => self
                    .target_counters(element, target, name)
                    .map(|values| {
                        values
                            .iter()
                            .map(|value| counter_text(*value, style, self.counter_styles))
                            .collect::<Vec<_>>()
                            .join(separator)
                    })
                    .unwrap_or_default(),
                ContentItem::TargetText { target, which } => {
                    self.asked.set(true);
                    self.target(element, target)
                        .and_then(|id| self.element_by_id(&id))
                        .map(|node| self.content_of(node, *which))
                        .unwrap_or_default()
                }
                _ => String::new(),
            })
            .collect()
    }

    /// The id of the element a `target-*()` function of generated content of `element` points
    /// to: the fragment of its URL.
    fn target(&self, element: NodeId, target: &fagus_style::values::TargetUrl) -> Option<String> {
        let url = match target {
            fagus_style::values::TargetUrl::Attr(name) => self.document.element(element)?.attribute(name)?,
            fagus_style::values::TargetUrl::Url(url) => url.as_str(),
        };
        let (_, fragment) = url.trim().split_once('#')?;
        (!fragment.is_empty()).then(|| fragment.to_owned())
    }

    /// The values of the counter `name` at the element a `target-counter()` points to, as a
    /// first layout found them.
    fn target_counters(
        &self,
        element: NodeId,
        target: &fagus_style::values::TargetUrl,
        name: &str,
    ) -> Option<Vec<i32>> {
        self.asked.set(true);
        let id = self.target(element, target)?;
        let counters = self.targets.counters.get(&id)?;
        counters
            .iter()
            .find(|(counter, _)| counter == name)
            .map(|(_, values)| values.clone())
    }

    fn element_by_id(&self, id: &str) -> Option<NodeId> {
        self.ids
            .get_or_init(|| {
                self.document
                    .descendants(NodeId::DOCUMENT)
                    .filter_map(|node| Some((self.document.element(node)?.id.as_deref()?.to_owned(), node)))
                    .collect()
            })
            .get(id)
            .copied()
    }

    /// The text `content()` stands for in `string-set` (CSS GCPM 3, §1.1): the element's text
    /// with white space collapsed, or the content of its pseudo-elements.
    fn content_of(&self, element: NodeId, target: ContentTarget) -> String {
        let text = || {
            self.document
                .text_content(element)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let generated = |key: PseudoKey| match self.styled.pseudo(element, key).map(|style| &style.content) {
            // `content()` refers to the element; inside generated content it means nothing.
            Some(Content::Items(items)) => self.content_text(
                element,
                &items
                    .iter()
                    .filter(|item| !matches!(item, ContentItem::Content(_)))
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            _ => String::new(),
        };
        match target {
            ContentTarget::Text => text(),
            ContentTarget::FirstLetter => {
                // The first letter with the punctuation before it (CSS Pseudo 4, §2.3).
                let text = text();
                let end = text
                    .char_indices()
                    .find(|(_, character)| character.is_alphanumeric())
                    .map_or(text.len(), |(index, character)| index + character.len_utf8());
                text[..end].to_owned()
            }
            ContentTarget::Before => generated(PseudoKey::Before),
            ContentTarget::After => generated(PseudoKey::After),
            ContentTarget::Marker => self
                .styled
                .style(element)
                .and_then(|style| {
                    marker_text(
                        self.counters.borrow().ordinal(),
                        &style.list_style_type,
                        self.counter_styles,
                    )
                })
                .unwrap_or_default(),
        }
    }

    /// The marker of a list item (CSS Lists 3, §3): `::marker` content, or the counter in the
    /// item's `list-style-type`.
    fn marker(&self, element: NodeId, style: &ComputedStyle, out: &mut Vec<Collected>) {
        self.marker_of(element, style, PseudoKey::Marker, out);
    }

    /// The marker of the list item `element` or its `::before` or `::after` (`key` says which
    /// marker), whose style is `style`.
    fn marker_of(&self, element: NodeId, style: &ComputedStyle, key: PseudoKey, out: &mut Vec<Collected>) {
        let Some(marker_style) = self.styled.pseudo(element, key) else {
            return;
        };
        let inside = style.list_style_position == ListStylePosition::Inside;
        // An image that loads stands in for the marker of the type.
        if matches!(marker_style.content, Content::Normal)
            && let fagus_style::values::Image::Url(url, resolution) = &style.list_style_image
            && let Some(image) = self.images.get(url)
        {
            out.push(Collected::Inline(InlineItem::Marker {
                text: String::new(),
                style: Arc::clone(marker_style),
                symbol: None,
                inside,
                image: Some(Image::zoomed(
                    &Image::styled(&Image::at_resolution(image, *resolution), marker_style),
                    marker_style.zoom,
                )),
            }));
            return;
        }
        // So does the image of the marker's `content`.
        if let Content::Items(items) = &marker_style.content
            && let [ContentItem::Image(url)] = items.as_slice()
        {
            if let Some(image) = self.images.get(url.trim()) {
                out.push(Collected::Inline(InlineItem::Marker {
                    text: String::new(),
                    style: Arc::clone(marker_style),
                    symbol: None,
                    inside,
                    image: Some(Image::zoomed(
                        &Image::styled(image, marker_style),
                        marker_style.zoom,
                    )),
                }));
            }
            return;
        }
        let text = match &marker_style.content {
            Content::Items(items) => Some(self.content_text(element, items)),
            Content::None => None,
            Content::Normal => marker_text(
                self.counters.borrow().ordinal(),
                &style.list_style_type,
                self.counter_styles,
            ),
        };
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return;
        };
        // Chrome draws the symbols of these styles as shapes.
        let symbol = match &style.list_style_type {
            _ if !matches!(marker_style.content, Content::Normal) => None,
            ListStyleType::Disc => Some(("\u{2022}", SymbolShape::Disc)),
            ListStyleType::Circle => Some(("\u{25E6}", SymbolShape::Circle)),
            ListStyleType::Square => Some(("\u{25AA}", SymbolShape::Square)),
            ListStyleType::Named(name) if name.eq_ignore_ascii_case("disclosure-open") => {
                Some(("\u{25BE}", SymbolShape::Disclosure { open: true }))
            }
            ListStyleType::Named(name) if name.eq_ignore_ascii_case("disclosure-closed") => {
                Some(("\u{25B8}", SymbolShape::Disclosure { open: false }))
            }
            _ => None,
        };
        if let Some((text, shape)) = symbol {
            out.push(Collected::Inline(InlineItem::Marker {
                text: text.to_owned(),
                style: Arc::clone(marker_style),
                symbol: Some(shape),
                inside,
                image: None,
            }));
        } else if inside {
            out.push(Collected::Inline(InlineItem::Open {
                style: Arc::clone(marker_style),
                node: None,
            }));
            // Lines may break after the space that ends the marker, as in Chrome, also where
            // the marker keeps its spaces (as the item's `white-space` says): a zero width
            // space, which draws nothing, ends it there.
            let text = if text.ends_with(' ') && !marker_style.white_space.collapses_spaces() {
                text + "\u{200B}"
            } else {
                text
            };
            out.push(Collected::Inline(InlineItem::Text {
                text,
                style: Arc::clone(marker_style),
            }));
            out.push(Collected::Inline(InlineItem::Close));
        } else {
            out.push(Collected::Inline(InlineItem::Marker {
                text,
                style: Arc::clone(marker_style),
                symbol: None,
                inside,
                image: None,
            }));
        }
    }

    /// Before a block child of a list item (or of a block that its outside marker went to)
    /// while no text came before it: the marker moves down to the first line inside that
    /// block, as in Chrome, which places it beside the first line box. A block without lines
    /// (empty, or holding only empty blocks or block-level images) lets it pass to the lines
    /// after it, which the marker returned is to go before; where no line follows, the marker
    /// goes into the block all the same.
    fn delegate_marker(
        &self,
        out: &mut Vec<Collected>,
        element: NodeId,
        child: NodeId,
    ) -> Option<InlineItem> {
        // Without a marker, there is nothing to look for (and the content need not be walked,
        // which for every block child would take time growing with the square of the tree).
        if !self.marker_ahead(out) {
            return None;
        }
        if !self.makes_lines(child) && self.rest_makes_lines(element, child) {
            return self.marker_here(out);
        }
        if let Some(marker) = self.marker_here(out) {
            *self.pending_marker.borrow_mut() = Some(marker);
        }
        None
    }

    /// Whether [`Self::marker_here`] finds a marker, without taking it.
    fn marker_ahead(&self, out: &[Collected]) -> bool {
        let run = out
            .iter()
            .rposition(|item| !matches!(item, Collected::Inline(_)))
            .map_or(0, |last| last + 1);
        let mut marker = false;
        for item in &out[run..] {
            match item {
                Collected::Inline(InlineItem::Marker { inside: false, .. }) => marker = true,
                Collected::Inline(InlineItem::Text { text, .. }) if text.trim().is_empty() => {}
                Collected::Inline(InlineItem::OutOfFlow(_) | InlineItem::Float(_)) => {}
                _ => return false,
            }
        }
        marker || self.pending_marker.borrow().is_some()
    }

    /// The outside marker that goes before content added to `out` next, taken out of `out` or
    /// out of the marker that went down to this block: if nothing but white space, floats and
    /// out-of-flow boxes came since the last block. An inside marker is inline content of its
    /// own: before a block, it is on a line of its own, as in Chrome.
    fn marker_here(&self, out: &mut Vec<Collected>) -> Option<InlineItem> {
        let run = out
            .iter()
            .rposition(|item| !matches!(item, Collected::Inline(_)))
            .map_or(0, |last| last + 1);
        let only_marker = out[run..].iter().all(|item| match item {
            Collected::Inline(InlineItem::Text { text, .. }) => text.trim().is_empty(),
            Collected::Inline(
                InlineItem::Marker { inside: false, .. } | InlineItem::OutOfFlow(_) | InlineItem::Float(_),
            ) => true,
            _ => false,
        });
        if !only_marker {
            return None;
        }
        match out[run..]
            .iter()
            .position(|item| matches!(item, Collected::Inline(InlineItem::Marker { inside: false, .. })))
        {
            Some(index) => match out.remove(run + index) {
                Collected::Inline(marker) => Some(marker),
                _ => None,
            },
            None => self.pending_marker.borrow_mut().take(),
        }
    }

    /// Whether the content of `element` after its child `child` makes a line box.
    fn rest_makes_lines(&self, element: NodeId, child: NodeId) -> bool {
        self.document
            .children(element)
            .skip_while(|node| *node != child)
            .skip(1)
            .any(|node| self.node_makes_lines(node))
            || self.styled.pseudo(element, PseudoKey::After).is_some()
    }

    /// Whether a node makes a line box where it is laid out (approximately, from the document):
    /// text other than white space, an inline-level replaced element or atomic inline, a line
    /// break, a list item (with its marker), generated content, or an element with such
    /// content in flow. Out-of-flow boxes and floats make none.
    fn node_makes_lines(&self, node: NodeId) -> bool {
        match self.document.text(node) {
            Some(text) => !text.trim().is_empty(),
            None => self.makes_lines(node),
        }
    }

    fn makes_lines(&self, element: NodeId) -> bool {
        let (Some(data), Some(style)) = (self.document.element(element), self.styled.style(element)) else {
            return false;
        };
        if style.display == Display::None || style.is_out_of_flow() || style.is_floating() {
            return false;
        }
        if self.is_replaced(element, data) || forms::Control::of(data).is_some() {
            return !style.display.is_block_level() || tables::widget_inline(style);
        }
        let atomic = matches!(
            style.display,
            Display::Normal {
                outside: DisplayOutside::Inline,
                inside,
                ..
            } if inside != DisplayInside::Flow
        );
        data.is("br")
            || atomic
            || matches!(style.display, Display::Normal { list_item: true, .. })
            || self.styled.pseudo(element, PseudoKey::Before).is_some()
            || self.styled.pseudo(element, PseudoKey::After).is_some()
            || self
                .document
                .children(element)
                .any(|node| self.node_makes_lines(node))
    }

    /// The box of a replaced element. It is atomic, so its style is blockified like that of an
    /// `inline-block`.
    fn replaced(&self, tree: &mut BoxTree, element: NodeId, style: &Arc<ComputedStyle>) -> BoxId {
        let data = self.document.element(element);
        let content = self.content_image(element);
        // An inline `<svg>` is drawn as an SVG image; its `width` and `height` are hints, as
        // those of an image are (with its ratio).
        let svg = content.is_none() && data.is_some_and(|data| data.is_svg("svg"));
        let mut fitted = None;
        let image = if let Some(image) = content {
            Some(Image::zoomed(&Image::styled(image, style), style.zoom))
        } else if svg {
            self.images
                .inline(element)
                .map(|image| Image::zoomed(image, style.zoom))
        } else if let Some(data) = data.filter(|data| data.is("canvas")) {
            let canvas = blank_canvas(data);
            // Its `width` and `height` give it `aspect-ratio: auto <width> / <height>` (HTML,
            // §15.4.3), which size containment, taking its natural ratio, leaves.
            if style.size_containment() != (false, false)
                && style.aspect_ratio.is_none()
                && data.attribute("width").is_some()
                && data.attribute("height").is_some()
            {
                fitted = canvas.ratio.map(|ratio| {
                    Arc::new(ComputedStyle::from(ComputedValues {
                        aspect_ratio: Some(ratio),
                        aspect_ratio_auto: true,
                        ..ComputedValues::clone(style)
                    }))
                });
            }
            Some(Image::zoomed(&Arc::new(canvas), style.zoom))
        } else if let Some(image) = self.images.frame(element) {
            // A frame that shows a document (see `crate::frame`) fills its content box with it:
            // `object-fit` and `object-position` do not apply.
            fitted = Some(Arc::new(ComputedStyle::from(ComputedValues {
                object_fit: fagus_style::values::ObjectFit::Fill,
                object_position: ComputedStyle::default().object_position.clone(),
                ..ComputedValues::clone(style)
            })));
            Some(Image::zoomed(image, style.zoom))
        } else if data.is_some_and(|data| data.is("iframe")) {
            Some(Image::zoomed(&Arc::new(frame(None)), style.zoom))
        } else if let Some(data) = data.filter(|data| data.is("video")) {
            // A video shows its poster, which no playback replaces in print; without one, it
            // is a frame of 300 × 150 (HTML §4.8.9.15), black all over where it has a video to
            // play (whose frames Fagus does not decode) or controls, as Chrome prints one that
            // does not load, and empty otherwise.
            match data
                .attribute("poster")
                .and_then(|poster| self.images.get(poster.trim()))
            {
                Some(poster) => Some(Image::zoomed(&Image::styled(poster, style), style.zoom)),
                None if data.attribute("src").is_some()
                    || data.attribute("controls").is_some()
                    || self.document.child_elements(element).any(|child| {
                        self.document
                            .element(child)
                            .is_some_and(|child| child.is("source"))
                    }) =>
                {
                    fitted = Some(Arc::new(ComputedStyle::from(ComputedValues {
                        object_fit: fagus_style::values::ObjectFit::Fill,
                        ..ComputedValues::clone(style)
                    })));
                    Some(Image::zoomed(&Arc::new(frame(Some("black"))), style.zoom))
                }
                None => Some(Image::zoomed(&Arc::new(frame(None)), style.zoom)),
            }
        } else if let Some(image) = data.and_then(|data| self.object_image(data)) {
            // An SVG document in an `<object>` or `<embed>` fills its content box (its root's
            // viewport): `object-fit` and `object-position` do not apply, as in Chrome.
            if image.format == crate::image::ImageFormat::Svg {
                fitted = Some(Arc::new(ComputedStyle::from(ComputedValues {
                    object_fit: fagus_style::values::ObjectFit::Fill,
                    object_position: ComputedStyle::default().object_position.clone(),
                    ..ComputedValues::clone(style)
                })));
            }
            Some(Image::zoomed(&Image::styled(image, style), style.zoom))
        } else {
            // The image `src` or `srcset` chooses, at its density.
            data.and_then(|data| self.images.source(element, data))
                .and_then(|(src, density)| Some(Image::at_resolution(self.images.get(src)?, density)))
                .map(|image| Image::zoomed(&Image::styled(&image, style), style.zoom))
        };
        let formula = data
            .filter(|data| crate::math::is_math(data))
            .and_then(|_| crate::math::build(self.document, self.styled, element));
        let style = fitted.as_ref().unwrap_or(style);
        let id = tree.push(LayoutBox {
            style: Arc::new(ComputedStyle::from(ComputedValues {
                display: style.display.blockify(),
                ..ComputedValues::clone(style)
            })),
            node: Some(element),
            generated: false,
            kind: BoxKind::Replaced {
                image,
                attributes: (None, None),
            },
        });
        // A formula is laid out from its MathML, a block one centered in the box.
        if let Some(formula) = formula {
            let block = style.display.is_block_level();
            tree.formulas
                .insert(id, Arc::new(crate::math::MathBox::new(formula, block)));
        }
        // The HTML in its `<foreignObject>` elements is clipped to it, as Chrome clips an
        // inline `<svg>` (`overflow: hidden`).
        if svg && self.foreign_objects(tree, element, id) {
            let layout_box = tree.box_mut(id);
            layout_box.style = Arc::new(ComputedStyle::from(ComputedValues {
                overflow_x: fagus_style::values::Overflow::Hidden,
                overflow_y: fagus_style::values::Overflow::Hidden,
                ..ComputedValues::clone(&layout_box.style)
            }));
        }
        id
    }

    /// A footnote (`float: footnote`, CSS Generated Content for Paged Media 3, §2): its box goes
    /// into the footnote area of a page, and its call (`::footnote-call`) stays in its place, in
    /// `out`, with the box's placeholder inside, which tells the page that the footnote is called
    /// there and keeps the two on one line.
    fn footnote(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        style: &Arc<ComputedStyle>,
        out: &mut Vec<Collected>,
    ) {
        let ordinal = self.next_footnote.get();
        self.next_footnote.set(ordinal.saturating_add(1));
        let mut call = Vec::new();
        let outer = self.footnote.replace(Some(ordinal));
        self.pseudo(tree, element, PseudoKey::FootnoteCall, false, &mut call);
        self.footnote.set(outer);
        let outer = self.footnote_element.replace(Some((element, ordinal)));
        let id = self.block_level(tree, element, Arc::clone(style));
        self.footnote_element.set(outer);
        tree.footnotes.insert(id, ordinal);
        let placeholder = Collected::Inline(InlineItem::OutOfFlow(id));
        match call
            .iter()
            .position(|item| matches!(item, Collected::Inline(InlineItem::Open { .. })))
        {
            Some(open) => {
                // The call's box knows its footnote, which it links to.
                if let Collected::Inline(InlineItem::Open { style, .. }) = &mut call[open] {
                    let mut own = ComputedStyle::clone(style);
                    own.footnote_call = Some(element);
                    *style = Arc::new(own);
                }
                call.insert(open + 1, placeholder);
            }
            None => call.insert(0, placeholder),
        }
        out.extend(call);
    }

    /// Adds the box or inline items of the element's `::before` or `::after` to `out`. In a flex
    /// or grid container (`item`), the pseudo-element is an item like the element's children.
    fn pseudo(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        key: PseudoKey,
        item: bool,
        out: &mut Vec<Collected>,
    ) {
        let Some(style) = self.styled.pseudo(element, key) else {
            return;
        };
        self.pseudo_box(tree, element, key, style, item, out);
    }

    /// The box of pseudo-element `key` of `element` with `style`, into `out` (see
    /// [`Builder::pseudo`]).
    fn pseudo_box(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        key: PseudoKey,
        style: &Arc<ComputedStyle>,
        item: bool,
        out: &mut Vec<Collected>,
    ) {
        // `display: none` generates no box, and its counters do not change (CSS Display 3, §2.5).
        if style.display == Display::None {
            return;
        }
        let Content::Items(items) = &style.content else {
            return;
        };
        // The pseudo-element is a child of the element for counters.
        let depth = self.depth(element) + 1;
        self.apply_counters(style, depth);
        // A pseudo-element that is a list item counts `list-item` (unless it sets it itself) and
        // has a marker before its content.
        let mut marker = Vec::new();
        if matches!(style.display, Display::Normal { list_item: true, .. }) {
            let own_list_item = [&style.counter_reset, &style.counter_increment, &style.counter_set]
                .iter()
                .any(|directives| directives.iter().any(|(name, _)| name == "list-item"));
            if !own_list_item {
                self.counters.borrow_mut().increment_list_item(depth);
            }
            let marker_key = if key == PseudoKey::Before {
                PseudoKey::BeforeMarker
            } else {
                PseudoKey::AfterMarker
            };
            self.marker_of(element, style, marker_key, &mut marker);
        }
        let mut content = self.content_items(tree, element, items, style);
        self.counters.borrow_mut().leave(depth);
        if !marker.is_empty() {
            content.splice(
                0..0,
                marker.into_iter().filter_map(|item| match item {
                    Collected::Inline(item) => Some(item),
                    Collected::Block(_) | Collected::Part(_) => None,
                }),
            );
        }
        let generated = |tree: &mut BoxTree, style: Arc<ComputedStyle>, content: Vec<InlineItem>| {
            let items = process_white_space(content);
            // A flex or grid container wraps its text in an anonymous item, which the container
            // aligns (CSS Flexbox 1, §4; CSS Grid 2, §6).
            let flex = match style.display {
                Display::Normal {
                    inside: DisplayInside::Flex,
                    ..
                } => true,
                Display::Normal {
                    inside: DisplayInside::Grid,
                    ..
                } => false,
                _ => {
                    return tree.push(LayoutBox {
                        style,
                        node: None,
                        generated: true,
                        kind: BoxKind::Inline { items },
                    });
                }
            };
            let children = if items.is_empty() {
                Vec::new()
            } else {
                vec![tree.push(LayoutBox {
                    style: Arc::new(anonymous_style(&style)),
                    node: None,
                    generated: false,
                    kind: BoxKind::Inline { items },
                })]
            };
            let kind = if flex {
                BoxKind::Flex { children }
            } else {
                BoxKind::Grid { children }
            };
            tree.push(LayoutBox {
                style,
                node: None,
                generated: true,
                kind,
            })
        };
        // `display: contents` generates no box: the content goes in its place, without the
        // pseudo-element's borders and backgrounds (CSS Display 3, §2.5).
        if style.display == Display::Contents {
            out.extend(content.into_iter().map(Collected::Inline));
            return;
        }
        if style.is_out_of_flow() {
            let id = generated(tree, Arc::clone(style), content);
            if style.position == Position::Fixed {
                tree.fixed.push(id);
            }
            out.push(Collected::Inline(InlineItem::OutOfFlow(id)));
        } else if item {
            // Items are blockified and do not float (CSS Flexbox 1, §4).
            let item_style = Arc::new(ComputedStyle::from(ComputedValues {
                display: style.display.blockify(),
                float: fagus_style::values::Float::None,
                ..ComputedValues::clone(style)
            }));
            out.push(Collected::Block(generated(tree, item_style, content)));
        } else if let Display::Internal(display) = style.display {
            let part = self.pseudo_part(tree, style, display, content);
            let parent = self.styled.style(element).unwrap_or(style);
            self.add_to_parent(tree, parent, out, part);
        } else if style.is_floating() {
            let id = generated(tree, Arc::clone(style), content);
            out.push(Collected::Inline(InlineItem::Float(id)));
        } else if style.display.is_block_level() {
            out.push(Collected::Block(generated(tree, Arc::clone(style), content)));
        } else if let Display::Normal {
            outside: DisplayOutside::Inline,
            inside,
            ..
        } = style.display
            && inside != DisplayInside::Flow
        {
            // An inline-block (or another atomic inline): a block container placed like a glyph.
            let block_style = Arc::new(ComputedStyle::from(ComputedValues {
                display: style.display.blockify(),
                ..ComputedValues::clone(style)
            }));
            out.push(Collected::Inline(InlineItem::Atomic(generated(
                tree,
                block_style,
                content,
            ))));
        } else {
            out.push(Collected::Inline(InlineItem::Open {
                style: Arc::clone(style),
                node: None,
            }));
            out.extend(content.into_iter().map(Collected::Inline));
            out.push(Collected::Inline(InlineItem::Close));
        }
    }

    /// Generated content as inline items: text, with the page number and count apart, since
    /// they are known only when the box is laid out on a page.
    fn content_items(
        &self,
        tree: &mut BoxTree,
        element: NodeId,
        items: &[ContentItem],
        style: &Arc<ComputedStyle>,
    ) -> Vec<InlineItem> {
        let mut result = Vec::new();
        let mut text = String::new();
        for item in items {
            match item {
                // An image is an anonymous replaced element that inherits from the
                // pseudo-element, placed in its line like an `<img>`.
                ContentItem::Image(url) => {
                    let Some(image) = self.images.get(url.trim()) else {
                        continue;
                    };
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    let replaced_style = Arc::new(anonymous_style(style));
                    let image = Image::zoomed(&Image::styled(image, &replaced_style), replaced_style.zoom);
                    let id = tree.push(LayoutBox {
                        style: replaced_style,
                        node: None,
                        generated: true,
                        kind: BoxKind::Replaced {
                            image: Some(image),
                            attributes: (None, None),
                        },
                    });
                    result.push(InlineItem::Atomic(id));
                }
                ContentItem::Counter { name, style: format }
                    if let Some(counter) = PageCounter::from_name(name) =>
                {
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    result.push(InlineItem::PageNumber {
                        counter,
                        format: format.clone(),
                        style: Arc::clone(style),
                    });
                }
                // The number of a footnote in its call or marker, which `@page` rules may count
                // from each page.
                ContentItem::Counter { name, style: format }
                    if name == "footnote"
                        && let Some(ordinal) = self.footnote.get() =>
                {
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    result.push(InlineItem::PageNumber {
                        counter: PageCounter::Footnote {
                            value: self.counters.borrow().value("footnote"),
                            ordinal,
                        },
                        format: format.clone(),
                        style: Arc::clone(style),
                    });
                }
                ContentItem::TargetCounter {
                    target,
                    name,
                    style: format,
                } if name == "page" => {
                    self.asked.set(true);
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    if let Some(id) = self.target(element, target) {
                        result.push(InlineItem::TargetPage {
                            id: Arc::from(id),
                            format: format.clone(),
                            style: Arc::clone(style),
                        });
                    }
                }
                ContentItem::Leader(fill) => {
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    result.push(InlineItem::Leader {
                        fill: fill.clone(),
                        style: Arc::clone(style),
                    });
                }
                ContentItem::NamedString { name, .. } => {
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    result.push(InlineItem::NamedString {
                        name: Arc::from(name.as_str()),
                        position: self.string_log.borrow().len(),
                        style: Arc::clone(style),
                    });
                }
                item => {
                    // Each part of the content is an item of its own, as Blink makes a text
                    // object of each (`LayoutTextFragment`, `LayoutCounter`, `LayoutQuote`),
                    // whose width it rounds up to layout units on its own: a style of its own
                    // keeps the line from taking it for the same item (`same_item`).
                    let part = self.content_text(element, std::slice::from_ref(item));
                    // Text combined upright is combined whole.
                    if style.text_combine_upright {
                        text.push_str(&part);
                        continue;
                    }
                    if part.is_empty() {
                        continue;
                    }
                    if !text.is_empty() {
                        result.push(InlineItem::Text {
                            text: std::mem::take(&mut text),
                            style: Arc::clone(style),
                        });
                    }
                    let own = result
                        .iter()
                        .any(|item| matches!(item, InlineItem::Text { style: other, .. } if Arc::ptr_eq(other, style)));
                    if own {
                        result.push(InlineItem::Text {
                            text: part,
                            style: Arc::new((**style).clone()),
                        });
                    } else {
                        text = part;
                    }
                }
            }
        }
        if !text.is_empty() || result.is_empty() {
            result.push(InlineItem::Text {
                text,
                style: Arc::clone(style),
            });
        }
        result
    }

    /// [`Self::flush_run`] for a run of text directly in a flex or grid container: a run of
    /// nothing but white space makes no item, even where `white-space` preserves it (CSS Grid
    /// 2, §6; CSS Flexbox 1, §4), as in Chrome.
    fn flush_item_run(
        &self,
        tree: &mut BoxTree,
        parent: &Arc<ComputedStyle>,
        run: &mut Vec<InlineItem>,
        children: &mut Vec<BoxId>,
    ) {
        let white_space = |text: &str| {
            text.chars()
                .all(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}'))
        };
        if run
            .iter()
            .all(|item| matches!(item, InlineItem::Text { text, .. } if white_space(text)))
        {
            run.clear();
            return;
        }
        self.flush_run(tree, parent, run, children);
    }

    /// Children of a flex or grid container: every child element becomes a blockified item;
    /// runs of text directly in the container become anonymous items (CSS Flexbox 1, §4).
    fn item_children(&self, tree: &mut BoxTree, element: NodeId, style: &Arc<ComputedStyle>) -> Vec<BoxId> {
        let mut children = Vec::new();
        // `content-visibility: hidden` skips the contents of the box.
        if style.content_hidden {
            return children;
        }
        let mut run = Vec::new();
        let mut before = Vec::new();
        self.pseudo(tree, element, PseudoKey::Before, true, &mut before);
        for item in before {
            match item {
                Collected::Inline(InlineItem::OutOfFlow(child) | InlineItem::Float(child))
                | Collected::Block(child) => {
                    children.push(child);
                }
                Collected::Inline(item) => run.push(item),
                Collected::Part(_) => {}
            }
        }
        // A closed `<details>` shows only its summary, also as a flex or grid container (as
        // Water.css styles it).
        let details = self.details(element);
        // The element before, whose scroll buttons and scroll marker group after it are still to
        // come (CSS Overflow 5).
        let mut scrolled: Option<NodeId> = None;
        for child in self.document.children(element) {
            if let Some(before) = scrolled.take() {
                let mut boxes = Vec::new();
                self.scroll_boxes_after(tree, before, true, &mut boxes);
                self.push_items(tree, style, boxes, &mut run, &mut children);
            }
            if details.is_some_and(|details| !details.shows(child)) {
                continue;
            }
            if let Some(text) = self.document.text(child) {
                run.push(InlineItem::Text {
                    text: text.to_owned(),
                    style: Arc::clone(style),
                });
                continue;
            }
            let Some(child_style) = self.styled.style(child) else {
                continue;
            };
            if child_style.display != Display::None {
                let mut boxes = Vec::new();
                self.scroll_boxes_before(tree, child, child_style, true, &mut boxes);
                self.push_items(tree, style, boxes, &mut run, &mut children);
                scrolled = Some(child);
            }
            // A `-webkit-box` does not blockify its children (Blink's `BlockifiesChildren`):
            // inline-level ones join the run of text around them, in one anonymous item.
            if style.legacy_box.is_box
                && matches!(
                    child_style.display,
                    Display::Normal {
                        outside: DisplayOutside::Inline,
                        ..
                    }
                )
                && !child_style.is_out_of_flow()
                && !child_style.is_floating()
            {
                let mut collected = Vec::new();
                self.collect_children_with(tree, element, style, &mut collected, Some(child));
                for item in collected {
                    match item {
                        Collected::Inline(item) => run.push(item),
                        Collected::Block(id) => {
                            self.flush_item_run(tree, style, &mut run, &mut children);
                            children.push(id);
                        }
                        Collected::Part(_) => {}
                    }
                }
                continue;
            }
            if child_style.display == Display::None {
                self.defer_strings(child, child_style);
            } else {
                self.enter(child, child_style);
            }
            let line_break = self.document.element(child).is_some_and(|data| data.is("br"));
            match child_style.display {
                Display::None => {}
                Display::Contents => {
                    self.flush_item_run(tree, style, &mut run, &mut children);
                    children.extend(self.item_children(tree, child, child_style));
                }
                // A line break belongs to the run of text around it, as in Blink, where it is
                // text: `A<br>B` is one item of two lines.
                _ if line_break && !child_style.is_out_of_flow() => {
                    run.push(InlineItem::LineBreak {
                        style: Arc::clone(child_style),
                    });
                }
                _ if child_style.is_out_of_flow() && !child_style.footnote => {
                    self.flush_item_run(tree, style, &mut run, &mut children);
                    let id = self.block_level(tree, child, Arc::clone(child_style));
                    if child_style.position == Position::Fixed {
                        tree.fixed.push(id);
                    }
                    children.push(id);
                }
                _ => {
                    self.flush_item_run(tree, style, &mut run, &mut children);
                    // `float` does not apply to flex and grid items.
                    let item_style = if child_style.display.is_block_level()
                        && !matches!(child_style.display, Display::Internal(_))
                        && !child_style.is_floating()
                        && !child_style.footnote
                    {
                        Arc::clone(child_style)
                    } else {
                        Arc::new(ComputedStyle::from(ComputedValues {
                            display: child_style.display.blockify(),
                            float: fagus_style::values::Float::None,
                            footnote: false,
                            ..ComputedValues::clone(child_style)
                        }))
                    };
                    children.push(self.block_level(tree, child, item_style));
                }
            }
        }
        if let Some(before) = scrolled {
            let mut boxes = Vec::new();
            self.scroll_boxes_after(tree, before, true, &mut boxes);
            self.push_items(tree, style, boxes, &mut run, &mut children);
        }
        let mut after = Vec::new();
        self.pseudo(tree, element, PseudoKey::After, true, &mut after);
        self.push_items(tree, style, after, &mut run, &mut children);
        self.flush_item_run(tree, style, &mut run, &mut children);
        self.counters.borrow_mut().leave(self.depth(element));
        children
    }

    /// Adds collected boxes to the items of a flex or grid container with `style`: boxes as
    /// items of their own, inline content to the run of text that makes an anonymous item.
    fn push_items(
        &self,
        tree: &mut BoxTree,
        style: &Arc<ComputedStyle>,
        collected: Vec<Collected>,
        run: &mut Vec<InlineItem>,
        children: &mut Vec<BoxId>,
    ) {
        for item in collected {
            match item {
                Collected::Inline(InlineItem::OutOfFlow(child) | InlineItem::Float(child))
                | Collected::Block(child) => {
                    self.flush_item_run(tree, style, run, children);
                    children.push(child);
                }
                Collected::Inline(item) => run.push(item),
                Collected::Part(_) => {}
            }
        }
    }

    /// A positive span attribute (`colspan`, `rowspan`, `span`), with HTML's limits.
    fn span_attribute(&self, element: NodeId, name: &str, default: usize, max: usize) -> usize {
        self.document
            .element(element)
            .and_then(|data| data.attribute(name))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .map_or(default, |value| value.min(max))
    }
}

/// The break at one break point from the values of a box and of a child passed on to it: a
/// forced break wins (a page break of a side over a plain one), then one that avoids a break.
pub(crate) fn combine_breaks(outer: BreakBetween, inner: BreakBetween) -> BreakBetween {
    let forced = |value: BreakBetween| value.is_forced_page_break() || value == BreakBetween::Column;
    let avoids = |value: BreakBetween| {
        matches!(
            value,
            BreakBetween::Avoid | BreakBetween::AvoidPage | BreakBetween::AvoidColumn
        )
    };
    match (outer, inner) {
        (BreakBetween::Page, inner) if inner.is_forced_page_break() => inner,
        (outer, _) if forced(outer) => outer,
        (_, inner) if forced(inner) => inner,
        (outer, _) if avoids(outer) => outer,
        (_, inner) => inner,
    }
}

/// Whether an inline box has a margin, border or padding at its start (`start`) or its end
/// (left or right, top or bottom in vertical writing modes, by its direction) that is not zero;
/// percentages count as not zero whatever they resolve against.
fn has_inline_edge(style: &ComputedStyle, start: bool) -> bool {
    let (margin, border, padding) = (&style.margin, &style.border_width, &style.padding);
    let sides = if style.is_vertical() {
        [
            (&margin.top, border.top, &padding.top),
            (&margin.bottom, border.bottom, &padding.bottom),
        ]
    } else {
        [
            (&margin.left, border.left, &padding.left),
            (&margin.right, border.right, &padding.right),
        ]
    };
    let [first, last] = sides;
    let (margin, border, padding) = if start == style.is_rtl() { last } else { first };
    margin.resolve_or(100.0, 0.0) != 0.0 || border != 0.0 || padding.resolve(100.0) != 0.0
}

/// Block `child` of a block container in `parent`'s style, inside the inline boxes `open`: where
/// any of them is relatively positioned, the block moves with it (CSS 2.1 §9.2.1.1), in a block
/// for each, as relatively positioned as the inline box. The block has no element, but has
/// fragments of its own (it is `generated`), which the offsets move.
fn relative_in_inlines(
    tree: &mut BoxTree,
    parent: &ComputedStyle,
    open: &[&Arc<ComputedStyle>],
    child: BoxId,
) -> BoxId {
    open.iter()
        .rev()
        .filter(|inline| inline.position == Position::Relative)
        .fold(child, |child, inline| {
            tree.push(LayoutBox {
                style: Arc::new(ComputedStyle::from(ComputedValues {
                    position: Position::Relative,
                    inset: inline.inset.clone(),
                    ..ComputedValues::from(anonymous_style(parent))
                })),
                node: None,
                generated: true,
                kind: BoxKind::Block {
                    children: vec![child],
                },
            })
        })
}

/// The innermost of the boxes that [`relative_in_inlines`] put around block `outer` for the
/// relatively positioned inline boxes among `open` (outermost first), if `outer` is the
/// outermost of them.
fn relative_wrapper(tree: &BoxTree, outer: BoxId, open: &[Arc<ComputedStyle>]) -> Option<BoxId> {
    let mut found = None;
    let mut next = Some(outer);
    for inline in open.iter().filter(|inline| inline.position == Position::Relative) {
        let id = next?;
        let layout_box = tree.get(id);
        let matches = layout_box.generated
            && layout_box.node.is_none()
            && layout_box.style.position == Position::Relative
            && layout_box.style.inset == inline.inset;
        let BoxKind::Block { children } = &layout_box.kind else {
            return None;
        };
        if !matches {
            return None;
        }
        found = Some(id);
        next = children.first().copied();
    }
    found
}

/// The style of the part of an inline box after a block inside it: without the margin, border
/// and padding at its start, which the part before has (unless `box-decoration-break: clone`
/// gives each part its own), as Chrome draws it.
fn continued_inline(style: &Arc<ComputedStyle>) -> Arc<ComputedStyle> {
    if style.clones_decorations() {
        return Arc::clone(style);
    }
    let mut continued = ComputedStyle::clone(style);
    let start_is_end = style.is_rtl();
    if style.is_vertical() {
        if start_is_end {
            continued.margin.bottom = fagus_style::values::LengthPercentageAuto::ZERO;
            continued.border_width.bottom = 0.0;
            continued.padding.bottom = LengthPercentage::ZERO;
        } else {
            continued.margin.top = fagus_style::values::LengthPercentageAuto::ZERO;
            continued.border_width.top = 0.0;
            continued.padding.top = LengthPercentage::ZERO;
        }
    } else if start_is_end {
        continued.margin.right = fagus_style::values::LengthPercentageAuto::ZERO;
        continued.border_width.right = 0.0;
        continued.padding.right = LengthPercentage::ZERO;
    } else {
        continued.margin.left = fagus_style::values::LengthPercentageAuto::ZERO;
        continued.border_width.left = 0.0;
        continued.padding.left = LengthPercentage::ZERO;
    }
    Arc::new(continued)
}

/// The style of an anonymous box: inherited properties from the parent, everything else initial.
pub(crate) fn anonymous_style(parent: &ComputedStyle) -> ComputedStyle {
    ComputedStyle::from(ComputedValues {
        display: Display::BLOCK,
        ..ComputedValues::from(parent.inherit())
    })
}

/// White space processing (CSS Text 3, §4.1) and `text-transform`, across inline items.
pub(crate) fn process_white_space(items: Vec<InlineItem>) -> Vec<InlineItem> {
    process_white_space_noting(items, &mut rustc_hash::FxHashSet::default())
}

/// [`process_white_space`], noting in `after_collapsed` the out-of-flow boxes that come right
/// after a text item it removes whole (see [`BoxTree::after_collapsed`]).
pub(crate) fn process_white_space_noting(
    items: Vec<InlineItem>,
    after_collapsed: &mut rustc_hash::FxHashSet<BoxId>,
) -> Vec<InlineItem> {
    let mut result = Vec::with_capacity(items.len());
    // Whether the item before was text that collapsed to nothing.
    let mut collapsed = false;
    // Whether the last character emitted is a collapsible space, or nothing was emitted yet.
    let mut after_space = true;
    // The text before, for the words that `text-transform: capitalize` finds.
    let mut before = crate::text_case::Before::default();
    // The last character of the text before, and a text item whose collapsible space at its end
    // stands for spaces with a segment break (Blink's `IsEndCollapsibleNewline`).
    let mut last_character: Option<char> = None;
    let mut newline_space: Option<usize> = None;
    for item in items {
        let was_collapsed = std::mem::take(&mut collapsed);
        match item {
            InlineItem::OutOfFlow(id) if was_collapsed => {
                after_collapsed.insert(id);
                result.push(item);
            }
            InlineItem::LineBreak { style } => {
                trim_trailing_space(&mut result);
                result.push(InlineItem::LineBreak { style });
                after_space = true;
                before = crate::text_case::Before::default();
                (last_character, newline_space) = (Some('\n'), None);
            }
            InlineItem::OutOfFlow(_)
            | InlineItem::Float(_)
            | InlineItem::Open { .. }
            | InlineItem::Close
            | InlineItem::Marker { .. } => {
                result.push(item);
            }
            InlineItem::Atomic(_)
            | InlineItem::NamedString { .. }
            | InlineItem::PageNumber { .. }
            | InlineItem::TargetPage { .. }
            | InlineItem::Leader { .. } => {
                result.push(item);
                after_space = false;
                before = crate::text_case::Before::default();
                (last_character, newline_space) = (Some('\u{FFFC}'), None);
            }
            InlineItem::Text { text, style } => {
                let white_space = style.white_space;
                let mut processed = String::with_capacity(text.len());
                let mut pending_breaks = Vec::new();
                // A collapsible space that ended the text before, from spaces with a segment
                // break, goes where this text starts with a zero width space.
                if let Some(index) = newline_space.take()
                    && text.starts_with('\u{200B}')
                    && let Some(InlineItem::Text { text: earlier, .. }) = result.get_mut(index)
                    && earlier.ends_with(' ')
                {
                    earlier.pop();
                }
                if white_space.collapses_spaces() {
                    let preserves_newlines = white_space.preserves_newlines();
                    // A form feed is no white space in CSS: where spaces collapse, Chrome
                    // draws it.
                    let collapsible = |character: char| {
                        matches!(character, ' ' | '\t' | '\r') || (character == '\n' && !preserves_newlines)
                    };
                    let mut characters = text.chars().peekable();
                    while let Some(character) = characters.next() {
                        if character == '\n' && preserves_newlines {
                            while processed.ends_with(' ') {
                                processed.pop();
                            }
                            pending_breaks.push(processed.len());
                            after_space = true;
                        } else if collapsible(character) {
                            // The run of spaces, and whether it has a segment break.
                            let mut segment_break = character == '\n';
                            while let Some(&next) = characters.peek()
                                && collapsible(next)
                            {
                                segment_break |= next == '\n';
                                characters.next();
                            }
                            // Segment break transformation (CSS Text 3, §4.1.2): spaces with a
                            // segment break next to a zero width space go, as in Blink.
                            let zero_width = |character: Option<char>| character == Some('\u{200B}');
                            if segment_break
                                && (zero_width(processed.chars().next_back().or(last_character))
                                    || zero_width(characters.peek().copied()))
                            {
                                continue;
                            }
                            if !after_space {
                                processed.push(' ');
                                after_space = true;
                                if segment_break && characters.peek().is_none() {
                                    newline_space = Some(result.len());
                                }
                            }
                        } else {
                            processed.push(character);
                            after_space = false;
                        }
                    }
                } else {
                    for character in text.chars() {
                        if character == '\n' {
                            pending_breaks.push(processed.len());
                        } else if character != '\r' && character != '\u{c}' {
                            // Preserved tabs stay: the line breaker widens them to tab stops.
                            // Carriage returns and form feeds go, as Blink ignores them where
                            // spaces are preserved (`ShouldIgnore`).
                            processed.push(character);
                        }
                    }
                    // A preserved newline at the end starts a line, where collapsible space
                    // after it goes; after other preserved characters, spaces and tabs too, it
                    // stays, as in Chrome (a preserved space is not collapsible).
                    if text.trim_end_matches('\r').ends_with('\n') {
                        after_space = true;
                    } else if !processed.is_empty() {
                        after_space = false;
                    }
                }
                if let Some(last) = processed.chars().next_back() {
                    last_character = Some(last);
                }
                let transformed = (style.text_transform != TextTransform::None)
                    .then(|| transform(&processed, style.text_transform, style.lang.as_deref(), before));
                before.push_str(&processed);
                let processed = transformed.unwrap_or(processed);
                // Text of nothing but spaces that collapse goes (an empty text item in Blink).
                collapsed = processed.is_empty() && !text.is_empty();
                // Split at preserved newlines into text and forced breaks.
                let mut start = 0;
                for at in pending_breaks {
                    let at = at.min(processed.len());
                    if at > start {
                        result.push(InlineItem::Text {
                            text: processed[start..at].to_owned(),
                            style: Arc::clone(&style),
                        });
                    }
                    result.push(InlineItem::LineBreak {
                        style: Arc::clone(&style),
                    });
                    start = at;
                }
                if start < processed.len() {
                    result.push(InlineItem::Text {
                        text: processed[start..].to_owned(),
                        style: Arc::clone(&style),
                    });
                }
            }
        }
    }
    trim_trailing_space(&mut result);
    result
}

fn trim_trailing_space(items: &mut Vec<InlineItem>) {
    // Out-of-flow placeholders and inline box edges take no text, so the text before them is
    // trailing too.
    let mut index = items.len();
    while index > 0 {
        index -= 1;
        match &mut items[index] {
            InlineItem::OutOfFlow(_)
            | InlineItem::Float(_)
            | InlineItem::Open { .. }
            | InlineItem::Close
            | InlineItem::Marker { .. } => {}
            InlineItem::Text { text, style } if style.white_space.collapses_spaces() => {
                while text.ends_with(' ') {
                    text.pop();
                }
                if text.is_empty() {
                    items.remove(index);
                } else {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// Text with a `text-transform`, in the case mapping of its language (`lang`); `before` is the
/// text before it in the paragraph, whose last word it may go on.
/// `text` as `text-transform` in `style` shows it, after `preceding`, the text before it in
/// its paragraph (where words that `capitalize` finds may start).
pub fn transformed_text(text: &str, style: &ComputedStyle, preceding: &str) -> String {
    let mut before = crate::text_case::Before::default();
    before.push_str(preceding);
    transform(text, style.text_transform, style.lang.as_deref(), before)
}

pub(crate) fn transform(
    text: &str,
    transform: TextTransform,
    lang: Option<&str>,
    before: crate::text_case::Before,
) -> String {
    use crate::text_case::{CaseLanguage, capitalize, lowercase, uppercase};
    let language = CaseLanguage::of(lang);
    match transform {
        TextTransform::None => text.to_owned(),
        TextTransform::Uppercase => uppercase(text, language),
        TextTransform::Lowercase => lowercase(text, language),
        TextTransform::Capitalize => capitalize(text, before),
        TextTransform::MathAuto => crate::math::italic(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_first_letter_with_its_punctuation() {
        assert_eq!(letter_span("  „Zitat“"), Some((2, 6)));
        assert_eq!(letter_span("x. y"), Some((0, 2)));
        assert_eq!(letter_span("1st"), Some((0, 1)));
        assert_eq!(letter_span("\u{65}\u{301}tude"), Some((0, 3)));
        assert_eq!(letter_span("„ "), None);
        // Dashes and connectors come along before the letter, not opening punctuation and
        // dashes after it; spaces after it only with punctuation after them (WPT
        // `first-letter-trailing-punctuation`).
        assert_eq!(letter_span("—Dash"), Some((0, 4)));
        assert_eq!(letter_span("_T_est"), Some((0, 3)));
        assert_eq!(letter_span("T(rail"), Some((0, 1)));
        assert_eq!(letter_span("T\u{2013}rail"), Some((0, 1)));
        assert_eq!(letter_span("\u{AB}\u{A0}T\u{A0}\u{BB}est"), Some((0, 5)));
        assert_eq!(letter_span("T\u{2003}.est"), Some((0, 5)));
        assert_eq!(letter_span("T.\u{2003}est"), Some((0, 2)));
    }
}
