//! Table layout (CSS 2.1 §17, CSS Tables 3).
//!
//! The rows and cells of a table form a grid of slots, which spanning cells cover several of. A
//! table is laid out once for its width: column widths from the cells' intrinsic widths, every
//! cell at its width, row heights from the cells. Pages then take rows from that layout and
//! repeat the header and footer rows. With `border-collapse: collapse`, neighboring cells share
//! one border, chosen by the rules of CSS 2.1 §17.6.2.1.
//!
//! Rows of the header and footer whose content depends on the page, through `string()` or a
//! page number, are laid out again for every page: `string()` shows the value its named string
//! has where the rows above end, so that a footer row can show the running total of the page and
//! a header row the total carried over. With `-fagus-repeat: at-break`, such a carry-over row
//! shows only where the table breaks.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use fagus_style::values::{
    BorderCollapse, BorderStyle, BoxSizing, BreakBetween, BreakInside, CaptionSide, Display, EmptyCells,
    LengthPercentage, MaxSize, Position, Repeat, Rgba, Size, TableLayout, VerticalAlign, Visibility,
};
use fagus_style::{ComputedStyle, ComputedValues};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::block::{BreakToken, Layouter, Margin, Outcome, Space, first_baseline};
use crate::colspan::SpanColumn;
use crate::fragment::{Children, Fragment, FragmentKind, VisibleSides};
use crate::parallel::{LineBoxes, Piece, Portion, split_line};
use crate::tree::{BoxId, BoxKind, InlineItem};
use crate::units::{Au, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Header,
    Body,
    Footer,
}

/// A cell placed in the grid.
#[derive(Clone, Copy, Debug)]
struct Slot {
    id: BoxId,
    row: usize,
    column: usize,
    colspan: usize,
    rowspan: usize,
}

#[derive(Clone, Copy, Debug)]
struct Row {
    id: BoxId,
    group: BoxId,
    part: Part,
}

/// The slots of a table; rows in the order header, bodies, footer. Columns count from the left:
/// in a right-to-left table the first one is the last (CSS 2.1, §17.5).
#[derive(Debug, Default)]
struct Grid {
    rows: Vec<Row>,
    cells: Vec<Slot>,
    columns: usize,
    rtl: bool,
    /// The rows and cells, and the slots of the rows that cells span (twice, for the lists of
    /// them that grow), charged to the budget of the render while the grid lives.
    held: fagus_dom::budget::Held,
}

impl Grid {
    /// The indexes of the cells that start in rows `rows`. The grid has its cells in the order of
    /// their rows, so they lie together.
    fn cells_in(&self, rows: std::ops::RangeInclusive<usize>) -> Range<usize> {
        let start = self.cells.partition_point(|slot| slot.row < *rows.start());
        let end = self.cells.partition_point(|slot| slot.row <= *rows.end());
        start..end.max(start)
    }

    /// The place from the left of the table's column `index` in the source.
    fn physical(&self, index: usize) -> usize {
        if self.rtl {
            self.columns.saturating_sub(index + 1)
        } else {
            index
        }
    }
}

/// A border in the collapsing border model.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Edge {
    width: f32,
    style: BorderStyle,
    color: Rgba,
    /// Cell 4, row 3, row group 2, column 1, table 0: the closer to the cell, the stronger.
    priority: u8,
    /// The place of the box it comes from in Blink's order: cells in the order of the source,
    /// then rows, row groups, columns and the table; of two borders alike, the earlier is drawn
    /// where they meet.
    order: u32,
}

impl Edge {
    const NONE: Self = Self {
        width: 0.0,
        style: BorderStyle::None,
        color: Rgba::TRANSPARENT,
        priority: 0,
        order: u32::MAX,
    };

    /// Whether this border wins against `other` (CSS 2.1 §17.6.2.1).
    fn beats(&self, other: &Self) -> bool {
        if other.style == BorderStyle::Hidden {
            return false;
        }
        if self.style == BorderStyle::Hidden {
            return true;
        }
        if self.style == BorderStyle::None || self.width <= 0.0 {
            return false;
        }
        if other.style == BorderStyle::None || other.width <= 0.0 {
            return true;
        }
        if (self.width - other.width).abs() > f32::EPSILON {
            return self.width > other.width;
        }
        let rank = |style: BorderStyle| match style {
            BorderStyle::Double => 8,
            BorderStyle::Solid => 7,
            BorderStyle::Dashed => 6,
            BorderStyle::Dotted => 5,
            BorderStyle::Ridge => 4,
            BorderStyle::Outset => 3,
            BorderStyle::Groove => 2,
            BorderStyle::Inset => 1,
            BorderStyle::None | BorderStyle::Hidden => 0,
        };
        if rank(self.style) != rank(other.style) {
            return rank(self.style) > rank(other.style);
        }
        self.priority > other.priority
    }

    /// The style it is drawn in: in the collapsing border model `inset` is drawn as `ridge` and
    /// `outset` as `groove` (CSS 2.1, §17.6.2), as in Blink.
    fn drawn_style(&self) -> BorderStyle {
        match self.style {
            BorderStyle::Inset => BorderStyle::Ridge,
            BorderStyle::Outset => BorderStyle::Groove,
            style => style,
        }
    }

    /// The width it takes in layout: hidden borders take none.
    fn used_width(&self) -> f32 {
        if self.style.is_visible() { self.width } else { 0.0 }
    }
}

/// Sides of a style as edges: top, right, bottom, left.
fn edges(style: &ComputedStyle, priority: u8, index: usize) -> [Edge; 4] {
    let class = match priority {
        4 => 0,
        3 => 1,
        2 => 2,
        1 => 3,
        _ => 4,
    };
    let order = class * 1_000_000 + u32::try_from(index).unwrap_or(999_999).min(999_999);
    let edge = |width: f32, border: BorderStyle, color: Option<Rgba>| Edge {
        width,
        style: border,
        color: style.border_color(color),
        priority,
        order,
    };
    [
        edge(
            style.border_width.top,
            style.border_style.top,
            style.border_color.top,
        ),
        edge(
            style.border_width.right,
            style.border_style.right,
            style.border_color.right,
        ),
        edge(
            style.border_width.bottom,
            style.border_style.bottom,
            style.border_color.bottom,
        ),
        edge(
            style.border_width.left,
            style.border_style.left,
            style.border_color.left,
        ),
    ]
}

/// The collapsed borders of a table: horizontal ones per row line and column, vertical ones per
/// row and column line.
struct Collapsed {
    horizontal: Vec<Vec<Edge>>,
    vertical: Vec<Vec<Edge>>,
}

impl Collapsed {
    fn consider(slot: &mut Edge, edge: Edge) {
        if edge.beats(slot) {
            *slot = edge;
        }
    }

    /// The widest of the edges along one side of a cell, and its style and color.
    fn side(edges: impl Iterator<Item = Edge>) -> Edge {
        edges.fold(Edge::NONE, |widest, edge| {
            if edge.used_width() > widest.used_width() {
                edge
            } else {
                widest
            }
        })
    }

    /// Top, right, bottom and left of a cell.
    fn cell(&self, slot: &Slot) -> [Edge; 4] {
        let columns = slot.column..slot.column + slot.colspan;
        let rows = slot.row..slot.row + slot.rowspan;
        [
            Self::side(self.horizontal[slot.row][columns.clone()].iter().copied()),
            Self::side(
                rows.clone()
                    .map(|row| self.vertical[row][slot.column + slot.colspan]),
            ),
            Self::side(self.horizontal[slot.row + slot.rowspan][columns].iter().copied()),
            Self::side(rows.map(|row| self.vertical[row][slot.column])),
        ]
    }
}

/// How a table's header or footer repeats where the table breaks, as decided where the table
/// starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeating {
    /// All its rows repeat (those with `-fagus-repeat: at-break` only where the table breaks).
    Whole,
    /// It is too tall to repeat, or does not avoid breaks inside: its rows come once, before or
    /// after the others, but its rows with `-fagus-repeat: at-break` repeat where the table
    /// breaks on their own, being short enough.
    AtBreak,
    /// Nothing repeats: its rows come once, those with `-fagus-repeat: at-break` never.
    No,
}

/// Which rows that show only where the table breaks (`-fagus-repeat: at-break`) a page of a
/// table leaves out: those of the header on the table's first page, those of the footer on its
/// last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hiding {
    header: bool,
    footer: bool,
}

/// The collapsed borders of a table on pages that leave rows out (see [`Hiding`]): resolved as
/// if those rows were not there, so that the rows around them share their own borders. The rows
/// keep the heights they were laid out in.
struct Without {
    hiding: Hiding,
    /// Whether each row is left out.
    hidden: Vec<bool>,
    /// As [`Laid::edges`], none for the cells of rows left out.
    edges: Vec<Option<[Edge; 4]>>,
    /// As [`Laid::borders`] and [`Laid::bottoms`], none for rows left out.
    borders: Vec<Vec<Fragment>>,
    bottoms: Vec<Vec<Fragment>>,
}

/// Restores the width that the percentage paddings of table cells referred to before a table
/// was laid out (see [`Layouter::cell_basis_for`]).
struct CellBasis<'l> {
    cell: &'l std::cell::Cell<Option<Au>>,
    outer: Option<Au>,
}

impl Drop for CellBasis<'_> {
    fn drop(&mut self) {
        self.cell.set(self.outer);
    }
}

/// The cells of rows that break across pages, laid out again where they break.
struct Cells<'l, 'a, 't> {
    layouter: &'l Layouter<'a>,
    laid: &'t Laid,
    /// The cells of the rows, by their index in the table.
    indexes: &'t [usize],
    /// Whether the rows start at the very top of the page, with nothing above them there (see
    /// [`Space::flush`]): monolithic content that does not fit in a cell stays there only then.
    flush: bool,
    /// The cells (by position in `indexes`) laid out whole past the end of the page because
    /// their bottom padding and border did not fit below their content (see `Cells::lay`).
    whole: Vec<usize>,
    /// How much shorter than laid out each cell (by position in `indexes`) breaks: what its
    /// row grew past its height on the table's first page (see `Laid::grown`).
    shrinks: &'t [Au],
    /// The cells (by position in `indexes`) that broke at a forced break inside them.
    forced: Vec<usize>,
    /// The room that a footer repeated below the rows takes on the page.
    footer: Au,
}

impl LineBoxes for Cells<'_, '_, '_> {
    fn lay(
        &mut self,
        position: usize,
        token: Option<&BreakToken>,
        remaining: Au,
        at_top: bool,
    ) -> Option<(Fragment, Option<BreakToken>)> {
        let index = *self.indexes.get(position)?;
        let laid = self.laid;
        let original = &laid.cells[index];
        // A cell that fits as it is stays as it is, with its content aligned, unless a forced
        // break inside it breaks it.
        if token.is_none()
            && original.rect.height <= remaining
            && !self.layouter.breaks_inside(laid.grid.cells[index].id)
        {
            // In rows that break, cells draw their collapsed borders themselves.
            let mut fragment = original.clone();
            laid.draw_collapsed(index, &mut fragment);
            return Some((fragment, None));
        }
        let slot = &laid.grid.cells[index];
        // A cell with collapsed borders is laid out with its own borders and then given the
        // collapsed ones (`collapse_cell`): its room and height are those of the cell with its
        // own borders. Where it continues from the page before, its content starts below half of
        // the border above its rows, as in Chrome.
        let style = &self.layouter.tree.get(slot.id).style;
        let continues = token.is_some() && !style.clones_decorations();
        let (above, below, lift) =
            laid.edges[index]
                .as_ref()
                .map_or((Au::ZERO, Au::ZERO, Au::ZERO), |edges| {
                    let shift = Au::from_px(edges[TOP].used_width() - style.border_width.top);
                    let above = if continues {
                        Au::from_px(edges[TOP].used_width() / 2.0)
                    } else {
                        shift
                    };
                    (
                        above,
                        Au::from_px(edges[BOTTOM].used_width() - style.border_width.bottom),
                        above - shift,
                    )
                });
        let space = Space {
            remaining: Some(remaining - above),
            at_top,
            flush: self.flush,
            ..Space::unfragmented(laid.spans[index])
        };
        // A cell is as tall as its rows: the rest of its height breaks below its content.
        let shrink = self.shrinks.get(position).copied().unwrap_or(Au::ZERO);
        let full = original.rect.height
            - laid.edges[index].as_ref().map_or(Au::ZERO, |edges| {
                Au::from_px(edges[TOP].used_width() - style.border_width.top)
            })
            - below
            - shrink;
        let height = self
            .layouter
            .stretched_rest(slot.id, full, laid.spans[index], token);
        let outer = self.layouter.forced_break.replace(false);
        let outcome = self.layouter.cell_container(
            slot.id,
            space,
            token,
            Some(laid.content_widths[index]),
            height,
            true,
        );
        // (Whether the cell broke at a forced break, the flag kept for the layout around.)
        let forced = self.layouter.forced_break.get();
        self.layouter.forced_break.set(forced || outer);
        let mut outcome = outcome?;
        self.forced.retain(|&other| other != position);
        if forced && outcome.token.is_some() {
            self.forced.push(position);
        }
        // Chrome does not break a cell before its bottom padding and border (Blink's
        // `FinalizeForFragmentation`): a cell that goes on from the page before, or starts the
        // page with nothing above it (not even the padding of a box around the table, see
        // [`Space::flush`]), and whose content ends on the page without room for them below it
        // is laid out whole, reaching past the end of the page, which the rows then go on below
        // (see `parallel::split_line`). Elsewhere its row moves to the next page, as Blink
        // breaks before a row that does not fit unless nothing at all precedes it.
        if (token.is_some() || at_top && self.flush)
            && let Some(rest) = outcome.token.as_ref()
            && self
                .layouter
                .breaks_in_bottom_frame(slot.id, Some(rest), laid.spans[index])
        {
            let model = self.layouter.box_model(style, laid.spans[index], None);
            let content_end = outcome
                .fragment
                .children
                .iter()
                .map(|child| child.rect.y + child.rect.height)
                .max()
                .unwrap_or(Au::ZERO);
            if content_end + model.padding[BOTTOM] + model.border[BOTTOM] > remaining - above
                && let Some(whole) = self.layouter.cell_container(
                    slot.id,
                    Space {
                        remaining: None,
                        ..space
                    },
                    token,
                    Some(laid.content_widths[index]),
                    height,
                    true,
                )
            {
                outcome = whole;
                self.whole.push(position);
            }
        }
        let mut fragment = outcome.fragment;
        if let Some(edges) = &laid.edges[index] {
            collapse_cell(&mut fragment, style, edges, true);
            for child in &mut fragment.children {
                child.rect.y += lift;
            }
            fragment.rect.height += lift;
        }
        // A cell that ends on the page in a row that grew past its height on the table's first
        // page (see `Laid::grown`) is as tall as the row's part on the page, which ends with the
        // page where another cell breaks, and its content is aligned in that height, as in
        // Chrome (Blink lays the cells out again at the height of the row grown to them, which
        // fits on the page, and aligns their content in it, `FinalizeTableCellLayout`).
        if token.is_none() && outcome.token.is_none() && shrink > Au::ZERO {
            let free = remaining - self.intrinsic_end(index);
            let shift = match style.vertical_align {
                VerticalAlign::Middle => free / 2,
                VerticalAlign::Bottom => free,
                _ => Au::ZERO,
            };
            if shift > Au::ZERO {
                for child in &mut fragment.children {
                    child.rect.y += shift;
                }
            }
        }
        fragment.rect.x = original.rect.x;
        Some((fragment, outcome.token))
    }

    fn broke_forced(&self, position: usize) -> bool {
        self.forced.contains(&position)
    }

    /// A cell that ends with its rows' part where another one broke at a forced break is laid
    /// out at that height in Chrome: its content moves down by `by` where it is aligned to the
    /// bottom, by half of it where it is aligned to the middle.
    fn realign(&self, position: usize, fragment: &mut Fragment, by: Au) {
        let Some(&index) = self.indexes.get(position) else {
            return;
        };
        let shift = match self
            .layouter
            .tree
            .get(self.laid.grid.cells[index].id)
            .style
            .vertical_align
        {
            VerticalAlign::Middle => by / 2,
            VerticalAlign::Bottom => by,
            _ => Au::ZERO,
        };
        if shift > Au::ZERO {
            for child in &mut fragment.children {
                child.rect.y += shift;
            }
        }
    }

    fn last_resort(&self, position: usize, token: &BreakToken) -> bool {
        self.indexes.get(position).is_some_and(|&index| {
            let slot = &self.laid.grid.cells[index];
            // A cell that spans rows breaks below its content as well as anywhere else where
            // its content, bottom padding and border fit above the break, as in Chrome (Blink
            // keeps them together in a cell and compares the cell's intrinsic block size): only
            // where they do not is its break the last resort.
            // So does a cell of one row that is taller than its content: where the bottom
            // padding and border below its content do not fit above the break, its break is the
            // last resort too, and its row moves to the next page where it can; elsewhere, even
            // in its own bottom border, it breaks in the room below its content (Blink's
            // `FinishFragmentation` makes only a box no taller than its content the last resort).
            let below_content = |rest: Au| {
                let (full, shift) = self.full_height(position, index);
                self.intrinsic_end(index) > full - rest + shift
            };
            match token {
                BreakToken::Rest { rest } => below_content(*rest),
                _ => self
                    .layouter
                    .breaks_in_bottom_frame(slot.id, Some(token), self.laid.spans[index]),
            }
        })
    }

    fn frame_rest(&self, position: usize, token: &BreakToken) -> bool {
        self.last_resort(position, token)
            || self.indexes.get(position).is_some_and(|&index| {
                matches!(token, BreakToken::Rest { .. })
                    && self.layouter.breaks_in_bottom_frame(
                        self.laid.grid.cells[index].id,
                        Some(token),
                        self.laid.spans[index],
                    )
            })
    }

    fn frame_below_rest(&self, position: usize) -> Option<(Au, bool)> {
        let index = *self.indexes.get(position)?;
        let style = &self.layouter.tree.get(self.laid.grid.cells[index].id).style;
        let model = self.layouter.box_model(style, self.laid.spans[index], None);
        let frame = model.padding[crate::block::BOTTOM] + model.border[crate::block::BOTTOM];
        let original = &self.laid.cells[index];
        let content_end = original
            .children
            .iter()
            .map(|child| child.rect.y + child.rect.height)
            .max()
            .unwrap_or(Au::ZERO);
        let shrink = self.shrinks.get(position).copied().unwrap_or(Au::ZERO);
        // A cell that breaks has its content at the top, whatever its alignment, as in Chrome:
        // where the content is aligned lower in the unbroken row, its intrinsic height counts.
        if matches!(
            style.vertical_align,
            VerticalAlign::Middle | VerticalAlign::Bottom
        ) {
            return Some((frame, self.intrinsic_end(index) < original.rect.height - shrink));
        }
        Some((frame, content_end + frame < original.rect.height - shrink))
    }

    fn worst_break_counts(&self) -> bool {
        true
    }

    fn flush(&self) -> bool {
        self.flush
    }

    fn grows_to_content(&self) -> bool {
        self.in_columns()
    }

    fn in_columns(&self) -> bool {
        self.layouter.columns.get() > 0
    }

    fn overflow_page(&self) -> Option<Au> {
        self.layouter.overflow_page()
    }

    fn overflow_room(&self) -> Au {
        self.footer
    }

    fn laid_whole(&self, index: usize) -> bool {
        self.whole.contains(&index)
    }

    fn bottom_frame(&self, position: usize) -> Au {
        let Some(&index) = self.indexes.get(position) else {
            return Au::ZERO;
        };
        let style = &self.layouter.tree.get(self.laid.grid.cells[index].id).style;
        let model = self.layouter.box_model(style, self.laid.spans[index], None);
        // A collapsed border is the cell's by half, as Blink lays cells out.
        let border = self.laid.edges[index]
            .as_ref()
            .map_or(model.border[BOTTOM], |edges| {
                Au::from_px(edges[BOTTOM].used_width() / 2.0)
            });
        model.padding[BOTTOM] + border
    }

    fn filler(&mut self, position: usize) -> Fragment {
        let Some(original) = self.indexes.get(position).map(|&index| &self.laid.cells[index]) else {
            return Fragment {
                rect: Rect::default(),
                kind: FragmentKind::Anonymous,
                children: Children::new(),
            };
        };
        let mut fragment = Fragment {
            rect: Rect::new(original.rect.x, Au::ZERO, original.rect.width, Au::ZERO),
            kind: original.kind.clone(),
            children: Children::new(),
        };
        // In rows that break, cells draw their collapsed borders themselves (see `lay`).
        if let Some(&index) = self.indexes.get(position) {
            self.laid.draw_collapsed(index, &mut fragment);
        }
        fragment
    }

    fn ends_open(&self, position: usize) -> bool {
        self.indexes
            .get(position)
            .is_some_and(|&index| self.laid.edges[index].is_some())
    }

    fn bare_filler(&mut self, position: usize) -> Fragment {
        let mut fragment = self.filler(position);
        if let FragmentKind::Box { style, .. } = &mut fragment.kind {
            let own = Arc::make_mut(style);
            own.background_color = Rgba::TRANSPARENT;
            own.background_image = Arc::from([]);
            own.box_shadow = Arc::from([]);
        }
        fragment
    }

    fn shifts_rows(&self) -> bool {
        true
    }
}

impl Cells<'_, '_, '_> {
    /// The height cell `index` (at `position` in the rows) breaks in, as `lay` lays it out (with
    /// its own borders), and how far its top lies below the top of its fragment.
    fn full_height(&self, position: usize, index: usize) -> (Au, Au) {
        let style = &self.layouter.tree.get(self.laid.grid.cells[index].id).style;
        let (shift, below) = self.laid.edges[index]
            .as_ref()
            .map_or((Au::ZERO, Au::ZERO), |edges| {
                (
                    Au::from_px(edges[TOP].used_width() - style.border_width.top),
                    Au::from_px(edges[BOTTOM].used_width() - style.border_width.bottom),
                )
            });
        let shrink = self.shrinks.get(position).copied().unwrap_or(Au::ZERO);
        (self.laid.cells[index].rect.height - shift - below - shrink, shift)
    }

    /// Where the content of cell `index` with its bottom padding and border (half the collapsed
    /// border, as Blink lays cells out) ends below the top of its fragment, its intrinsic height.
    fn intrinsic_end(&self, index: usize) -> Au {
        let style = &self.layouter.tree.get(self.laid.grid.cells[index].id).style;
        let model = self.layouter.box_model(style, self.laid.spans[index], None);
        let original = &self.laid.cells[index];
        let content = original
            .children
            .iter()
            .map(|child| (child.rect.y, child.rect.y + child.rect.height))
            .reduce(|(top, bottom), (child_top, child_bottom)| (top.min(child_top), bottom.max(child_bottom)))
            .map_or(Au::ZERO, |(top, bottom)| bottom - top);
        let (border_top, border_bottom) =
            self.laid.edges[index]
                .as_ref()
                .map_or((model.border[TOP], model.border[BOTTOM]), |edges| {
                    (
                        Au::from_px(edges[TOP].used_width()),
                        Au::from_px(edges[BOTTOM].used_width() / 2.0),
                    )
                });
        border_top + model.padding[TOP] + content + model.padding[BOTTOM] + border_bottom
    }
}

/// How many slots (rows times columns) a table with collapsing borders has at most. Its borders
/// are worked out for every line between slots, so a table of tens of thousands of columns (a few
/// cells spanning a thousand each) and as many rows would take gigabytes; beyond this, its
/// borders do not collapse.
const MAX_COLLAPSED_SLOTS: usize = 1_000_000;

/// Whether a table of style `style` has a fixed layout: `table-layout: fixed` and a width that
/// is not automatic, a length or a keyword such as `stretch` (Blink's `IsFixedTableLayout`).
fn fixed_table_layout(style: &ComputedStyle) -> bool {
    style.table_layout == TableLayout::Fixed && !matches!(style.width, Size::Auto)
}

/// Whether the borders of a table with `style` and `grid` collapse.
fn collapses(style: &ComputedStyle, grid: &Grid) -> bool {
    style.border_collapse == BorderCollapse::Collapse
        && grid.rows.len().saturating_mul(grid.columns + 1) <= MAX_COLLAPSED_SLOTS
}

/// A table laid out for one width, before it is cut into pages.
pub(crate) struct Laid {
    grid: Grid,
    /// The table box's style; with collapsing borders without its own borders, which the cells
    /// draw.
    style: Arc<ComputedStyle>,
    /// Width of the table's border box.
    width: Au,
    /// Border and padding of the table: top, right, bottom, left.
    frame: [Au; 4],
    heights: Vec<Au>,
    /// How far below the top of the first row each row begins.
    row_tops: Vec<Au>,
    /// For each row, the row after the last that cells starting in it or above it span: rows up
    /// to there stay together on a page.
    reach: Vec<usize>,
    spacing: (Au, Au),
    /// The cells, laid out with the height of their rows; `rect.y` is their offset from the top
    /// of their first row.
    cells: Vec<Fragment>,
    /// The width each cell is laid out in (its columns), and the width of its content.
    spans: Vec<Au>,
    content_widths: Vec<Au>,
    /// The collapsed borders of each cell, with `border-collapse: collapse`.
    edges: Vec<Option<[Edge; 4]>>,
    /// The collapsed borders, as Blink draws them, by row: those of its top line and those
    /// along it, from the row's top.
    borders: Vec<Vec<Fragment>>,
    /// The collapsed borders of each row's bottom line, from the row's top, for a row that the
    /// next one does not follow on the page.
    bottoms: Vec<Vec<Fragment>>,
    /// The collapsed borders on pages that leave rows of the header or footer out, for each way
    /// they can be left out (see [`Without`]).
    without: Vec<Without>,
    /// Backgrounds of rows and row groups, without borders, by box.
    backgrounds: Vec<(BoxId, Arc<ComputedStyle>)>,
    /// The outer shadows of rows, by row: a style with nothing else (see [`row_shadow`]).
    shadows: Vec<Option<Arc<ComputedStyle>>>,
    /// Backgrounds of column groups and columns, each group before its columns.
    columns: Vec<ColumnPart>,
    /// Where the first column begins and how far the columns reach from there, which the
    /// backgrounds of rows and row groups are positioned in.
    grid_left: Au,
    grid_width: Au,
    /// The baseline of each row, for cells aligned on it.
    row_baselines: Vec<Au>,
    /// Whether cells with `vertical-align: baseline` are moved to the baseline of their row,
    /// rather than left at its top (see `Layouter::laid_table`).
    aligned: bool,
    /// Which rows of the header and footer show content that depends on the page.
    per_page: Vec<bool>,
    /// The height of the content box of a table without rows.
    empty_height: Au,
    /// How much taller each row is than on the page where the table started, laid out in
    /// another width there (see `Layouter::laid_table`): Chrome keeps the heights of those rows
    /// (Blink's table break token), which grow to their cells only where they fit on the page,
    /// so a row that breaks is as tall as its cells need on each page (see `split_rows`).
    grown: Vec<Au>,
    /// The styles of cells that draw their collapsed borders themselves, by cell (see
    /// [`Laid::draw_collapsed`]).
    drawn: std::cell::RefCell<FxHashMap<usize, Arc<ComputedStyle>>>,
    /// While the table is placed: the runs of its fragment's children that each cell placed adds
    /// (the cell with its row's shadow and the backgrounds of its parts), with the cell's row
    /// (see [`Layouter::nest_positioned_rows`]).
    placed_cells: std::cell::RefCell<Vec<(Range<usize>, usize)>>,
    /// Whether a row or row group is positioned (see `Layouter::nest_positioned_rows`), found
    /// once for all pages of the table rather than on each.
    positioned_parts: std::cell::OnceCell<bool>,
    /// The styles of the fragments of positioned rows and row groups that differ from their
    /// boxes' (see where they are made).
    part_styles: FxHashMap<BoxId, Arc<ComputedStyle>>,
    /// The same for positioned cells, by their elements.
    cell_styles: FxHashMap<fagus_dom::NodeId, Arc<ComputedStyle>>,
}

const TOP: usize = 0;
const RIGHT: usize = 1;
const BOTTOM: usize = 2;
const LEFT: usize = 3;

/// The max-content width of a table of `table-layout: fixed` and a percentage width, in pixels:
/// as wide as anything around it lets it be (Blink's `TableTypes::kTableMaxInlineSize`).
const TABLE_MAX_INLINE_SIZE: f32 = 1_000_000.0;

impl Layouter<'_> {
    fn table_grid(&self, id: BoxId) -> Grid {
        let BoxKind::Table {
            header,
            bodies,
            footer,
            columns,
            ..
        } = &self.tree.get(id).kind
        else {
            return Grid::default();
        };
        let groups = header
            .iter()
            .map(|&group| (group, Part::Header))
            .chain(bodies.iter().map(|&group| (group, Part::Body)))
            .chain(footer.iter().map(|&group| (group, Part::Footer)));
        // In an automatic layout, columns that only `<col>` elements without a width make are
        // dropped at the end (Blink's mergeable columns, `ComputeMaximumNonMergeableColumnCount`).
        let table_style = &self.tree.get(id).style;
        let fixed_layout = fixed_table_layout(table_style);
        let given = |style: &Arc<ComputedStyle>| match &style.width {
            Size::Length(LengthPercentage::Percent(percentage)) => *percentage > 0.0,
            Size::Length(length) => length.fixed().is_some_and(|width| {
                let min = match &style.min_width {
                    Size::Length(min) => min.fixed().unwrap_or(0.0),
                    _ => 0.0,
                };
                width.max(min) > 0.0
            }),
            _ => false,
        };
        let mut grid = Grid {
            columns: if fixed_layout {
                columns.len()
            } else {
                columns.iter().rposition(given).map_or(0, |index| index + 1)
            },
            held: fagus_dom::budget::hold(fagus_dom::budget::Kind::Tables, 0),
            ..Grid::default()
        };
        for (group, part) in groups {
            let BoxKind::RowGroup { rows } = &self.tree.get(group).kind else {
                continue;
            };
            let first = grid.rows.len();
            // Slots taken by cells from rows above (rowspan), as ranges of columns; spans end with
            // their group. (Ranges, not a slot each: a few cells spanning a thousand columns and
            // thousands of rows would take gigabytes otherwise.)
            let mut taken: Vec<Vec<Range<usize>>> = vec![Vec::new(); rows.len()];
            for (index, &row) in rows.iter().enumerate() {
                fagus_dom::budget::check_time();
                grid.held.add(size_of::<Row>() as u64);
                grid.rows.push(Row { id: row, group, part });
                let mut above = std::mem::take(&mut taken[index]);
                grid.columns = grid
                    .columns
                    .max(above.iter().map(|range| range.end).max().unwrap_or(0));
                let BoxKind::Row { cells } = &self.tree.get(row).kind else {
                    continue;
                };
                above.sort_unstable_by_key(|range| range.start);
                let mut above = above.into_iter().peekable();
                let mut column = 0;
                for cell in cells {
                    while let Some(range) = above.next_if(|range| range.start <= column) {
                        column = column.max(range.end);
                    }
                    let left = rows.len() - index;
                    let rowspan = if cell.rowspan == 0 {
                        left
                    } else {
                        cell.rowspan.min(left)
                    }
                    .max(1);
                    grid.held
                        .add((size_of::<Slot>() + 2 * (rowspan - 1) * size_of::<Range<usize>>()) as u64);
                    for line in &mut taken[index + 1..index + rowspan] {
                        line.push(column..column + cell.colspan);
                    }
                    grid.cells.push(Slot {
                        id: cell.id,
                        row: first + index,
                        column,
                        colspan: cell.colspan,
                        rowspan,
                    });
                    column += cell.colspan;
                    grid.columns = grid.columns.max(column);
                }
            }
        }
        if self.tree.get(id).style.is_rtl() {
            grid.rtl = true;
            for slot in &mut grid.cells {
                slot.column = grid.columns.saturating_sub(slot.column + slot.colspan);
            }
        }
        grid
    }

    fn collapsed(&self, table: BoxId, grid: &Grid) -> Collapsed {
        let rows = grid.rows.len();
        let columns = grid.columns;
        let mut collapsed = Collapsed {
            horizontal: vec![vec![Edge::NONE; columns]; rows + 1],
            vertical: vec![vec![Edge::NONE; columns + 1]; rows],
        };
        let frame = |first: usize, last: usize, sides: [Edge; 4], collapsed: &mut Collapsed| {
            for column in 0..columns {
                Collapsed::consider(&mut collapsed.horizontal[first][column], sides[TOP]);
                Collapsed::consider(&mut collapsed.horizontal[last + 1][column], sides[BOTTOM]);
            }
            for row in first..=last {
                Collapsed::consider(&mut collapsed.vertical[row][0], sides[LEFT]);
                Collapsed::consider(&mut collapsed.vertical[row][columns], sides[RIGHT]);
            }
        };
        if rows > 0 {
            frame(
                0,
                rows - 1,
                edges(&self.tree.get(table).style, 0, 0),
                &mut collapsed,
            );
        }
        let mut start = 0;
        let mut groups = 0;
        while start < rows {
            let group = grid.rows[start].group;
            let end = (start..rows)
                .take_while(|&row| grid.rows[row].group == group)
                .last()
                .unwrap_or(start);
            frame(
                start,
                end,
                edges(&self.tree.get(group).style, 2, groups),
                &mut collapsed,
            );
            groups += 1;
            start = end + 1;
        }
        for (index, row) in grid.rows.iter().enumerate() {
            frame(
                index,
                index,
                edges(&self.tree.get(row.id).style, 3, index),
                &mut collapsed,
            );
        }
        if let BoxKind::Table { columns: styles, .. } = &self.tree.get(table).kind {
            for (index, style) in styles.iter().enumerate().take(columns) {
                let column = grid.physical(index);
                let sides = edges(style, 1, index);
                for row in 0..rows {
                    Collapsed::consider(&mut collapsed.vertical[row][column], sides[LEFT]);
                    Collapsed::consider(&mut collapsed.vertical[row][column + 1], sides[RIGHT]);
                }
            }
        }
        for (index, slot) in grid.cells.iter().enumerate() {
            let sides = edges(&self.tree.get(slot.id).style, 4, index);
            for column in slot.column..slot.column + slot.colspan {
                Collapsed::consider(&mut collapsed.horizontal[slot.row][column], sides[TOP]);
                Collapsed::consider(
                    &mut collapsed.horizontal[slot.row + slot.rowspan][column],
                    sides[BOTTOM],
                );
            }
            for row in slot.row..slot.row + slot.rowspan {
                Collapsed::consider(&mut collapsed.vertical[row][slot.column], sides[LEFT]);
                Collapsed::consider(
                    &mut collapsed.vertical[row][slot.column + slot.colspan],
                    sides[RIGHT],
                );
            }
        }
        collapsed
    }

    /// Whether `row` of a table shows only where the table breaks: `-fagus-repeat: at-break` on
    /// it or its row group.
    fn shows_at_break(&self, row: &Row) -> bool {
        [row.id, row.group]
            .iter()
            .any(|&id| self.tree.get(id).style.repeat == Repeat::AtBreak)
    }

    /// The collapsed borders of table `table` with `grid` (its columns at `lines`, its rows
    /// `heights` tall) for the pages that leave out rows of its header or footer that show only
    /// where it breaks (see [`Without`]); pieces to draw only if the table is `visible`.
    fn collapsed_without(
        &self,
        table: BoxId,
        grid: &Grid,
        lines: &[Au],
        heights: &[Au],
        visible: bool,
    ) -> Vec<Without> {
        let at_break: Vec<bool> = grid
            .rows
            .iter()
            .map(|row| row.part != Part::Body && self.shows_at_break(row))
            .collect();
        let in_part =
            |part: Part| (0..grid.rows.len()).any(|row| at_break[row] && grid.rows[row].part == part);
        let (header, footer) = (in_part(Part::Header), in_part(Part::Footer));
        [(true, false), (false, true), (true, true)]
            .into_iter()
            .filter(|&(hides_header, hides_footer)| (header || !hides_header) && (footer || !hides_footer))
            .map(|(hides_header, hides_footer)| {
                let hiding = Hiding {
                    header: hides_header,
                    footer: hides_footer,
                };
                let hidden: Vec<bool> = grid
                    .rows
                    .iter()
                    .zip(&at_break)
                    .map(|(row, &at_break)| {
                        at_break
                            && match row.part {
                                Part::Header => hiding.header,
                                Part::Footer => hiding.footer,
                                Part::Body => false,
                            }
                    })
                    .collect();
                // The grid without the rows left out; cells that span them span one row less
                // for each.
                let mut kept = Vec::with_capacity(grid.rows.len());
                let mut places = Vec::with_capacity(grid.rows.len());
                for (row, &hidden) in grid.rows.iter().zip(&hidden) {
                    places.push(kept.len());
                    if !hidden {
                        kept.push(*row);
                    }
                }
                let mut slots = Vec::with_capacity(grid.cells.len());
                let cells: Vec<Option<usize>> = grid
                    .cells
                    .iter()
                    .map(|slot| {
                        if hidden[slot.row] {
                            return None;
                        }
                        let end = (slot.row + slot.rowspan).min(grid.rows.len());
                        slots.push(Slot {
                            row: places[slot.row],
                            rowspan: (slot.row..end).filter(|&row| !hidden[row]).count().max(1),
                            ..*slot
                        });
                        Some(slots.len() - 1)
                    })
                    .collect();
                let reduced = Grid {
                    rows: kept,
                    cells: slots,
                    columns: grid.columns,
                    rtl: grid.rtl,
                    held: fagus_dom::budget::Held::none(fagus_dom::budget::Kind::Tables),
                };
                let collapsed = self.collapsed(table, &reduced);
                let edges = cells
                    .iter()
                    .map(|slot| slot.map(|slot| collapsed.cell(&reduced.cells[slot])))
                    .collect();
                let (mut borders, mut bottoms) = (
                    vec![Vec::new(); grid.rows.len()],
                    vec![Vec::new(); grid.rows.len()],
                );
                if visible {
                    let kept_heights: Vec<Au> = heights
                        .iter()
                        .zip(&hidden)
                        .filter(|(_, hidden)| !**hidden)
                        .map(|(&height, _)| height)
                        .collect();
                    let (reduced_borders, reduced_bottoms) = collapsed_pieces(
                        &collapsed,
                        &reduced,
                        lines,
                        &kept_heights,
                        &mut self.collapsed_styles.borrow_mut(),
                    );
                    let rows = (0..grid.rows.len()).filter(|&row| !hidden[row]);
                    for ((row, row_borders), row_bottoms) in rows.zip(reduced_borders).zip(reduced_bottoms) {
                        borders[row] = row_borders;
                        bottoms[row] = row_bottoms;
                    }
                }
                Without {
                    hiding,
                    hidden,
                    edges,
                    borders,
                    bottoms,
                }
            })
            .collect()
    }

    /// The (min, max) width of every column, and the percentage and fixed width each column was
    /// given by its cells or `<col>`.
    fn column_contributions(&self, table: BoxId, grid: &Grid, collapsed: Option<&Collapsed>) -> Columns {
        // The percentage paddings of the cells count for nothing here, also in a table inside a
        // cell of another being laid out.
        let _cleared = CellBasis {
            cell: &self.cell_basis,
            outer: self.cell_basis.replace(None),
        };
        let columns = grid.columns;
        let mut result = Columns {
            widths: vec![(Au::ZERO, Au::ZERO); columns],
            percentages: vec![None; columns],
            fixed: vec![None; columns],
            given: vec![None; columns],
            cells: Vec::with_capacity(grid.cells.len()),
            frames: Vec::with_capacity(grid.cells.len()),
        };
        let table_style = &self.tree.get(table).style;
        let fixed_layout = fixed_table_layout(table_style);
        if let BoxKind::Table { columns: styles, .. } = &self.tree.get(table).kind {
            for (index, style) in styles.iter().enumerate().take(columns) {
                let column = grid.physical(index);
                let min_width = match &style.min_width {
                    Size::Length(length) => Au::from_px(length.fixed().unwrap_or(0.0).max(0.0)),
                    _ => Au::ZERO,
                };
                match &style.width {
                    Size::Length(LengthPercentage::Percent(percentage)) => {
                        result.percentages[column] = Some(*percentage);
                    }
                    Size::Length(length) if let Some(px) = length.fixed() => {
                        let width = Au::from_px(px.max(min_width.to_px()));
                        result.fixed[column] = Some(width);
                        result.given[column] = Some(width);
                        // In an automatic layout the width of a `<col>` is the column's
                        // max-content width but not its minimum, which its cells and its
                        // `min-width` give (Blink's `TableTypes::CreateColumn`, which leaves out
                        // its `max-width`).
                        result.widths[column] = (if fixed_layout { width } else { min_width }, width);
                    }
                    _ => {}
                }
                result.widths[column].0 = result.widths[column].0.max(min_width);
                result.widths[column].1 = result.widths[column].1.max(min_width);
            }
        }
        // The cells of each column, merged: (min, max, whether one has a fixed width).
        let mut merged: Vec<Option<(Au, Au, bool)>> = vec![None; columns];
        let mut spanning: Vec<(&Slot, Au, Au)> = Vec::new();
        let spacing = if collapsed.is_some() {
            Au::ZERO
        } else {
            border_spacing(table_style).0
        };
        let end = self.table_end(table);
        for slot in &grid.cells {
            let style = &self.tree.get(slot.id).style;
            // A cell is at least as wide as its content, even with a smaller fixed width, which
            // otherwise takes the place of the content's max-content width (as in browsers).
            // The header and footer are measured with the named strings' values at the end of the
            // table, the widest running totals.
            let (mut min, max) = if grid.rows[slot.row].part == Part::Body {
                self.content_widths(slot.id)
            } else {
                self.at_string_position(Some(end), || self.content_widths(slot.id))
            };
            let mut given = self.fixed_width(slot.id);
            // A fixed `min-width` widens the cell's min-content width too (Blink's
            // `ComputeColumnInlineConstraints`).
            let mut least = self.fixed_min_width(slot.id);
            let mut max = max;
            // The fixed width of a cell in another writing mode comes with its padding along its
            // own lines, the top and bottom padding of a vertical cell, rather than the padding
            // across the column, as Chrome adds it (the cell is laid out with its own padding).
            if self.is_orthogonal(slot.id) && style.box_sizing != BoxSizing::BorderBox {
                let side = |padding: &LengthPercentage| Au::from_px(padding.resolve(0.0));
                let along = side(&style.padding.top) + side(&style.padding.bottom);
                let across = side(&style.padding.left) + side(&style.padding.right);
                given = given.map(|width| (width + along - across).clamp_non_negative());
            }
            // The cell's border and padding across its column, which a percentage width of its
            // content box leaves out.
            let mut frame = Au::from_px(
                style.border_width.left
                    + style.border_width.right
                    + style.padding.left.resolve(0.0)
                    + style.padding.right.resolve(0.0),
            );
            if let Some(collapsed) = collapsed {
                // The cell's own borders give way to half of the collapsed ones; a width of the
                // border box includes whichever borders the cell has (as in Chrome).
                let sides = collapsed.cell(slot);
                let own = Au::from_px(style.border_width.left + style.border_width.right);
                let shared = Au::from_px(f32::midpoint(sides[LEFT].used_width(), sides[RIGHT].used_width()));
                frame = (frame - own + shared).clamp_non_negative();
                min = (min - own + shared).clamp_non_negative();
                max = (max - own + shared).clamp_non_negative();
                if style.box_sizing != BoxSizing::BorderBox {
                    given = given.map(|width| (width - own + shared).clamp_non_negative());
                    least = least.map(|width| (width - own + shared).clamp_non_negative());
                }
            }
            let mut min = least.map_or(min, |least| min.max(least));
            let mut max = given.map_or(max, |width| width.max(min)).max(min);
            // A fixed `max-width` caps both, down to the cell's border and padding, though the
            // cell still fills its column: `max-width: 0` with `text-overflow: ellipsis` gives
            // up the content's width (Blink's `CreateCellInlineConstraint`). A percentage width
            // keeps its column.
            if let Some(cap) = self.fixed_max_width(slot.id, frame) {
                let cap = cap.max(least.unwrap_or(Au::ZERO));
                min = min.min(cap);
                max = max.min(cap).max(min);
            }
            result.cells.push(given);
            result.frames.push(frame);
            if slot.colspan > 1 {
                spanning.push((slot, min, max));
                continue;
            }
            // A cell with a fixed width takes the place of the max-content widths of the cells
            // without one in its column, and those widen it to their minimum only (Blink's
            // `CellInlineConstraint::Encompass`).
            let constrained = matches!(&style.width, Size::Length(length) if length.fixed().is_some());
            merged[slot.column] = Some(match merged[slot.column] {
                None => (min, max, constrained),
                Some((have_min, have_max, have_constrained)) => {
                    let min = have_min.max(min);
                    let max = if have_constrained == constrained {
                        have_max.max(max)
                    } else if have_constrained {
                        have_max.max(min)
                    } else {
                        min.max(max)
                    };
                    (min, max, have_constrained || constrained)
                }
            });
            match &style.width {
                Size::Length(LengthPercentage::Percent(percentage)) => {
                    let current = result.percentages[slot.column].unwrap_or(0.0);
                    result.percentages[slot.column] = Some(current.max(*percentage));
                }
                Size::Length(length) if length.fixed().is_some() => {
                    let current = result.fixed[slot.column].unwrap_or(Au::ZERO);
                    result.fixed[slot.column] = Some(current.max(max));
                }
                _ => {}
            }
        }
        // A column whose `<col>` gives it a width takes the cells' minimum, and their maximum
        // where one of them has a fixed width too (Blink's `TableTypes::Column::Encompass`).
        for ((column, cells), given) in result.widths.iter_mut().zip(&merged).zip(&result.given) {
            let Some((min, max, constrained)) = *cells else {
                continue;
            };
            if fixed_layout {
                *column = (column.0.max(min), column.1.max(max));
                continue;
            }
            let max = match (given.is_some(), constrained) {
                (false, _) | (true, true) => column.1.max(max),
                (true, false) => column.1.max(min),
            };
            *column = (column.0.max(min), max.max(min));
        }
        // Spanning cells widen their columns where their own width does not fit, narrowest
        // spans first, as in Chrome (see `crate::colspan`).
        if fixed_layout {
            // A fixed layout takes the widths of the first row's cells (see `fixed_widths`); a
            // spanning cell's content widens its columns evenly.
            spanning.sort_by_key(|(slot, _, _)| slot.colspan);
            for (slot, min, max) in spanning {
                let range = slot.column..(slot.column + slot.colspan).min(columns);
                for (wanted, pick) in [(min, 0usize), (max, 1usize)] {
                    let get = |column: &(Au, Au)| if pick == 0 { column.0 } else { column.1 };
                    let have: Au = result.widths[range.clone()].iter().map(get).sum();
                    if wanted <= have {
                        continue;
                    }
                    let extra = wanted - have;
                    let total: Au = result.widths[range.clone()].iter().map(|column| column.1).sum();
                    let count = range.len() as i32;
                    for column in &mut result.widths[range.clone()] {
                        let share = if total > Au::ZERO {
                            Au::from_px(extra.to_px() * column.1.to_px() / total.to_px())
                        } else {
                            extra / count
                        };
                        if pick == 0 {
                            column.0 += share;
                            column.1 = column.1.max(column.0);
                        } else {
                            column.1 += share;
                        }
                    }
                }
            }
        } else {
            spanning.sort_by_key(|(slot, _, _)| (slot.colspan, slot.column));
            for (slot, min, max) in spanning {
                let range = slot.column..(slot.column + slot.colspan).min(columns);
                let mut span: Vec<SpanColumn> = range
                    .clone()
                    .map(|column| SpanColumn {
                        min: result.widths[column].0,
                        max: result.widths[column].1,
                        percent: result.percentages[column],
                        constrained: result.fixed[column].is_some(),
                    })
                    .collect();
                let style = &self.tree.get(slot.id).style;
                let percent = match &style.width {
                    Size::Length(LengthPercentage::Percent(percentage)) => Some(*percentage),
                    _ => None,
                };
                let constrained = matches!(&style.width, Size::Length(length) if length.fixed().is_some());
                crate::colspan::widen(&mut span, (min, max), percent, constrained, spacing);
                for (column, widened) in range.zip(span) {
                    result.widths[column] = (widened.min, widened.max);
                    result.percentages[column] = widened.percent;
                }
            }
        }
        result
    }

    /// The width of a cell's border box that its fixed `min-width` gives, if it has one.
    /// The fixed `max-width` of the border box of cell `id` whose border and padding across its
    /// column are `frame` (half the collapsed borders where they collapse), never less than
    /// those (Blink's `InlineSizesFromStyle`).
    fn fixed_max_width(&self, id: BoxId, frame: Au) -> Option<Au> {
        let style = &self.tree.get(id).style;
        let MaxSize::Length(length) = &style.max_width else {
            return None;
        };
        let width = Au::from_px(length.fixed()?);
        Some(if style.box_sizing == BoxSizing::BorderBox {
            width.max(frame)
        } else {
            width + frame
        })
    }

    fn fixed_min_width(&self, id: BoxId) -> Option<Au> {
        let style = &self.tree.get(id).style;
        let Size::Length(length) = &style.min_width else {
            return None;
        };
        let width = Au::from_px(length.fixed()?);
        if style.box_sizing == BoxSizing::BorderBox {
            return Some(width);
        }
        let model = self.box_model(style, Au::ZERO, None);
        Some(width + model.border[LEFT] + model.border[RIGHT] + model.padding[LEFT] + model.padding[RIGHT])
    }

    /// The table's border and padding (top, right, bottom, left); with collapsing borders, half
    /// of the outer borders and no padding.
    fn table_frame(
        &self,
        style: &ComputedStyle,
        containing_width: Au,
        collapsed: Option<&Collapsed>,
    ) -> [Au; 4] {
        if let Some(collapsed) = collapsed {
            let widest = |edges: &mut dyn Iterator<Item = &Edge>| {
                Au::from_px(edges.map(Edge::used_width).fold(0.0, f32::max) / 2.0)
            };
            [
                widest(&mut collapsed.horizontal.first().into_iter().flatten()),
                widest(&mut collapsed.vertical.iter().filter_map(|row| row.last())),
                widest(&mut collapsed.horizontal.last().into_iter().flatten()),
                widest(&mut collapsed.vertical.iter().filter_map(|row| row.first())),
            ]
        } else {
            let basis = containing_width.to_px();
            [
                Au::from_px(style.border_width.top + style.padding.top.resolve(basis)),
                Au::from_px(style.border_width.right + style.padding.right.resolve(basis)),
                Au::from_px(style.border_width.bottom + style.padding.bottom.resolve(basis)),
                Au::from_px(style.border_width.left + style.padding.left.resolve(basis)),
            ]
        }
    }

    /// The min-content and max-content widths of a table's border box.
    pub(crate) fn table_intrinsic(&self, id: BoxId) -> (Au, Au) {
        let style = &self.tree.get(id).style;
        let grid = self.table_grid(id);
        let collapse = collapses(style, &grid);
        let collapsed = collapse.then(|| self.collapsed(id, &grid));
        let columns = self.column_contributions(id, &grid, collapsed.as_ref());
        let frame = self.table_frame(style, Au::ZERO, collapsed.as_ref());
        let spacing = if collapse || grid.columns == 0 {
            Au::ZERO
        } else {
            border_spacing(style).0 * (grid.columns as i32 + 1)
        };
        let outside = frame[LEFT] + frame[RIGHT] + spacing;
        let min: Au = columns.widths.iter().map(|column| column.0).sum();
        let max: Au = columns.widths.iter().map(|column| column.1).sum();
        // Columns with percentages widen the table here too, unless it is inside a cell, flex or
        // grid container (Blink's `TableNode::AllowColumnPercentages`).
        let max = if style.width == Size::MaxContent || self.nested_table(id) {
            max
        } else {
            max.max(percentage_width(&columns))
        };
        // A fixed layout ignores the content here too: the table is at least as wide as its
        // `<col>` and first-row widths, and one of a percentage width as wide as it may be
        // (Blink's `ComputeGridInlineMinMax` and `kTableMaxInlineSize`), so that a long line in
        // it widens no cell or table around it.
        let (min, max) = if fixed_table_layout(style) {
            let min = fixed_min(&columns, &grid, self);
            let max = if matches!(style.width, Size::Length(LengthPercentage::Percent(_))) {
                Au::from_px(TABLE_MAX_INLINE_SIZE)
            } else {
                max.max(min)
            };
            (min, max)
        } else {
            (min, max)
        };
        let caption = self.caption_min(id);
        ((min + outside).max(caption), (max + outside).max(caption))
    }

    /// The width of the border box of a table whose `width` is `length` px: with its border and
    /// padding where that is the content box's (only half its outer borders where they
    /// collapse), and never narrower than its content.
    pub(crate) fn table_fixed_width(&self, id: BoxId, length: Au) -> Au {
        let style = &self.tree.get(id).style;
        let grid = self.table_grid(id);
        let collapsed = collapses(style, &grid).then(|| self.collapsed(id, &grid));
        let frame = self.table_frame(style, Au::ZERO, collapsed.as_ref());
        let border_box = if style.box_sizing == BoxSizing::BorderBox {
            length
        } else {
            length + frame[LEFT] + frame[RIGHT]
        };
        border_box.max(self.table_intrinsic(id).0)
    }

    /// The height of a table's box laid out in a containing block `containing_width` wide,
    /// without its captions.
    pub(crate) fn table_height(&self, id: BoxId, containing_width: Au) -> Au {
        let laid = self.laid_table_by((
            id,
            containing_width,
            (false, false),
            Au::ZERO,
            None,
            self.is_unfragmented(),
            None,
        ));
        let rows = if laid.heights.is_empty() {
            laid.empty_height
        } else {
            laid.heights.iter().copied().sum::<Au>() + laid.spacing.1 * (laid.heights.len() as i32 + 1)
        };
        laid.frame[TOP] + rows + laid.frame[BOTTOM]
    }

    /// Whether a table is inside a table cell, a flex or a grid container.
    fn nested_table(&self, id: BoxId) -> bool {
        self.nested_tables
            .get_or_init(|| {
                let mut found = FxHashSet::default();
                let roots = self.tree.root.into_iter().chain(self.tree.fixed.iter().copied());
                let mut stack: Vec<(BoxId, bool)> = roots.map(|root| (root, false)).collect();
                let mut seen = FxHashSet::default();
                while let Some((id, inside)) = stack.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let kind = &self.tree.get(id).kind;
                    if inside && matches!(kind, BoxKind::Table { .. }) {
                        found.insert(id);
                    }
                    // The children of a row are its cells: a table in one is inside.
                    let holds = inside
                        || matches!(
                            kind,
                            BoxKind::Flex { .. } | BoxKind::Grid { .. } | BoxKind::Row { .. }
                        );
                    stack.extend(self.tree.children(id).into_iter().map(|child| (child, holds)));
                }
                found
            })
            .contains(&id)
    }

    /// The widest min-content width of a table's captions: the table is at least as wide.
    fn caption_min(&self, table: BoxId) -> Au {
        match &self.tree.get(table).kind {
            BoxKind::Table { captions, .. } => captions
                .iter()
                .map(|&caption| self.intrinsic(caption).0)
                .max()
                .unwrap_or(Au::ZERO),
            _ => Au::ZERO,
        }
    }

    /// Lays out a table for a containing block width (cached: every page uses the same), filling
    /// it if the table is a flex or grid item sized by its container (`Layouter::table_fills`).
    ///
    /// Chrome aligns the cells of a table on the baseline of their row only where it lays the
    /// table out without fragmentation (Blink's `TableRowLayoutAlgorithm`): when it prints, in
    /// a box it lays out whole (see `crate::unfragmented`), and in an inline table, measured in
    /// Chrome (not in a table inside an `inline-block`, nor in one inside an inline table's
    /// cell). Elsewhere it puts them at the top of their row, which keeps the height that
    /// aligned cells give it.
    ///
    /// With `rows_from`, the width of the containing block where the table started on an earlier
    /// page, a table that goes on in a containing block of another width (beside a float there,
    /// not here) keeps the heights of the rows it had there, as in Chrome (Blink's table break
    /// token keeps the rows of its first fragment while the columns take the new width).
    fn laid_table(
        &self,
        id: BoxId,
        containing_width: Au,
        containing_height: Option<Au>,
        rows_from: Option<Au>,
    ) -> Rc<Laid> {
        let inline = self.inline_table.replace(false);
        let key = (
            id,
            containing_width,
            (self.table_fills.replace(false), self.table_unsized.replace(false)),
            self.table_grow.replace(Au::ZERO),
            containing_height,
            inline || self.is_unfragmented(),
            rows_from.filter(|&width| width != containing_width),
        );
        self.laid_table_by(key)
    }

    /// The table laid out as `key` tells (see [`Layouter::laid_table`]), cached.
    fn laid_table_by(&self, key: crate::block::TableKey) -> Rc<Laid> {
        if let Some(laid) = self.table_cache.borrow().get(&key) {
            return Rc::clone(laid);
        }
        let (id, containing_width, fills, grow, containing_height, aligned, rows_from) = key;
        let first = rows_from
            .map(|width| self.laid_table_by((id, width, fills, grow, containing_height, aligned, None)));
        let laid = Rc::new(self.lay_out_table(
            id,
            (containing_width, containing_height),
            (fills, grow),
            aligned,
            first.as_ref().map(|first| first.heights.as_slice()),
        ));
        self.table_cache.borrow_mut().insert(key, Rc::clone(&laid));
        laid
    }

    fn lay_out_table(
        &self,
        id: BoxId,
        (containing_width, containing_height): (Au, Option<Au>),
        ((fills, given), grow): ((bool, bool), Au),
        aligned: bool,
        first_rows: Option<&[Au]>,
    ) -> Laid {
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        let grid = self.table_grid(id);
        let collapse = collapses(style, &grid);
        let collapsed = collapse.then(|| self.collapsed(id, &grid));
        let frame = self.table_frame(style, containing_width, collapsed.as_ref());
        // A table without columns has no border spacing either (as in Chrome).
        let spacing = if collapse || grid.columns == 0 {
            (Au::ZERO, Au::ZERO)
        } else {
            border_spacing(style)
        };
        let contributions = self.column_contributions(id, &grid, collapsed.as_ref());
        let columns = grid.columns;
        let gaps = spacing.0 * (columns as i32 + 1);
        let outside = frame[LEFT] + frame[RIGHT] + gaps;
        let min: Au = contributions.widths.iter().map(|column| column.0).sum::<Au>() + outside;
        let max: Au = contributions.widths.iter().map(|column| column.1).sum::<Au>() + outside;
        // Columns with percentages widen an auto table to give every column its max-content width.
        let max = if style.width == Size::MaxContent {
            max
        } else {
            max.max(percentage_width(&contributions) + outside)
        };

        // The table's width: as wide as its content unless given, never narrower than it.
        let margins = Au::from_px(
            style.margin.left.resolve_or(containing_width.to_px(), 0.0)
                + style.margin.right.resolve_or(containing_width.to_px(), 0.0),
        );
        let available = (containing_width - margins).clamp_non_negative();
        let length = |length: &LengthPercentage| {
            let width = Au::from_px(length.resolve(containing_width.to_px()));
            // `box-sizing: border-box` is the default for tables; content-box widths add the
            // frame.
            if style.box_sizing == BoxSizing::BorderBox {
                width
            } else {
                width + frame[LEFT] + frame[RIGHT]
            }
        };
        // Sizing keywords take the table's min-content and max-content widths, or the available
        // width (CSS Box Sizing 3, §3.1).
        let fit = max.min(available).max(min);
        let keyword = |size: &Size| match size {
            Size::MinContent => Some(min),
            Size::MaxContent => Some(max),
            Size::FitContent(_) => Some(fit),
            Size::Stretch => Some(available),
            _ => None,
        };
        // A flex or grid container that sizes the table gives it its width instead of its own.
        let specified = match &style.width {
            _ if fills => Some(available),
            Size::Length(width) => Some(length(width)),
            other => keyword(other),
        };
        let fixed_layout = fixed_table_layout(style) && specified.is_some();
        let width = match specified {
            // A fixed layout ignores the content: the table is as wide as given, or as its first
            // row's columns (CSS 2.1, §17.5.2.1).
            Some(specified) if fixed_layout => {
                specified.max(fixed_min(&contributions, &grid, self) + outside)
            }
            _ => specified.unwrap_or(fit),
        };
        // `max-width`, then `min-width`, and never narrower than the content (which a fixed
        // layout ignores). A table that fills the width its float, inline box, flex or grid
        // container gave it was clamped there, with percentages of the real containing block;
        // here they would be of that width (`max-width: 50%` would halve it again).
        let width = match &style.max_width {
            _ if fills => width,
            MaxSize::Length(max_width) => width.min(length(max_width)),
            MaxSize::MinContent => width.min(min),
            MaxSize::MaxContent => width.min(max),
            MaxSize::FitContent(_) => width.min(fit),
            MaxSize::Stretch => width.min(available),
            MaxSize::None => width,
        };
        let width = match &style.min_width {
            _ if fills => width,
            Size::Length(min_width) => width.max(length(min_width)),
            other => keyword(other).map_or(width, |bound| width.max(bound)),
        };
        let width = if fixed_layout { width } else { width.max(min) };
        let width = width.max(self.caption_min(id));
        let grid_width = (width - outside).clamp_non_negative();
        let widths = if fixed_layout {
            fixed_widths(&contributions, &grid, self, grid_width)
        } else {
            distribute_columns(
                &contributions.widths,
                &contributions.percentages,
                &contributions.fixed,
                grid_width,
            )
        };
        // `visibility: collapse` takes columns and rows out of the table (CSS 2.1, §17.5.5):
        // they and the spacing after them take no room, the table gets narrower, and their cells
        // are hidden, as in Chrome.
        let column_styles = match &layout_box.kind {
            BoxKind::Table { columns: styles, .. } => styles.as_slice(),
            _ => &[],
        };
        let collapsed_columns: Vec<bool> = (0..columns)
            .map(|column| {
                column_styles
                    .get(grid.physical(column))
                    .is_some_and(|column| column.visibility == Visibility::Collapse)
            })
            .collect();
        let collapsed_rows: Vec<bool> = grid
            .rows
            .iter()
            .map(|row| {
                [row.id, row.group]
                    .iter()
                    .any(|&id| self.tree.get(id).style.visibility == Visibility::Collapse)
            })
            .collect();
        let mut widths = widths;
        let mut width = width;
        for (column, _) in collapsed_columns
            .iter()
            .enumerate()
            .filter(|(_, collapsed)| **collapsed)
        {
            width -= widths[column] + spacing.0;
            widths[column] = -spacing.0;
        }
        if collapsed_columns.contains(&true) {
            width = width.max(self.caption_min(id));
        }
        let mut lines = Vec::with_capacity(columns + 1);
        let mut x = frame[LEFT] + spacing.0;
        for width in &widths {
            lines.push(x);
            x += *width + spacing.0;
        }
        lines.push(x);
        // The percentage paddings of the cells refer to the width of the columns with the
        // spacing between them, no longer to nothing as for the columns' widths (Blink, Gecko).
        let _basis = self.cell_basis_for((lines[columns] - spacing.0 - lines[0]).clamp_non_negative());

        let end = self.table_end(id);
        let per_page: Vec<bool> = (0..grid.rows.len())
            .map(|row| {
                grid.rows[row].part != Part::Body
                    && grid
                        .cells
                        .iter()
                        .any(|slot| slot.row == row && self.depends_on_page(slot.id))
            })
            .collect();

        // Cells at their widths, unfragmented.
        let mut cells = Vec::with_capacity(grid.cells.len());
        // How much taller than its content a cell's `height` asks its row to be.
        let mut extras = Vec::with_capacity(grid.cells.len());
        let mut sides = Vec::with_capacity(grid.cells.len());
        let mut spans = Vec::with_capacity(grid.cells.len());
        let mut content_widths = Vec::with_capacity(grid.cells.len());
        for slot in &grid.cells {
            let cell_style = &self.tree.get(slot.id).style;
            let span =
                (lines[slot.column + slot.colspan] - lines[slot.column] - spacing.0).clamp_non_negative();
            let padding_basis = self.cell_basis.get().unwrap_or(span).to_px();
            let model_padding = |length: &LengthPercentage| Au::from_px(length.resolve(padding_basis));
            let padding_x =
                model_padding(&cell_style.padding.left) + model_padding(&cell_style.padding.right);
            let own = |width: f32| Au::from_px(width);
            let shared = collapsed.as_ref().map(|collapsed| collapsed.cell(slot));
            let (left, right) = match &shared {
                Some(edges) => (
                    Au::from_px(edges[LEFT].used_width() / 2.0),
                    Au::from_px(edges[RIGHT].used_width() / 2.0),
                ),
                None => (
                    own(cell_style.border_width.left),
                    own(cell_style.border_width.right),
                ),
            };
            let content_width = (span - left - right - padding_x).clamp_non_negative();
            let position = (grid.rows[slot.row].part != Part::Body).then_some(end);
            let restricted = self.restricted_cell.replace(!matches!(style.height, Size::Auto));
            let measured =
                self.at_string_position(position, || self.cell(slot.id, span, content_width, None));
            self.restricted_cell.set(restricted);
            let mut fragment = measured.unwrap_or_else(|| Fragment {
                rect: Rect::default(),
                kind: FragmentKind::Anonymous,
                children: Children::new(),
            });
            // `empty-cells: hide`: a cell without content draws neither borders nor backgrounds
            // (CSS 2.1, §17.6.1.1).
            if !collapse && cell_style.empty_cells == EmptyCells::Hide && !has_content(&fragment) {
                fragment.kind = FragmentKind::Anonymous;
            }
            // Chrome adds the horizontal padding to the height of a cell whose lines run down.
            let padding_y = if self.is_orthogonal(slot.id) {
                padding_x
            } else {
                model_padding(&cell_style.padding.top) + model_padding(&cell_style.padding.bottom)
            };
            extras.push(match &cell_style.height {
                Size::Length(LengthPercentage::Px(px)) => {
                    let height = Au::from_px(*px);
                    let own_y = own(cell_style.border_width.top) + own(cell_style.border_width.bottom);
                    // In quirks mode the height of a cell includes its padding and borders
                    // (the table cell height box-sizing quirk of the Quirks Mode standard); in
                    // collapsed tables those are the halves of the shared borders, while the
                    // fragment still has the cell's own.
                    let wanted =
                        if cell_style.box_sizing == BoxSizing::BorderBox || self.tree.quirky.is_some() {
                            match &shared {
                                Some(edges) => {
                                    height + own_y
                                        - Au::from_px(edges[TOP].used_width() / 2.0)
                                        - Au::from_px(edges[BOTTOM].used_width() / 2.0)
                                }
                                None => height,
                            }
                        } else {
                            height + padding_y + own_y
                        };
                    (wanted - fragment.rect.height).clamp_non_negative()
                }
                _ => Au::ZERO,
            });
            if let Some(edges) = shared {
                self.collapsed_styles
                    .borrow_mut()
                    .collapse_shared(&mut fragment, cell_style, &edges);
            }
            sides.push(shared);
            spans.push(span);
            content_widths.push(content_width);
            cells.push(fragment);
        }

        // Row heights: the tallest cell, then rows grown for cells that span them.
        let row_count = grid.rows.len();
        let mut heights = vec![Au::ZERO; row_count];
        for (index, row) in grid.rows.iter().enumerate() {
            if let Size::Length(LengthPercentage::Px(px)) = &self.tree.get(row.id).style.height {
                heights[index] = Au::from_px(*px);
            }
        }
        // Baselines of cells aligned on the baseline, per row.
        let baselines: Vec<Option<Au>> = grid
            .cells
            .iter()
            .zip(&cells)
            .map(|(slot, fragment)| {
                (self.tree.get(slot.id).style.vertical_align == VerticalAlign::Baseline)
                    .then(|| first_baseline(fragment, self.writing_mode()))
                    .flatten()
            })
            .collect();
        let mut row_baselines = vec![Au::ZERO; row_count];
        for (slot, baseline) in grid.cells.iter().zip(&baselines) {
            if let Some(baseline) = baseline {
                row_baselines[slot.row] = row_baselines[slot.row].max(*baseline);
            }
        }
        let shift = |index: usize| match baselines[index] {
            Some(baseline) => row_baselines[grid.cells[index].row] - baseline,
            None => Au::ZERO,
        };
        let natural = |index: usize| {
            let height = cells[index].rect.height + extras[index] + shift(index);
            // With collapsing borders, rows meet in the middle of the borders between them.
            match &sides[index] {
                Some(edges) => {
                    height - Au::from_px(f32::midpoint(edges[TOP].used_width(), edges[BOTTOM].used_width()))
                }
                None => height,
            }
        };
        for (index, slot) in grid.cells.iter().enumerate() {
            if slot.rowspan == 1 {
                heights[slot.row] = heights[slot.row].max(natural(index));
            }
        }
        let mut by_span: Vec<usize> = (0..grid.cells.len())
            .filter(|&index| grid.cells[index].rowspan > 1)
            .collect();
        by_span.sort_by_key(|&index| (grid.cells[index].rowspan, grid.cells[index].row));
        let constrained: Vec<bool> = grid
            .rows
            .iter()
            .map(|row| {
                matches!(
                    self.tree.get(row.id).style.height,
                    Size::Length(LengthPercentage::Px(_))
                )
            })
            .collect();
        let mut spans_start = vec![false; row_count];
        for &index in &by_span {
            spans_start[grid.cells[index].row] = true;
        }
        for index in by_span {
            let slot = &grid.cells[index];
            let rows = slot.row..(slot.row + slot.rowspan).min(row_count);
            distribute_rowspan(
                &mut heights,
                (&constrained, &spans_start),
                rows,
                natural(index),
                spacing.1,
            );
        }

        // A fixed height of a row group gives its rows the room their content leaves, as the
        // height of a cell spanning them would (Blink's `DistributeSectionFixedBlockSizeToRows`;
        // WPT css-break table/overflow-scroll-section, Chrome 157).
        let no_spans = vec![false; row_count];
        let mut start = 0;
        while start < row_count {
            let group = grid.rows[start].group;
            let end = (start..row_count)
                .find(|&row| grid.rows[row].group != group)
                .unwrap_or(row_count);
            if let Size::Length(LengthPercentage::Px(fixed)) = self.tree.get(group).style.height {
                distribute_rowspan(
                    &mut heights,
                    (&constrained, &no_spans),
                    start..end,
                    Au::from_px(fixed),
                    spacing.1,
                );
            }
            start = end;
        }
        // A fixed table height gives its rows the room their content leaves (CSS Tables 3,
        // §3.10.5, as Chrome does it).
        // So does the height a flex or grid container gives it (`grow` more than its rows).
        // A percentage refers to the height of a containing block that does not depend on the
        // content.
        let resolve = |length: &LengthPercentage| {
            length
                .fixed()
                .or_else(|| containing_height.map(|containing| length.resolve(containing.to_px())))
        };
        let specified = match &style.height {
            Size::Length(length) if !given => resolve(length),
            // An aspect ratio gives the table the height of its width, as a height would
            // (Chrome 157, measured).
            Size::Auto if !given => crate::ratio::ratio(style).map(|ratio| {
                let (frame_x, frame_y) = (frame[LEFT] + frame[RIGHT], frame[TOP] + frame[BOTTOM]);
                let border_box = if crate::ratio::ratio_sizing(style) == BoxSizing::BorderBox {
                    width.to_px() / ratio
                } else {
                    (width - frame_x).clamp_non_negative().to_px() / ratio + frame_y.to_px()
                };
                if style.box_sizing == BoxSizing::BorderBox {
                    border_box
                } else {
                    border_box - frame_y.to_px()
                }
            }),
            _ => None,
        };
        // `max-height` lowers a given height, and `min-height` raises it or the content's (the
        // table never gets shorter than its rows, and the minimum wins over the maximum).
        let specified = match &style.max_height {
            MaxSize::Length(max) => {
                specified.map(|height| resolve(max).map_or(height, |max| height.min(max)))
            }
            _ => specified,
        };
        let table_height = specified.is_some();
        let minimum = match &style.min_height {
            Size::Length(min) if !given => resolve(min).filter(|&min| min > 0.0),
            _ => None,
        };
        let target = match (specified, minimum) {
            (Some(height), Some(min)) => Some(height.max(min)),
            (height, min) => height.or(min),
        };
        let room = if let Some(px) = target {
            let specified = Au::from_px(px);
            let border_box = if style.box_sizing == BoxSizing::BorderBox {
                specified
            } else {
                specified + frame[TOP] + frame[BOTTOM]
            };
            Some(border_box - frame[TOP] - frame[BOTTOM] - spacing.1 * (row_count as i32 + 1))
        } else {
            (grow > Au::ZERO).then(|| heights.iter().copied().sum::<Au>() + grow)
        };
        if let Some(room) = room {
            let kinds: Vec<RowHeight> = grid
                .rows
                .iter()
                .map(|row| match &self.tree.get(row.id).style.height {
                    Size::Length(LengthPercentage::Percent(percentage)) => RowHeight::Percent(*percentage),
                    Size::Length(length) if length.fixed().is_some() => RowHeight::Fixed,
                    _ => RowHeight::Auto,
                })
                .collect();
            let bodies: Vec<bool> = grid.rows.iter().map(|row| row.part == Part::Body).collect();
            grow_rows(&mut heights, &kinds, &bodies, room);
        }
        // A table without rows is as tall as its height asks.
        let empty_height = room
            .filter(|_| grid.rows.is_empty())
            .unwrap_or(Au::ZERO)
            .clamp_non_negative();
        // Rows that went on from a page where the table was laid out in another width keep the
        // heights they had there, where their content leaves them room.
        let mut grown = vec![Au::ZERO; heights.len()];
        if let Some(first) = first_rows.filter(|first| first.len() == heights.len()) {
            for ((height, first), grown) in heights.iter_mut().zip(first).zip(&mut grown) {
                *grown = (*height - *first).clamp_non_negative();
                *height = (*height).max(*first);
            }
        }
        // A collapsed row is as tall as the content of its cells, without their `height` and
        // without the spacing after it (Chrome keeps that much room for it in print); a cell
        // that spans it from above is as tall as the other rows it spans, and makes the first
        // of them taller if its content needs more.
        if collapsed_rows.contains(&true) {
            let content = |index: usize| natural(index) - extras[index] - shift(index);
            for (row, _) in collapsed_rows
                .iter()
                .enumerate()
                .filter(|(_, collapsed)| **collapsed)
            {
                let tallest = grid
                    .cells_in(row..=row)
                    .filter(|&index| grid.cells[index].rowspan == 1)
                    .map(content)
                    .max()
                    .unwrap_or(Au::ZERO);
                heights[row] = tallest - spacing.1;
            }
            // The spacing between row groups stays, also after one whose rows all collapse.
            for (row, grid_row) in grid.rows.iter().enumerate() {
                let last_of_group = grid
                    .rows
                    .get(row + 1)
                    .is_none_or(|next| next.group != grid_row.group);
                let group_collapsed = (0..=row)
                    .rev()
                    .take_while(|&other| grid.rows[other].group == grid_row.group)
                    .all(|other| collapsed_rows[other]);
                if last_of_group && group_collapsed {
                    heights[row] += spacing.1;
                }
            }
            for (index, slot) in grid.cells.iter().enumerate() {
                if !collapsed_rows[slot.row] && slot.rowspan > 1 {
                    let room = spanned_height(&heights, &collapsed_rows, slot, spacing.1);
                    let wanted = content(index);
                    if wanted > room {
                        heights[slot.row] += wanted - room;
                    }
                }
            }
        }

        // Cells take the height of their rows; their content is aligned inside.
        for (index, slot) in grid.cells.iter().enumerate() {
            let mut height = spanned_height(&heights, &collapsed_rows, slot, spacing.1);
            let mut offset_y = Au::ZERO;
            let mut x = lines[slot.column];
            if collapsed_rows[slot.row]
                || collapsed_columns[slot.column..slot.column + slot.colspan]
                    .iter()
                    .all(|&collapsed| collapsed)
            {
                let fragment = &mut cells[index];
                fragment.kind = FragmentKind::Anonymous;
                fragment.children = Children::new();
                fragment.rect = Rect::new(x, Au::ZERO, Au::ZERO, Au::ZERO);
                continue;
            }
            if let Some(edges) = &sides[index] {
                let top = Au::from_px(edges[TOP].used_width() / 2.0);
                let bottom = Au::from_px(edges[BOTTOM].used_width() / 2.0);
                height += top + bottom;
                offset_y = -top;
                x -= Au::from_px(edges[LEFT].used_width() / 2.0);
            }
            // A cell whose lines run across the table's is as long as its rows: it is laid out
            // again at their height, and aligns its content across its lines. So is a cell
            // whose content has percentage heights where the table or the cell has a fixed
            // height (as in Chrome); its content is aligned by where it ends.
            let fixed_cell = matches!(
                &self.tree.get(slot.id).style.height,
                Size::Length(length) if length.fixed().is_some()
            );
            let percentages = !self.is_orthogonal(slot.id)
                && (table_height || fixed_cell)
                && self.has_percentage_heights(slot.id);
            if percentages {
                let position = (grid.rows[slot.row].part != Part::Body).then_some(end);
                if let Some(mut relaid) = self.at_string_position(position, || {
                    self.cell(slot.id, spans[index], content_widths[index], Some(height))
                }) {
                    if let Some(edges) = &sides[index] {
                        collapse_cell(&mut relaid, self.tree.get(slot.id).style.as_ref(), edges, false);
                    }
                    if matches!(cells[index].kind, FragmentKind::Anonymous) {
                        relaid.kind = FragmentKind::Anonymous;
                    }
                    let style = &self.tree.get(slot.id).style;
                    let bottom = Au::from_px(
                        style.border_width.bottom
                            + style
                                .padding
                                .bottom
                                .resolve(self.cell_basis.get().unwrap_or(spans[index]).to_px()),
                    );
                    let content_end = relaid
                        .children
                        .iter()
                        .map(|child| child.rect.y + child.rect.height)
                        .max()
                        .unwrap_or(Au::ZERO);
                    relaid.rect.height = content_end + bottom;
                    cells[index] = relaid;
                }
            }
            if self.is_orthogonal(slot.id) {
                let position = (grid.rows[slot.row].part != Part::Body).then_some(end);
                if let Some(mut relaid) = self.at_string_position(position, || {
                    self.cell(slot.id, spans[index], content_widths[index], Some(height))
                }) {
                    if let Some(edges) = &sides[index] {
                        collapse_cell(&mut relaid, self.tree.get(slot.id).style.as_ref(), edges, false);
                    }
                    if matches!(cells[index].kind, FragmentKind::Anonymous) {
                        relaid.kind = FragmentKind::Anonymous;
                    }
                    cells[index] = relaid;
                }
            }
            let fragment = &mut cells[index];
            let free = height - fragment.rect.height - shift(index);
            let content_shift = if aligned { shift(index) } else { Au::ZERO }
                + match self.tree.get(slot.id).style.vertical_align {
                    VerticalAlign::Middle => free / 2,
                    VerticalAlign::Bottom => free,
                    _ => Au::ZERO,
                };
            if content_shift != Au::ZERO {
                for child in &mut fragment.children {
                    child.rect.y += content_shift;
                }
            }
            fragment.rect.x = x;
            fragment.rect.y = offset_y;
            fragment.rect.height = height;
        }

        // The table paints the collapsed borders, unless it is hidden (as in Chrome).
        let (borders, bottoms) = match &collapsed {
            Some(collapsed) if style.visibility == Visibility::Visible => collapsed_pieces(
                collapsed,
                &grid,
                &lines,
                &heights,
                &mut self.collapsed_styles.borrow_mut(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let without = if collapse {
            self.collapsed_without(
                id,
                &grid,
                &lines,
                &heights,
                style.visibility == Visibility::Visible,
            )
        } else {
            Vec::new()
        };

        let mut backgrounds = Vec::new();
        let mut seen_groups = FxHashSet::default();
        for row in &grid.rows {
            for id in [row.group, row.id] {
                if id == row.group && !seen_groups.insert(id) {
                    continue;
                }
                let row_style = &self.tree.get(id).style;
                let painted = !row_style.background_color.is_transparent()
                    || row_style
                        .background_image
                        .iter()
                        .any(|image| !matches!(image, fagus_style::values::Image::None));
                if painted {
                    let mut background = (**row_style).clone();
                    background.border_width = fagus_style::values::Sides::all(0.0);
                    background.box_shadow = Arc::from([]);
                    // The backgrounds go where the cells are, with the row or row group they
                    // belong to (see `Layouter::nest_positioned_rows`): not offset or painted as
                    // positioned boxes of their own, and without the outline the part draws
                    // around itself (see `Layouter::part_outlines`).
                    background.position = Position::Static;
                    background.z_index = None;
                    background.outline_width = 0.0;
                    backgrounds.push((id, Arc::new(background)));
                }
            }
        }

        // Positioned rows, row groups and cells whose `top` or `bottom` is a percentage: their
        // fragments (see `Layouter::nest_positioned_rows`) take it of the `height` that the part
        // around them (the table, the row group, the row) specifies as a length, and as `auto`
        // without one, as the heights of the parts depend on their content, as in Blink (WPT
        // `css-position/position-relative-008` … `-013`).
        let mut part_styles = FxHashMap::default();
        let mut cell_styles = FxHashMap::default();
        for row in &grid.rows {
            for (part_id, parent) in [(row.group, id), (row.id, row.group)] {
                if !part_styles.contains_key(&part_id)
                    && let Some(own) =
                        resolved_part_insets(&self.tree.get(part_id).style, &self.tree.get(parent).style)
                {
                    part_styles.insert(part_id, Arc::new(own));
                }
            }
            for cell in self.tree.children(row.id) {
                let cell = self.tree.get(cell);
                if let Some(node) = cell.node
                    && let Some(own) = resolved_part_insets(&cell.style, &self.tree.get(row.id).style)
                {
                    cell_styles.insert(node, Arc::new(own));
                }
            }
        }

        let shadows = grid
            .rows
            .iter()
            .map(|row| {
                let row_style = &self.tree.get(row.id).style;
                let outer = row_style.box_shadow.iter().any(|shadow| !shadow.inset);
                (outer && row_style.visibility == Visibility::Visible).then(|| {
                    Arc::new(ComputedStyle::from(ComputedValues {
                        box_shadow: row_style
                            .box_shadow
                            .iter()
                            .filter(|shadow| !shadow.inset)
                            .cloned()
                            .collect(),
                        color: row_style.color,
                        ..ComputedValues::default()
                    }))
                })
            })
            .collect();

        let column_parts = match &layout_box.kind {
            BoxKind::Table {
                column_backgrounds, ..
            } => column_backgrounds
                .iter()
                .filter(|part| part.start < columns && part.span > 0)
                .map(|part| {
                    let last = (part.start + part.span).min(columns) - 1;
                    let (left, right) = if grid.rtl {
                        (grid.physical(last), grid.physical(part.start))
                    } else {
                        (part.start, last)
                    };
                    ColumnPart {
                        start: part.start,
                        span: last + 1 - part.start,
                        x: lines[left],
                        width: lines[right + 1] - spacing.0 - lines[left],
                        style: Arc::new(ComputedStyle::from(ComputedValues {
                            background_color: part.style.background_color,
                            background_image: Arc::clone(&part.style.background_image),
                            background_position_x: Arc::clone(&part.style.background_position_x),
                            background_position_y: Arc::clone(&part.style.background_position_y),
                            background_size: Arc::clone(&part.style.background_size),
                            background_repeat: Arc::clone(&part.style.background_repeat),
                            background_origin: Arc::clone(&part.style.background_origin),
                            background_clip: Arc::clone(&part.style.background_clip),
                            background_blend_mode: Arc::clone(&part.style.background_blend_mode),
                            ..ComputedValues::default()
                        })),
                    }
                })
                .collect(),
            _ => Vec::new(),
        };

        let style = if collapse {
            let mut without_borders = (**style).clone();
            without_borders.border_width = fagus_style::values::Sides::all(0.0);
            Arc::new(without_borders)
        } else {
            Arc::clone(style)
        };
        let reach = grid
            .cells
            .iter()
            .fold(vec![0; heights.len()], |mut reach, slot| {
                if let Some(row) = reach.get_mut(slot.row) {
                    *row = (*row).max(slot.row + slot.rowspan);
                }
                reach
            })
            .into_iter()
            .scan(0, |reached, row| {
                *reached = row.max(*reached);
                Some(*reached)
            })
            .collect();
        Laid {
            grid,
            style,
            width,
            frame,
            row_tops: heights
                .iter()
                .scan(Au::ZERO, |top, &height| {
                    let row_top = *top;
                    *top += height + spacing.1;
                    Some(row_top)
                })
                .collect(),
            reach,
            heights,
            spacing,
            cells,
            spans,
            content_widths,
            edges: sides,
            borders,
            bottoms,
            without,
            backgrounds,
            shadows,
            columns: column_parts,
            grid_left: lines[0],
            grid_width: (lines[columns] - spacing.0 - lines[0]).clamp_non_negative(),
            row_baselines,
            aligned,
            per_page,
            empty_height,
            grown,
            drawn: std::cell::RefCell::default(),
            placed_cells: std::cell::RefCell::default(),
            positioned_parts: std::cell::OnceCell::new(),
            part_styles,
            cell_styles,
        }
    }

    /// Where a table ends in the string log.
    fn table_end(&self, table: BoxId) -> usize {
        self.tree.string_positions.get(&table).map_or(0, |&(end, _)| end)
    }

    /// Runs `layout` with `string()` taking its values at `position` in the string log (or where
    /// its content is, with `None`).
    fn at_string_position<T>(&self, position: Option<usize>, layout: impl FnOnce() -> T) -> T {
        let outer = self.string_position.replace(position);
        let result = layout();
        self.string_position.set(outer);
        result
    }

    /// Whether the content of a box depends on the page it is on: `string()` or a page number.
    fn depends_on_page(&self, id: BoxId) -> bool {
        if let BoxKind::Inline { items } = &self.tree.get(id).kind
            && items.iter().any(|item| {
                matches!(
                    item,
                    InlineItem::NamedString { .. }
                        | InlineItem::PageNumber { .. }
                        | InlineItem::TargetPage { .. }
                        | InlineItem::Leader { .. }
                )
            })
        {
            return true;
        }
        self.tree
            .children(id)
            .into_iter()
            .any(|child| self.depends_on_page(child))
    }

    /// Cell `index` laid out again, in the place and at the size it has in `laid`.
    fn relaid_cell(&self, laid: &Laid, index: usize) -> Fragment {
        let original = &laid.cells[index];
        let slot = &laid.grid.cells[index];
        let style = &self.tree.get(slot.id).style;
        let height = self.is_orthogonal(slot.id).then_some(original.rect.height);
        let Some(mut fragment) = self.cell(slot.id, laid.spans[index], laid.content_widths[index], height)
        else {
            return original.clone();
        };
        if let Some(edges) = &laid.edges[index] {
            collapse_cell(&mut fragment, style, edges, false);
        }
        let shift = match (
            &style.vertical_align,
            first_baseline(&fragment, self.writing_mode()),
        ) {
            (VerticalAlign::Baseline, Some(baseline)) if laid.aligned => {
                laid.row_baselines[slot.row] - baseline
            }
            _ => Au::ZERO,
        };
        let free = original.rect.height - fragment.rect.height - shift;
        let content_shift = shift
            + match style.vertical_align {
                VerticalAlign::Middle => free / 2,
                VerticalAlign::Bottom => free,
                _ => Au::ZERO,
            };
        if content_shift != Au::ZERO {
            for child in &mut fragment.children {
                child.rect.y += content_shift;
            }
        }
        fragment.rect = original.rect;
        if let (FragmentKind::Box { sides, .. }, FragmentKind::Box { sides: drawn, .. }) =
            (&mut fragment.kind, &original.kind)
        {
            *sides = *drawn;
        }
        fragment
    }

    /// Lays out a cell with a fixed content width, and the height of its row where its lines
    /// run across the table's.
    /// Whether an in-flow child of a box has a percentage `height`, `min-height` or
    /// `max-height`.
    /// Found once per box, as items broken across pages ask on every page.
    pub(crate) fn has_percentage_heights(&self, id: BoxId) -> bool {
        let (BoxKind::Block { children } | BoxKind::Flex { children } | BoxKind::Grid { children }) =
            &self.tree.get(id).kind
        else {
            return false;
        };
        if let Some(&found) = self.percentage_heights.borrow().get(&id) {
            return found;
        }
        let percentage = |length: &LengthPercentage| length.fixed().is_none();
        let found = children.iter().any(|&child| {
            let style = &self.tree.get(child).style;
            !style.is_out_of_flow()
                && (matches!(&style.height, Size::Length(length) if percentage(length))
                    || matches!(&style.min_height, Size::Length(length) if percentage(length))
                    || matches!(&style.max_height, MaxSize::Length(length) if percentage(length)))
        });
        self.percentage_heights.borrow_mut().insert(id, found);
        found
    }

    fn cell(&self, id: BoxId, width: Au, content_width: Au, height: Option<Au>) -> Option<Fragment> {
        // A cell with `columns` lays out its content in columns, as in Chrome.
        if self.tree.get(id).style.is_multicol() {
            return self
                .multicol_sized(
                    id,
                    Space::unfragmented(width),
                    None,
                    (Some(content_width), height),
                )
                .map(|outcome| outcome.fragment);
        }
        self.block_container_with(
            id,
            Space::unfragmented(width),
            None,
            Some(content_width),
            height,
            true,
        )
        .map(|outcome| {
            // A cell narrower than its border and padding (a fixed layout can make it so) is as
            // wide as its column, as in Chrome.
            let mut fragment = outcome.fragment;
            if content_width == Au::ZERO {
                fragment.rect.width = fragment.rect.width.min(width);
            }
            fragment
        })
    }

    /// Lays out the part of a table that fits on the current page.
    /// Makes `width` the width the percentage paddings of table cells refer to until the
    /// returned guard is dropped, which restores the one before (of a table around).
    fn cell_basis_for(&self, width: Au) -> CellBasis<'_> {
        CellBasis {
            cell: &self.cell_basis,
            outer: self.cell_basis.replace(Some(width)),
        }
    }

    pub(crate) fn table(&self, id: BoxId, space: Space, token: Option<&BreakToken>) -> Option<Outcome> {
        // A table that is an item of a column flex container breaks before its bottom border
        // and padding where they do not fit below its last rows, as in Chrome; elsewhere the
        // spacing below the rows shrinks to fit (see below). Neither where the table has to end
        // early (`Layouter::ending_early`): it then breaks between rows or moves.
        let early = self.ending_early.get();
        let frame_breaks = self.table_frame_breaks.replace(false) && !early;
        let layout_box = self.tree.get(id);
        let style = &layout_box.style;
        // Captions above the table that broke on the page before go on first; the table box
        // follows them as if it started on this page.
        let (resumed, token) = match token {
            Some(BreakToken::Captions { index, inner }) => (Some((*index, inner.as_deref())), None),
            _ => (None, token),
        };
        let rows_from = if token.is_some() || resumed.is_some() {
            self.table_widths.borrow().get(&id).copied()
        } else {
            self.table_widths.borrow_mut().insert(id, space.containing_width);
            None
        };
        let laid = self.laid_table(id, space.containing_width, space.containing_height, rows_from);
        let _basis = self.cell_basis_for(laid.grid_width);
        let basis = space.containing_width.to_px();
        let margin_top = Au::from_px(style.margin.top.resolve_or(basis, 0.0));
        let margin_bottom = Au::from_px(style.margin.bottom.resolve_or(basis, 0.0));
        // Auto margins center the table.
        let free = (space.containing_width - laid.width).clamp_non_negative();
        let margin_left = match (style.margin.left.is_auto(), style.margin.right.is_auto()) {
            (true, true) => free / 2,
            (true, false) => free - Au::from_px(style.margin.right.resolve_or(basis, 0.0)),
            // In a right-to-left containing block the table keeps its right margin.
            (false, false) if space.rtl => {
                space.containing_width - laid.width - Au::from_px(style.margin.right.resolve_or(basis, 0.0))
            }
            _ => Au::from_px(style.margin.left.resolve_or(basis, 0.0)),
        };
        let continuing = token.is_some();
        // The table's top margin, taken where its first part is.
        let margin_top_here = if continuing || resumed.is_some() {
            Au::ZERO
        } else {
            margin_top
        };
        let (start, mut continued, overflow, after_cover) = match token {
            Some(BreakToken::Table {
                row,
                cells,
                offset,
                overflow,
                covered,
                ..
            }) => (
                *row,
                (!cells.is_empty()).then_some((cells.as_slice(), *offset)),
                *overflow,
                *covered && !cells.is_empty(),
            ),
            _ => (0, None, Au::ZERO, false),
        };
        let limit = space.remaining.map(|remaining| remaining - margin_top_here);
        // Only captions below the table are left: the table box ended on a page before.
        if let Some((index, _)) = resumed
            && let BoxKind::Table { captions, .. } = &layout_box.kind
            && captions
                .get(index)
                .is_some_and(|&caption| self.tree.get(caption).style.caption_side == CaptionSide::Bottom)
        {
            return Some(
                match self.side_captions(
                    id,
                    laid.width,
                    CaptionSide::Bottom,
                    (limit, space.at_top),
                    resumed,
                ) {
                    CaptionsPart::Broken(fragments, token) => captions_broken(
                        fragments,
                        (margin_left, laid.width, limit.unwrap_or(Au::ZERO)),
                        token,
                        Au::ZERO,
                    ),
                    CaptionsPart::Done(fragments, height) => Outcome {
                        fragment: Fragment {
                            rect: Rect::new(margin_left, Au::ZERO, laid.width, height),
                            kind: FragmentKind::Anonymous,
                            children: fragments.into(),
                        },
                        token: None,
                        margin_bottom,
                        collapsed_top: Margin::default(),
                        collapsed_bottom: Margin::of(margin_bottom),
                        through: false,
                        passed_breaks: (BreakBetween::Auto, BreakBetween::Auto),
                    },
                    CaptionsPart::Moves => return None,
                },
            );
        }
        if laid.grid.rows.is_empty() {
            return self.empty_table(
                id,
                &laid,
                (token, resumed, limit, space.at_top),
                (margin_left, margin_top_here, margin_bottom),
            );
        }

        let rows_of = |part: Part| -> Vec<usize> {
            laid.grid
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.part == part)
                .map(|(index, _)| index)
                .collect()
        };
        let mut header = rows_of(Part::Header);
        let mut body = rows_of(Part::Body);
        let mut footer = rows_of(Part::Footer);
        // A header or footer repeats on every page only if it avoids breaks inside and is no
        // taller than a quarter of the page, as in Chrome (CSS Tables 3, §5.4); otherwise its
        // rows come once, before and after the others.
        let avoids_inside = |group: BoxId| {
            matches!(
                self.tree.get(group).style.break_inside,
                BreakInside::Avoid | BreakInside::AvoidPage
            )
        };
        let repeats = |rows: &[usize]| {
            rows.first()
                .is_some_and(|&row| avoids_inside(laid.grid.rows[row].group))
                && self.repeatable_height(rows.iter().map(|&row| laid.heights[row]), laid.spacing.1)
        };
        // A footer without rows (all of them `display: none`) repeats too, as in Chrome, where
        // it is a section of no height: it takes no room but what the table reserves for it.
        let footer_group = match &layout_box.kind {
            BoxKind::Table { footer, .. } => *footer,
            _ => None,
        };
        let footer_repeats_now = || {
            if footer.is_empty() {
                footer_group.is_some_and(|group| {
                    avoids_inside(group) && self.repeatable_height(std::iter::empty(), laid.spacing.1)
                })
            } else {
                repeats(&footer)
            }
        };
        // Rows of a header or footer that does not repeat come once, with the others, except for
        // those that show only where the table breaks: they repeat on their own where they are
        // short enough to (and their row group avoids breaks inside), else they never show.
        let at_break_rows = |rows: &[usize]| -> Vec<usize> {
            rows.iter()
                .copied()
                .filter(|&row| self.shows_at_break(&laid.grid.rows[row]))
                .collect()
        };
        let repeating = |whole: bool, rows: &[usize]| {
            let carried = at_break_rows(rows);
            if whole {
                Repeating::Whole
            } else if repeats(&carried) {
                Repeating::AtBreak
            } else {
                Repeating::No
            }
        };
        // Whether they repeat is decided where the table starts, as in Chrome: in columns, by the
        // height of its first one.
        let decided = continuing
            .then(|| self.table_repeats.borrow().get(&id).copied())
            .flatten();
        let (header_repeating, footer_repeating) = decided.unwrap_or_else(|| {
            (
                repeating(repeats(&header), &header),
                repeating(footer_repeats_now(), &footer),
            )
        });
        if !continuing {
            self.table_repeats
                .borrow_mut()
                .insert(id, (header_repeating, footer_repeating));
        }
        let footer_repeats = footer_repeating != Repeating::No;
        // Rows that show only where the table breaks and that the page leaves out whether it
        // breaks or not.
        let header_dropped = header_repeating == Repeating::No && !at_break_rows(&header).is_empty();
        let footer_dropped = footer_repeating == Repeating::No && !at_break_rows(&footer).is_empty();
        if header_repeating != Repeating::Whole {
            let (carried, once): (Vec<usize>, Vec<usize>) = header
                .iter()
                .partition(|&&row| self.shows_at_break(&laid.grid.rows[row]));
            body.splice(0..0, once);
            header = if header_repeating == Repeating::AtBreak {
                carried
            } else {
                Vec::new()
            };
        }
        if footer_repeating != Repeating::Whole {
            let (carried, mut once): (Vec<usize>, Vec<usize>) = footer
                .iter()
                .partition(|&&row| self.shows_at_break(&laid.grid.rows[row]));
            body.append(&mut once);
            footer = if footer_repeating == Repeating::AtBreak {
                carried
            } else {
                Vec::new()
            };
        }
        let rows_height =
            |rows: &[usize]| -> Au { rows.iter().map(|&row| laid.heights[row] + laid.spacing.1).sum() };

        // Captions above the table on its first page, below it on its last.
        let mut above = Vec::new();
        let mut caption_height = Au::ZERO;
        if !continuing {
            match self.top_captions(id, laid.width, (limit, space.at_top), resumed) {
                CaptionsPart::Done(fragments, height) => {
                    above = fragments;
                    caption_height = height;
                }
                CaptionsPart::Broken(fragments, token) => {
                    return Some(captions_broken(
                        fragments,
                        (margin_left, laid.width, limit.unwrap_or(Au::ZERO)),
                        token,
                        margin_top_here,
                    ));
                }
                CaptionsPart::Moves => return None,
            }
        }

        // The table box starts on this page, also where its rows follow its caption on the next.
        let starts = !matches!(token, Some(BreakToken::Table { started: true, .. }));
        // With `box-decoration-break: clone`, every part of a table with separate borders has
        // its top and bottom border and padding, which its rows leave room for (the spacing
        // between the rows and those comes only where the table starts and ends), as in Chrome.
        let clones = laid.style.clones_decorations() && !collapses(style, &laid.grid);
        let caption_limit = limit;
        let top = if starts || clones {
            laid.frame[TOP]
        } else {
            Au::ZERO
        };
        // The rows get at least a pixel on every page, so that a table whose cloned border and
        // padding take all of the page makes progress, as in Blink.
        let limit = limit.map(|limit| {
            if clones {
                (limit - laid.frame[BOTTOM]).max(top + Au::from_px(1.0))
            } else {
                limit
            }
        });
        let mut y = top + laid.spacing.1;
        let mut children: Vec<Fragment> = Vec::new();
        laid.placed_cells.borrow_mut().clear();
        // The rows placed whole and where, for their collapsed borders.
        let placed: std::cell::RefCell<Vec<(usize, Au)>> = std::cell::RefCell::new(Vec::new());
        let place_rows = |rows: &[usize], y: &mut Au, children: &mut Vec<Fragment>| {
            for &row in rows {
                self.place_row(&laid, row, *y, children);
                placed.borrow_mut().push((row, *y));
                *y += laid.heights[row] + laid.spacing.1;
            }
        };
        // Rows of the header and footer that show only where the table breaks.
        let at_break = |row: usize| self.shows_at_break(&laid.grid.rows[row]);
        // Where the rows above end in the string log: before body row `index`, or after the
        // row's own assignments if its rest continues there.
        let end = self.table_end(id);
        let boundary = |index: usize, continued: bool| {
            body.get(index)
                .and_then(|&row| self.tree.string_positions.get(&laid.grid.rows[row].id))
                .map_or(end, |&(before, after)| if continued { after } else { before })
        };
        let place_repeated = |rows: &[usize], position: usize, y: &mut Au, children: &mut Vec<Fragment>| {
            for &row in rows {
                if laid.per_page[row] {
                    let cells: Vec<(usize, Fragment)> = self.at_string_position(Some(position), || {
                        laid.grid
                            .cells_in(row..=row)
                            .map(|index| (index, self.relaid_cell(&laid, index)))
                            .collect()
                    });
                    self.place_cells(&laid, row, *y, children, |index| {
                        cells
                            .iter()
                            .find(|(relaid, _)| *relaid == index)
                            .map_or_else(|| laid.cells[index].clone(), |(_, cell)| cell.clone())
                    });
                } else {
                    self.place_row(&laid, row, *y, children);
                }
                placed.borrow_mut().push((row, *y));
                *y += laid.heights[row] + laid.spacing.1;
            }
        };
        // Only the bottom border and padding of a table broken before them are left (a token
        // past the end of the body, see below); a token at its end leaves the closing footer.
        let frame_only = start > body.len() && continued.is_none() && continuing;
        // Below a page that monolithic content of the rows covered, the rest of their cells
        // comes at the top of the page; where no rows follow it, Chrome repeats no header.
        let had_header = !header.is_empty();
        let rest_alone = after_cover && footer.is_empty() && {
            let mut end = start;
            while end + 1 < body.len() && self.spans_into(&laid, body[end], body[end + 1]) {
                end += 1;
            }
            end + 1 >= body.len()
        };
        // Below a page that monolithic content of the last rows covered, only the end of the
        // table is left, without a header (see `ends_past` below).
        let end_after_cover = start >= body.len()
            && matches!(token, Some(BreakToken::Table { covered: true, cells, .. }) if cells.is_empty());
        // On the table's first page, the collapsed borders are those of the table without the
        // header's rows that show only where it breaks; on its last, without the footer's.
        // Where a header or footer does not repeat, its rows that show only where the table
        // breaks are left out on every page.
        let hides_header = header_dropped || !continuing && header.iter().any(|&row| at_break(row));
        let hides_footer = footer_dropped || footer.iter().any(|&row| at_break(row));
        let header: Vec<usize> = header
            .into_iter()
            .filter(|&row| (continuing || !at_break(row)) && !frame_only && !rest_alone && !end_after_cover)
            .collect();
        // At the top of the next page, the rows (the repeated header first) start without
        // spacing above them, as in Chrome; a repeated header with collapsed borders starts
        // below the table's top border, which it draws again whole as on the first page (Blink's
        // `TableLayoutAlgorithm` makes room for that border above it).
        if !starts {
            y = if !header.is_empty() && laid.edges.iter().any(Option::is_some) {
                laid.frame[TOP]
            } else {
                top
            };
        }
        let header_start = y;
        place_repeated(
            &header,
            boundary(start, continued.is_some()),
            &mut y,
            &mut children,
        );
        if frame_only && let Some(BreakToken::Table { offset, .. }) = token {
            y += *offset;
        }
        let header_end = y;
        // Monolithic content of the rows on the page before that reaches into this one (an image
        // taller than the page) leaves the rows room below it only, while a repeated header
        // keeps its place at the top, as in Chrome (Blink's `TableLayoutAlgorithm` lays it over
        // the overflow). Where the overflow covers the page, it holds only the header, and the
        // rows go on below the overflow on the next page, at its top where a row goes on after
        // it (see `parallel::split_line`).
        let page = self.overflow_page();
        let covered = overflow > Au::ZERO && page.is_some_and(|page| overflow >= page) && !end_after_cover;
        // Only the end of the table is left after rows that ended with the overflow (see
        // `ends_past` below).
        let ends_after = start == body.len() && continued.is_none();
        let mut next = None;
        if end_after_cover {
            // Chrome lays the end of the table out at the top of the page after the one the
            // overflow covered, where the content after it follows, but draws it below the
            // overflow (see `lift` below).
            y += laid.spacing.1;
        } else if covered {
            // The rows go on at the top of the page after where their cells have a rest there
            // (their bottom padding and border, below the place in the rows where the next page
            // starts), as in Chrome, where the rows then take room on it; else below the
            // overflow (Blink reserves room for it only in a fragment without content).
            let rest = continued.is_some_and(|(_, offset)| {
                let mut end = start;
                while end + 1 < body.len() && self.spans_into(&laid, body[end], body[end + 1]) {
                    end += 1;
                }
                let (Some(&first), Some(&last)) = (body.get(start), body.get(end)) else {
                    return false;
                };
                laid.row_tops[last] + laid.heights[last] - laid.row_tops[first] > offset
            });
            next = Some(BreakToken::Table {
                row: start,
                cells: continued.map_or_else(Vec::new, |(cells, _)| cells.to_vec()),
                offset: continued.map_or(Au::ZERO, |(_, offset)| offset),
                started: true,
                overflow: if rest {
                    Au::ZERO
                } else {
                    overflow - page.unwrap_or(Au::ZERO)
                },
                covered: rest || ends_after,
            });
            continued = None;
        } else if overflow > Au::ZERO {
            // Chrome starts the table below the overflow there and lays a repeated header over
            // it at the top of the page (Blink's `TableLayoutAlgorithm`): the rows start below
            // the header or the overflow, whichever ends lower, with the spacing below the
            // header. A row that ended with the overflow keeps the spacing below it there (Blink
            // resumes it past its end, with no height).
            y = if header.is_empty() {
                y.max(overflow)
            } else {
                y.max(overflow + laid.spacing.1)
            };
            // So does the end of the table after rows that ended with the overflow: it ends
            // twice the spacing below the overflow or the header, as in Chrome.
            if continued.is_none() && (start < body.len() || ends_after) {
                y += laid.spacing.1;
            }
        }
        // The footer needs room for its carry-over rows only on pages where the table breaks.
        let footer_height = rows_height(&footer);
        let closing_footer: Vec<usize> = footer.iter().copied().filter(|&row| !at_break(row)).collect();
        let closing_height = rows_height(&closing_footer);
        // Room for the bottom border and padding of a table that clones them is kept on every
        // page (see `limit`).
        let bottom_frame = if clones { Au::ZERO } else { laid.frame[BOTTOM] };
        // Where the table breaks, a repeated footer needs room below the rows: its rows with the
        // spacing below them (that of a footer without rows too) and, with collapsed borders,
        // the table's bottom border (half the border below the footer), which Blink's
        // `TableLayoutAlgorithm` reserves on every page (`repeated_footer_block_size`).
        let footer_reserve = if footer_repeats {
            let spacing = if footer.is_empty() {
                laid.spacing.1
            } else {
                Au::ZERO
            };
            let border = if collapses(style, &laid.grid) {
                bottom_frame
            } else {
                Au::ZERO
            };
            footer_height + spacing + border
        } else {
            Au::ZERO
        };
        // Whether the table breaks right before its closing footer, which then goes on to the
        // next page, with no footer on this one.
        let mut before_footer = false;
        // Whether the table ends right below its rows on this page (see below).
        let mut ends_below_rows = false;
        let mut index = if covered { body.len() } else { start };
        // The rows placed whole on this page: their index in `body`, the number of children and
        // `y` before them.
        let mut placed_rows: Vec<(usize, usize, Au)> = Vec::new();
        while index < body.len() {
            // Rows joined by cells that span them stay on one page.
            let mut end = index;
            while end + 1 < body.len() && self.spans_into(&laid, body[end], body[end + 1]) {
                end += 1;
            }
            let group = &body[index..=end];
            let height = rows_height(group);
            let first = body[index];
            // The table's bottom border and padding need room only on the page where it ends, its
            // carry-over rows only on pages where it breaks.
            let (frame, footer_room) = if end + 1 == body.len() {
                (bottom_frame, closing_height)
            } else {
                (Au::ZERO, footer_reserve)
            };
            // A repeated footer follows the rows that a break between them leaves on the page.
            let repeats = footer_height > Au::ZERO;
            // Cells of rows that break draw their collapsed borders themselves, as the table
            // draws them on the page: the closing footer follows the last rows where they end.
            let hiding = Hiding {
                header: hides_header,
                footer: hides_footer && (footer_dropped || end + 1 == body.len()),
            };
            // Rows that break need no spacing below them, nor the table's bottom border and
            // padding; if they end on the page after all, they need room for those.
            let split = |top: Au, cells: Option<(&[Option<BreakToken>], Au)>, at_top: bool| {
                let room = limit.map(|limit| limit - caption_height - top - footer_reserve);
                // Nothing lies above the rows on the page but a repeated header with the spacing
                // around it (which Blink leaves out of the place on the page where the rows
                // start): no margin, border or spacing of the table.
                // Rows that go on from the page before start at the top of theirs, below the
                // header (Chrome makes progress with their content there).
                let above = if header.is_empty() {
                    top
                } else if top == header_end && cells.is_some() {
                    Au::ZERO
                } else if top == header_end {
                    header_start - if starts { laid.spacing.1 } else { Au::ZERO }
                } else {
                    top
                };
                let flush = at_top
                    && (self.lenient_top.get()
                        || space.flush
                            && above == Au::ZERO
                            && caption_height == Au::ZERO
                            && (continuing || margin_top == Au::ZERO));
                let rows = (room, at_top, flush);
                let footer = repeats.then_some(footer_reserve);
                let part = self.split_rows(&laid, group, rows, cells, footer, hiding)?;
                // Below the last rows come the closing footer and the table's bottom border and
                // padding (the spacing below them shrinks to fit, see below, unless the table
                // has to end early); below others, the spacing ends with the page.
                let kept = if end + 1 == body.len() && self.ending_early.get() {
                    laid.spacing.1
                } else {
                    Au::ZERO
                };
                let tail = footer_room + frame + kept;
                if part.next.is_none()
                    && let Some(limit) = limit
                    && caption_height + top + part.height + tail > limit
                {
                    let room = limit - caption_height - top - tail;
                    let earlier =
                        self.split_rows(&laid, group, (Some(room), at_top, flush), cells, footer, hiding);
                    if earlier.as_ref().is_none_or(|earlier| earlier.next.is_some()) {
                        return earlier;
                    }
                }
                Some(part)
            };
            // The rest of rows broken on the page before comes first. Where none of their cells
            // gets any further (monolithic content that does not fit below what lies above the
            // rows), that content stays at the top of the page, which the rows start, so that
            // layout progresses, as in Chrome.
            if let Some(cells) = continued.take()
                && let Some(part) = split(y, Some(cells), true).and_then(|part| {
                    let stuck = part
                        .next
                        .as_ref()
                        .is_some_and(|(next, _)| next.as_slice() == cells.0 && !self.lenient_top.get());
                    if !stuck {
                        return Some(part);
                    }
                    self.lenient_top.set(true);
                    let lenient = split(y, Some(cells), true);
                    self.lenient_top.set(false);
                    lenient
                })
            {
                if after_cover {
                    // The rest lies at the top of the page, under a repeated header, which the
                    // rows below start after (in Chrome, the table starts below the rest and
                    // spacing, the header over it, and the rest takes no room in its rows).
                    self.place_part(&laid, group, &part, (header_start, cells.1), &mut children);
                    let rest = header_start + part.height + laid.spacing.1;
                    y = if !header.is_empty() {
                        (header_end - laid.spacing.1).max(rest) + laid.spacing.1 + laid.spacing.1
                    } else if rest_alone {
                        // The table ends with the rest, with the spacing below it only where it
                        // has a header that does not repeat there.
                        rest - if had_header { Au::ZERO } else { laid.spacing.1 }
                    } else {
                        rest
                    };
                } else {
                    self.place_part(&laid, group, &part, (y, cells.1), &mut children);
                    y += part.height + laid.spacing.1;
                }
                if let Some((cells, offset)) = part.next {
                    next = Some(BreakToken::Table {
                        row: index,
                        cells,
                        offset,
                        started: true,
                        overflow: Au::ZERO,
                        covered: false,
                    });
                    break;
                }
                index = end + 1;
                continue;
            }
            // Forced breaks between rows, and before the first below a caption.
            let parts = |row: usize| (laid.grid.rows[row].id, laid.grid.rows[row].group);
            let forced = match index.checked_sub(1) {
                Some(above) if index > start => {
                    self.tree.break_between_rows(parts(body[above]), parts(first))
                }
                None if caption_height > Au::ZERO => self.tree.break_before(laid.grid.rows[first].group),
                _ => BreakBetween::Auto,
            };
            if self.forces_break(forced) && limit.is_some() {
                next = Some(BreakToken::Table {
                    row: index,
                    cells: Vec::new(),
                    offset: Au::ZERO,
                    started: index > 0,
                    overflow: Au::ZERO,
                    covered: false,
                });
                self.forced_break.set(true);
                break;
            }
            // The spacing below the last rows shrinks to fit on the page, as in Chrome, but not
            // to make room for the bottom border and padding of a box around the table.
            let trailing = if end + 1 == body.len() && !early && !frame_breaks {
                laid.spacing.1
            } else {
                Au::ZERO
            };
            // A forced break between block-level boxes in a cell breaks its rows there, as in
            // Chrome, wherever they are on the page.
            if limit.is_some()
                && laid
                    .grid
                    .cells_in(first..=body[end])
                    .any(|cell| self.breaks_inside(laid.grid.cells[cell].id))
            {
                let at_top = y == header_end && space.at_top && caption_height == Au::ZERO;
                if let Some(part) = split(y, None, at_top) {
                    self.place_part(&laid, group, &part, (y, Au::ZERO), &mut children);
                    y += part.height + laid.spacing.1;
                    if let Some((cells, offset)) = part.next {
                        next = Some(BreakToken::Table {
                            row: index,
                            cells,
                            offset,
                            started: true,
                            overflow: Au::ZERO,
                            covered: false,
                        });
                        self.forced_break.set(true);
                        break;
                    }
                    index = end + 1;
                    continue;
                }
            }
            if let Some(limit) = limit
                && caption_height + y + height - trailing + footer_room + frame > limit
            {
                // A caption on the page is progress: below it, the rows are not at the top.
                let at_top = y == header_end && space.at_top && caption_height == Au::ZERO;
                // Rows break inside like their cells do, which continue on the next page, unless
                // they avoid it, or one of their cells does, as in Chrome.
                let avoids = group.iter().any(|&row| {
                    self.avoids_breaks_inside(self.tree.get(laid.grid.rows[row].id).style.break_inside)
                }) || laid.grid.cells_in(first..=body[end]).any(|cell| {
                    self.avoids_breaks_inside(self.tree.get(laid.grid.cells[cell].id).style.break_inside)
                });
                // A row group that avoids breaks inside moves to the next page from its first row,
                // where that row started on this page below other rows, as in Chrome; one that
                // starts the table on the page moves it along.
                let row_group = laid.grid.rows[first].group;
                let group_avoids = self.avoids_breaks_inside(self.tree.get(row_group).style.break_inside);
                let group_start = placed_rows
                    .iter()
                    .find(|&&(row, ..)| laid.grid.rows[body[row]].group == row_group)
                    .copied();
                if group_avoids && !at_top {
                    match group_start {
                        Some((_, _, before))
                            if before == header_end && !space.at_top && caption_height == Au::ZERO =>
                        {
                            return None;
                        }
                        Some((row, count, before)) if before > header_end => {
                            children.truncate(count);
                            y = before;
                            next = Some(BreakToken::Table {
                                row,
                                cells: Vec::new(),
                                offset: Au::ZERO,
                                started: true,
                                overflow: Au::ZERO,
                                covered: false,
                            });
                            break;
                        }
                        _ => {}
                    }
                }
                // A group that starts the page has nowhere better to go: its rows break inside
                // as others do, as in Chrome.
                let group_at_top = group_start.is_some_and(|(_, _, before)| {
                    before == header_end && space.at_top && caption_height == Au::ZERO
                });
                let avoids = avoids || (group_avoids && !at_top && !group_at_top);
                // The last rows fit, but not the spacing and bottom border and padding below
                // them, which go on to the next page (where no footer repeats).
                // So do they where they are rows of a row group that goes on from the page
                // before, as in Chrome: Blink finds no earlier break inside a resumed box (its
                // `MovePastBreakpoint` leaves it out of the early breaks).
                let section = |row: usize| laid.grid.rows[body[row]].group;
                let resumed_group = matches!(token, Some(BreakToken::Table { started: true, .. }))
                    && (continued.is_some()
                        || start == 0
                        || start < body.len() && section(start - 1) == section(start))
                    && section(index) == section(start.min(body.len() - 1));
                if (frame_breaks || resumed_group && !early)
                    && footer.is_empty()
                    && end + 1 == body.len()
                    && caption_height + y + height - laid.spacing.1 + footer_room <= limit
                {
                    place_rows(group, &mut y, &mut children);
                    // What does not fit of the spacing below them comes first on the next page.
                    let end = limit - caption_height;
                    let carried = (y - end).clamp_non_negative();
                    // The table ends at the end of the page, or right below its rows where they
                    // end a row group that goes on from the page before, as in Chrome.
                    y = if frame_breaks {
                        end
                    } else {
                        (y - laid.spacing.1).min(end)
                    };
                    ends_below_rows = !frame_breaks;
                    next = Some(BreakToken::Table {
                        row: body.len() + 1,
                        cells: Vec::new(),
                        offset: carried,
                        started: true,
                        overflow: Au::ZERO,
                        covered: false,
                    });
                    break;
                }
                // The last rows fit with room for the footer as it repeats where the table
                // breaks, but not with the closing footer and the bottom border and padding: the
                // table breaks before the footer, as in Chrome.
                if !footer.is_empty()
                    && end + 1 == body.len()
                    && caption_height + y + height - laid.spacing.1 + footer_reserve <= limit
                {
                    // An item of a column flex container moves to the next page instead.
                    if frame_breaks && !space.at_top {
                        return None;
                    }
                    place_rows(group, &mut y, &mut children);
                    before_footer = true;
                    next = Some(BreakToken::Table {
                        row: body.len(),
                        cells: Vec::new(),
                        offset: Au::ZERO,
                        started: true,
                        overflow: Au::ZERO,
                        covered: false,
                    });
                    break;
                }
                // Rows that avoid breaks inside break all the same where they do not fit even at
                // the top of a page, as in Chrome.
                if (!avoids || at_top)
                    && let Some(part) = split(y, None, at_top)
                {
                    if part.next.is_none() {
                        placed_rows.push((index, children.len(), y));
                    }
                    self.place_part(&laid, group, &part, (y, Au::ZERO), &mut children);
                    y += part.height + laid.spacing.1;
                    if let Some((cells, offset)) = part.next {
                        next = Some(BreakToken::Table {
                            row: index,
                            cells,
                            offset,
                            started: true,
                            overflow: Au::ZERO,
                            covered: false,
                        });
                        break;
                    }
                    index = end + 1;
                    continue;
                }
                // Rows that start the page below only the top padding or border of a box around
                // the table (and the table's captions and header), whose first row breaks only in
                // its bottom padding and border (see `LineBoxes::flush`): the table breaks before
                // them, its box starting on this page, as in Chrome.
                if at_top && !avoids && y == header_end && token.is_none() && resumed.is_none() {
                    next = Some(BreakToken::Table {
                        row: index,
                        cells: Vec::new(),
                        offset: Au::ZERO,
                        started: true,
                        overflow: Au::ZERO,
                        covered: false,
                    });
                    break;
                }
                if !at_top {
                    if y == header_end {
                        // Not even one row fits: the table box starts on the next page, below its
                        // captions if they fit on this one with room for the box's top border,
                        // padding and spacing, as in Chrome.
                        if caption_height == Au::ZERO && resumed.is_none() {
                            return None;
                        }
                        if header.is_empty() || resumed.is_some() {
                            if caption_height + top + laid.spacing.1 > limit && resumed.is_none() {
                                return None;
                            }
                            let rect = Rect::new(margin_left, Au::ZERO, laid.width, caption_height);
                            return Some(captions_alone(above, rect, index, margin_top_here));
                        }
                        // With a repeated header, the table box starts with it (and the footer)
                        // below its captions if they fit, the spacing around the footer shrunk,
                        // or moves on with the captions, as in Chrome.
                        let footer_end = y - laid.spacing.1 + footer_height - laid.spacing.1;
                        if caption_height + footer_end.max(y - laid.spacing.1) > limit {
                            return None;
                        }
                    }
                    // Rows between which the break is avoided move to the next page together,
                    // with as many rows above them as avoid the break before them, while a row
                    // stays on this page, as in Chrome (CSS Fragmentation 3, §4.4). Where none
                    // would, a table that starts on the page moves to the next one whole.
                    let mut cut = (index, children.len(), y);
                    for &(row, count, before) in placed_rows.iter().rev() {
                        // (Only rows placed whole right above, not rows that broke.)
                        let mut last = row;
                        while last + 1 < body.len() && self.spans_into(&laid, body[last], body[last + 1]) {
                            last += 1;
                        }
                        if last + 1 != cut.0
                            || !self.avoids_break(
                                self.tree
                                    .break_between_rows(parts(body[last]), parts(body[cut.0])),
                            )
                        {
                            break;
                        }
                        cut = (row, count, before);
                    }
                    if cut.0 != index {
                        if placed_rows.first().is_some_and(|&(row, ..)| row == cut.0) {
                            if token.is_none() && !space.at_top && caption_height == Au::ZERO {
                                return None;
                            }
                        } else {
                            children.truncate(cut.1);
                            y = cut.2;
                            index = cut.0;
                        }
                    }
                    next = Some(BreakToken::Table {
                        row: index,
                        cells: Vec::new(),
                        offset: Au::ZERO,
                        started: true,
                        overflow: Au::ZERO,
                        covered: false,
                    });
                    break;
                }
            }
            placed_rows.push((index, children.len(), y));
            place_rows(group, &mut y, &mut children);
            index = end + 1;
        }
        // Rows that end the table with monolithic content reaching past the end of the page:
        // Chrome breaks the table after them, as tall as they reach, and its end (the spacing
        // and bottom border and padding) goes on below the overflow on the next page, under a
        // header repeated over the overflow (Blink's `TableLayoutAlgorithm` breaks before the
        // end of a table box that does not fit). Their cells without such content end with the
        // page. (Without a header, a table whose overflow covers the next page ends with it.)
        let rows_end = y - laid.spacing.1;
        let ends_past = next.is_none()
            && !frame_only
            && !early
            && !clones
            && !covered
            && limit.is_some_and(|limit| {
                let over = rows_end - (limit - caption_height);
                over > Au::ZERO
                    && self
                        .overflow_page()
                        .is_some_and(|page| !header.is_empty() || over < page)
            });
        let mut reach = Au::ZERO;
        if ends_past && let Some(limit) = limit {
            let end = limit - caption_height;
            end_cells_with_page(&mut children, end - footer_height);
            next = Some(BreakToken::Table {
                row: body.len(),
                cells: Vec::new(),
                offset: Au::ZERO,
                started: true,
                overflow: rows_end - end,
                covered: false,
            });
            // A repeated footer goes at the end of the page (see below).
            y = if footer.is_empty() {
                rows_end
            } else {
                (end - footer_height).max(header_end)
            };
            reach = rows_end;
            ends_below_rows = true;
        }
        // The footer closes every page of the table, as in browsers; its carry-over rows only the
        // pages where the table breaks.
        let broken = next.is_some();
        // Monolithic content of the rows that reaches past the end of the page moves the rows on
        // the next page below it (see above); the table reaches as far as the rows, a repeated
        // footer at the end of the page, and their cells without such content end with the
        // page, as in Chrome. In columns, the table grows to hold it and the next column starts
        // at the top, as in Chrome.
        if broken
            && !covered
            && !ends_past
            && self.columns.get() == 0
            && let Some(limit) = limit
            && let Some(BreakToken::Table { overflow, .. }) = &mut next
        {
            let end = limit - caption_height;
            let rows_end = y - laid.spacing.1;
            if rows_end > end {
                *overflow = rows_end - end;
                y = (end - footer_height).max(header_end);
                reach = rows_end;
                end_cells_with_page(&mut children, end - footer_height);
            }
        }
        let footer_position = match &next {
            Some(BreakToken::Table { row, cells, .. }) => boundary(*row, !cells.is_empty()),
            _ => end,
        };
        // The repeated footer goes up into the spacing above it where it does not fit below with
        // the spacing below it, as in Chrome; below a header without rows, it follows the header
        // right away unless it fits with twice that spacing.
        if broken
            && !frame_only
            && !before_footer
            && !footer.is_empty()
            && let Some(limit) = limit
        {
            let latest = limit - caption_height - footer_height;
            y = if clones {
                // Chrome puts a repeated footer of a table that clones its bottom border and
                // padding as far above the end of the rows' room as that border and padding
                // reach.
                latest - laid.frame[BOTTOM]
            } else if covered {
                // On a page that monolithic content of the rows covers, the footer goes at the
                // end of the page, below the overflow it would follow, as in Chrome (WPT css-page
                // monolithic-overflow-017).
                latest
            } else if y == header_end && !header.is_empty() {
                if y + laid.spacing.1 > latest {
                    y - laid.spacing.1
                } else {
                    y
                }
            } else {
                y.min(latest).max(y - laid.spacing.1)
            };
        }
        // Where the table breaks right before its closing footer, which goes on to the next
        // page, the footer's rows that show where it breaks show here.
        let carried_footer: Vec<usize> = footer.iter().copied().filter(|&row| at_break(row)).collect();
        place_repeated(
            if frame_only {
                &[]
            } else if before_footer {
                &carried_footer
            } else if broken {
                &footer
            } else {
                &closing_footer
            },
            footer_position,
            &mut y,
            &mut children,
        );
        // The spacing below the last rows shrinks to what is left on the page above the bottom
        // border and padding, as in Chrome.
        if !broken
            && !early
            && let Some(limit) = limit
        {
            let over = caption_height + y + bottom_frame - limit;
            if over > Au::ZERO {
                y -= over.min(laid.spacing.1);
            }
        }
        // A table that breaks before rows that go on to the next page whole reaches down to the
        // end of this one, as in Chrome, where no footer repeats at its end; one whose rows
        // break at the end of the page ends there, without the spacing below them.
        let y = match limit {
            Some(limit)
                if broken
                    && !early
                    && !ends_below_rows
                    && footer.is_empty()
                    && !laid.style.clones_decorations() =>
            {
                let end = limit - caption_height;
                if y > end && y - laid.spacing.1 <= end {
                    end
                } else {
                    y.max(end)
                }
            }
            _ => y,
        };
        let height = match (broken, clones) {
            // A part that clones the bottom border and padding ends with them at the end of the
            // page, or right below its rows, without spacing, where those reach further.
            (true, true) => {
                let end = limit.map_or(Au::ZERO, |limit| limit - caption_height);
                (y - laid.spacing.1).max(top).max(end) + laid.frame[BOTTOM]
            }
            (true, false) => y.max(reach),
            (false, _) => y + laid.frame[BOTTOM],
        };
        // Collapsed borders go over the cells: those of each row placed whole, and the bottom
        // line of a row that the next does not follow.
        let placed = placed.into_inner();
        let without = laid.without(Hiding {
            header: hides_header,
            footer: hides_footer && (footer_dropped || !broken && !frame_only && !before_footer),
        });
        let (borders, bottoms) = without.map_or((&laid.borders, &laid.bottoms), |without| {
            (&without.borders, &without.bottoms)
        });
        // A row is followed by the next on the page also where rows left out lie between them.
        let left_out = |rows: Range<usize>| {
            rows.into_iter()
                .all(|row| without.is_some_and(|without| without.hidden[row]))
        };
        for (position, &(row, top)) in placed.iter().enumerate() {
            let followed = placed.get(position + 1).is_some_and(|&(next, next_top)| {
                next > row && left_out(row + 1..next) && next_top == top + laid.heights[row]
            });
            let bottoms = if followed { None } else { bottoms.get(row) };
            for piece in borders.get(row).into_iter().chain(bottoms).flatten() {
                let mut piece = piece.clone();
                piece.rect.y += top;
                children.push(piece);
            }
        }
        children.extend(self.part_outlines(&laid, &placed));
        if !laid.cell_styles.is_empty() {
            for child in &mut children {
                if let FragmentKind::Box {
                    style,
                    node: Some(node),
                    ..
                } = &mut child.kind
                    && let Some(own) = laid.cell_styles.get(node)
                {
                    *style = Arc::clone(own);
                }
            }
        }
        let children = self.nest_positioned_rows(&laid, children);

        let mut table = Fragment {
            rect: Rect::new(Au::ZERO, caption_height, laid.width, height),
            kind: FragmentKind::Box {
                style: Arc::clone(&laid.style),
                node: layout_box.node,
                sides: VisibleSides {
                    top: starts || clones,
                    bottom: !broken || clones,
                    ..VisibleSides::ALL
                },
            },
            children: children.into(),
        };
        // Monolithic content of the rows on the page before that reaches into this one: Chrome
        // places the table box below it, with a repeated header laid over it at the top of the
        // page (Blink's `TableLayoutAlgorithm` places the header that far above the box), so
        // that the box's background does not cover it. Where the overflow covered the page
        // before and only the end of the table is left, that end takes its own height on the
        // page, where the content after it follows, but is drawn below the overflow, as in
        // Chrome; or, with no height, the content after it follows below the overflow.
        let lift = if overflow > Au::ZERO && self.overflow_page().is_some() {
            overflow
        } else {
            Au::ZERO
        };
        let mut room = height;
        if lift > Au::ZERO {
            table.rect.y += lift;
            if end_after_cover {
                if height == Au::ZERO {
                    room = lift;
                }
            } else {
                table.rect.height = (height - lift).clamp_non_negative();
                for child in &mut table.children {
                    child.rect.y -= lift;
                }
            }
        }
        if starts {
            align_marker(&mut above, &mut table, self.writing_mode());
        }
        let mut below = Vec::new();
        let mut total = caption_height + room;
        let (mut next, mut broken) = (next, broken);
        if !broken {
            // The captions below the table break like blocks too, as in Chrome.
            let room = caption_limit.map(|limit| limit - total);
            let (fragments, end, token) =
                match self.side_captions(id, laid.width, CaptionSide::Bottom, (room, false), None) {
                    CaptionsPart::Done(fragments, end) => (fragments, end, None),
                    CaptionsPart::Broken(fragments, token) => {
                        (fragments, room.unwrap_or(Au::ZERO), Some(token))
                    }
                    CaptionsPart::Moves => (Vec::new(), Au::ZERO, None),
                };
            below.extend(fragments.into_iter().map(|mut fragment| {
                fragment.rect.y += total;
                fragment
            }));
            total = below
                .iter()
                .map(|fragment| fragment.rect.y + fragment.rect.height)
                .fold(total + end, Au::max);
            if token.is_some() {
                next = token;
                broken = true;
            }
        }
        let fragment = if above.is_empty() && below.is_empty() && lift == Au::ZERO {
            let mut table = table;
            table.rect.x = margin_left;
            table
        } else {
            // The captions and the table form the table wrapper box.
            let mut wrapper_children = above;
            wrapper_children.push(table);
            wrapper_children.extend(below);
            wrapper(
                Rect::new(margin_left, Au::ZERO, laid.width, total),
                wrapper_children,
                &self.tree.get(id).style,
            )
        };
        Some(Outcome {
            fragment,
            token: next,
            margin_bottom: if broken { Au::ZERO } else { margin_bottom },
            collapsed_top: Margin::of(margin_top_here),
            collapsed_bottom: Margin::of(if broken { Au::ZERO } else { margin_bottom }),
            through: false,
            passed_breaks: (BreakBetween::Auto, BreakBetween::Auto),
        })
    }

    /// Lays out the captions above table `id` (`width` wide) that fit in `limit`, from where
    /// `resumed` tells (the caption, among all of the table's, and the token inside it), the
    /// first of them at the top of the page if `at_top`: they break like blocks, as in Chrome.
    fn top_captions(
        &self,
        id: BoxId,
        width: Au,
        (limit, at_top): (Option<Au>, bool),
        resumed: CaptionResume<'_>,
    ) -> CaptionsPart {
        self.side_captions(id, width, CaptionSide::Top, (limit, at_top), resumed)
    }

    /// Lays out the captions of table `id` on `side` as [`Layouter::top_captions`] does; those
    /// below the table never move it (`Moves`), but break before the first that does not fit.
    fn side_captions(
        &self,
        id: BoxId,
        width: Au,
        side: CaptionSide,
        (limit, at_top): (Option<Au>, bool),
        resumed: CaptionResume<'_>,
    ) -> CaptionsPart {
        let captions: &[BoxId] = match &self.tree.get(id).kind {
            BoxKind::Table { captions, .. } => captions,
            _ => &[],
        };
        let first = resumed.map_or(0, |(index, _)| index);
        let mut fragments = Vec::new();
        let mut y = Au::ZERO;
        for (index, &caption) in captions.iter().enumerate().skip(first) {
            if self.tree.get(caption).style.caption_side != side {
                continue;
            }
            let token = resumed.and_then(|(at, token)| if at == index { token } else { None });
            let space = Space {
                remaining: limit.map(|limit| limit - y),
                at_top: at_top && fragments.is_empty(),
                ..Space::unfragmented(width)
            };
            let Some(outcome) = self.block_level(caption, space, token) else {
                if fragments.is_empty() && resumed.is_none() && side == CaptionSide::Top {
                    return CaptionsPart::Moves;
                }
                return CaptionsPart::Broken(fragments, BreakToken::Captions { index, inner: None });
            };
            let mut fragment = outcome.fragment;
            fragment.rect.y = y + if token.is_some() {
                Au::ZERO
            } else {
                outcome.collapsed_top.resolve()
            };
            y = fragment.rect.y + fragment.rect.height;
            fragments.push(fragment);
            if let Some(inner) = outcome.token {
                return CaptionsPart::Broken(
                    fragments,
                    BreakToken::Captions {
                        index,
                        inner: Some(Box::new(inner)),
                    },
                );
            }
            y += outcome.collapsed_bottom.resolve();
        }
        CaptionsPart::Done(fragments, y)
    }

    /// A table without rows: its captions around a box as tall as its height asks, which
    /// breaks at the end of the page like a box taller than its content (the rest of its
    /// height continues on the next), as in Chrome. `at` holds the token it continues at, the
    /// room left on the page and whether it starts the page; `margins` its left, top and
    /// bottom margins.
    fn empty_table(
        &self,
        id: BoxId,
        laid: &Laid,
        (token, resumed, limit, at_top): (Option<&BreakToken>, CaptionResume<'_>, Option<Au>, bool),
        (margin_left, margin_top, margin_bottom): (Au, Au, Au),
    ) -> Option<Outcome> {
        let layout_box = self.tree.get(id);
        let captions: &[BoxId] = match &layout_box.kind {
            BoxKind::Table { captions, .. } => captions,
            _ => &[],
        };
        let whole = laid.frame[TOP] + laid.empty_height + laid.frame[BOTTOM];
        let (continuing, rest) = match token {
            Some(BreakToken::Rest { rest }) => (true, *rest),
            _ => (false, whole),
        };
        // The captions above it on its first page, below it on its last.
        let place_captions = |side: CaptionSide, y: &mut Au, out: &mut Vec<Fragment>| {
            for &caption in captions {
                if self.tree.get(caption).style.caption_side == side
                    && let Some(outcome) = self.block_level(caption, Space::unfragmented(laid.width), None)
                {
                    let mut fragment = outcome.fragment;
                    fragment.rect.y = *y + outcome.collapsed_top.resolve();
                    *y = fragment.rect.y + fragment.rect.height + outcome.collapsed_bottom.resolve();
                    out.push(fragment);
                }
            }
        };
        let mut children = Vec::new();
        let mut y = Au::ZERO;
        if !continuing {
            match self.top_captions(id, laid.width, (limit, at_top), resumed) {
                CaptionsPart::Done(fragments, height) => {
                    children = fragments;
                    y = height;
                }
                CaptionsPart::Broken(fragments, token) => {
                    return Some(captions_broken(
                        fragments,
                        (margin_left, laid.width, limit.unwrap_or(Au::ZERO)),
                        token,
                        margin_top,
                    ));
                }
                CaptionsPart::Moves => return None,
            }
        }
        // A page with room for none of it still takes some, where nothing else is on it.
        let placed = match limit.map(|limit| limit - y) {
            Some(room) if rest > room => room.max(if at_top { Au::EPSILON } else { Au::ZERO }),
            _ => rest,
        };
        let broken = placed < rest;
        // Only the bottom captions are left (see below): the box ended on the page before.
        if !(continuing && rest == Au::ZERO) {
            children.push(Fragment {
                rect: Rect::new(Au::ZERO, y, laid.width, placed),
                kind: FragmentKind::Box {
                    style: Arc::clone(&laid.style),
                    node: layout_box.node,
                    sides: VisibleSides {
                        top: !continuing,
                        bottom: !broken,
                        ..VisibleSides::ALL
                    },
                },
                children: Children::new(),
            });
        }
        y += placed;
        // Bottom captions that do not fit below the box where it ends on the page go on to the
        // next, as in Chrome (the box with them where nothing of it is on the page).
        let mut below = Vec::new();
        let mut end = y;
        if !broken {
            place_captions(CaptionSide::Bottom, &mut end, &mut below);
        }
        let captions_move =
            !broken && placed > Au::ZERO && !below.is_empty() && limit.is_some_and(|limit| end > limit);
        if !captions_move {
            children.extend(below);
            y = end;
        }
        let broken = broken || captions_move;
        let fragment = if children.len() == 1 {
            let mut table = children.remove(0);
            table.rect.x = margin_left;
            table
        } else {
            // The captions and the table form the table wrapper box.
            wrapper(
                Rect::new(margin_left, Au::ZERO, laid.width, y),
                children,
                &layout_box.style,
            )
        };
        Some(Outcome {
            fragment,
            token: broken.then(|| BreakToken::Rest { rest: rest - placed }),
            margin_bottom: if broken { Au::ZERO } else { margin_bottom },
            collapsed_top: Margin::of(if continuing { Au::ZERO } else { margin_top }),
            collapsed_bottom: Margin::of(if broken { Au::ZERO } else { margin_bottom }),
            through: false,
            passed_breaks: (BreakBetween::Auto, BreakBetween::Auto),
        })
    }

    /// The outlines of the rows and row groups with one among the rows `placed` (with their
    /// tops), which have no boxes of their own: boxes over the rows of the part on the page.
    fn part_outlines(&self, laid: &Laid, placed: &[(usize, Au)]) -> Vec<Fragment> {
        let mut parts: Vec<(BoxId, Au, Au)> = Vec::new();
        for &(row, top) in placed {
            let grid_row = laid.grid.rows[row];
            let bottom = top + laid.heights[row];
            for id in [grid_row.group, grid_row.id] {
                // (Anonymous rows keep the initial width with the initial style `none`.)
                let style = &self.tree.get(id).style;
                if style.outline_width <= 0.0
                    || matches!(style.outline_style, fagus_style::values::OutlineStyle::Line(line)
                        if !line.is_visible())
                {
                    continue;
                }
                match parts.iter_mut().find(|(part, _, _)| *part == id) {
                    Some(part) => {
                        part.1 = part.1.min(top);
                        part.2 = part.2.max(bottom);
                    }
                    None => parts.push((id, top, bottom)),
                }
            }
        }
        parts
            .into_iter()
            .map(|(id, top, bottom)| {
                let style = &self.tree.get(id).style;
                let outline = ComputedStyle::from(ComputedValues {
                    color: style.color,
                    outline_width: style.outline_width,
                    outline_style: style.outline_style,
                    outline_color: style.outline_color,
                    outline_offset: style.outline_offset,
                    visibility: style.visibility,
                    ..ComputedValues::default()
                });
                Fragment {
                    rect: Rect::new(laid.grid_left, top, laid.grid_width, bottom - top),
                    kind: FragmentKind::Box {
                        style: Arc::new(outline),
                        node: None,
                        sides: VisibleSides::ALL,
                    },
                    children: Children::new(),
                }
            })
            .collect()
    }

    /// The part of the rows `group` (joined by cells that span them) that fits in `room`, the
    /// cells broken where their content breaks and continuing at `continued` (a token per cell
    /// of the rows, `None` for cells that are done, and the place in the rows where the page
    /// starts), as in Chrome. `at_top` tells whether the rows start the page, `flush` whether
    /// nothing at all lies above them there (see [`Space::flush`]). `None` if the rows move to
    /// the next page, or cannot break. With `footer` (a footer repeats below the part, taking
    /// that room), a part that breaks between the rows ends with them. `hiding` tells which rows
    /// of the header or footer the page leaves out (see [`Without`]).
    fn split_rows(
        &self,
        laid: &Laid,
        group: &[usize],
        (room, at_top, flush): (Option<Au>, bool, bool),
        continued: Option<(&[Option<BreakToken>], Au)>,
        footer: Option<Au>,
        hiding: Hiding,
    ) -> Option<Portion> {
        let ends_at_rows = footer.is_some();
        // Rows that go on from the page before in a table laid out without a page (in a cell
        // laid out whole below its last break, see `Cells::lay`) go on where their cells broke,
        // with all the room they need, rather than from their start again.
        let room = match room {
            Some(room) => room,
            None if continued.is_some() => Au::MAX / 4,
            None => return None,
        };
        let first = *group.first()?;
        let last = *group.last()?;
        let row_top = |row: usize| laid.row_tops[row] - laid.row_tops[first];
        let indexes: Vec<usize> = laid.grid.cells_in(first..=last).collect();
        if indexes.is_empty() {
            return None;
        }
        let half = |edge: &Edge| Au::from_px(edge.used_width() / 2.0);
        // A row that grew past the height it had on the page where the table started (laid out
        // in another width there) breaks as tall as that height, its cells growing to their
        // content on each page, as in Chrome (see `Laid::grown`).
        let full = row_top(last) + laid.heights[last];
        let shrink = match group {
            [row] if continued.is_some() || full > room => laid.grown.get(*row).copied().unwrap_or(Au::ZERO),
            _ => Au::ZERO,
        };
        let shrinks: Vec<Au> = indexes
            .iter()
            .map(|&index| {
                if laid.grid.cells[index].rowspan == 1 {
                    shrink
                } else {
                    Au::ZERO
                }
            })
            .collect();
        let pieces: Vec<Piece> = indexes
            .iter()
            .zip(&shrinks)
            .map(|(&index, &shrink)| {
                let slot = &laid.grid.cells[index];
                let outset = laid.edges[index].as_ref().map_or((Au::ZERO, Au::ZERO), |edges| {
                    (half(&edges[TOP]), half(&edges[BOTTOM]))
                });
                Piece {
                    offset: row_top(slot.row),
                    height: laid.cells[index].rect.height - outset.0 - outset.1 - shrink,
                    margin_bottom: Au::ZERO,
                    stretched: true,
                    clones: self.tree.get(slot.id).style.clones_decorations(),
                    grows: slot.rowspan == 1,
                    outset,
                    growth: crate::parallel::Growth::None,
                }
            })
            .collect();
        let height = full - shrink;
        let mut cells = Cells {
            layouter: self,
            laid,
            indexes: &indexes,
            flush,
            whole: Vec::new(),
            shrinks: &shrinks,
            forced: Vec::new(),
            footer: footer.unwrap_or(Au::ZERO),
        };
        let mut portion = split_line(&pieces, height, continued, room, at_top, &mut cells)?;
        // Rows whose cells all end on the page, the monolithic content of each reaching past
        // the end of the rows, end there with it, as in Chrome (Blink's `FinishFragmentation`
        // lets the fragment take the height of the content that overflows the page). Where a
        // cell has more below its content (bottom padding), that goes on below the overflow.
        if continued.is_none()
            && portion.fragments.len() == indexes.len()
            && portion
                .next
                .as_ref()
                .is_some_and(|(tokens, _)| tokens.iter().all(Option::is_none))
            && portion.fragments.iter().all(|(_, fragment)| {
                crate::monolithic::content_bottom(fragment, Au::ZERO).is_some_and(|reach| reach >= height)
            })
        {
            portion.next = None;
            portion.height = height;
        }
        // Where the rows break between two of them (in the spacing between them, or right
        // above the lower one), the spacing goes to the next page, above the row there, as in
        // Chrome (the cells that span them start it).
        let start = continued.map_or(Au::ZERO, |(_, offset)| offset);
        let mut rows_end = portion.rows_end;
        if let Some((_, offset)) = &mut portion.next
            && let Some(between) = group[1..]
                .iter()
                .map(|&row| row_top(row) - laid.spacing.1)
                .find(|&between| (between..=between + laid.spacing.1).contains(offset))
            && between > start
        {
            *offset = between;
            rows_end = rows_end.or(Some(between - start));
        }
        // Where the rows break between them and a footer repeats, the part ends with the rows
        // left on the page, and so do the cells that span into the next page, as in Chrome.
        let end = rows_end.filter(|_| ends_at_rows && portion.next.is_some());
        if let Some(end) = end {
            portion.height = end;
        }
        // The closing footer follows the rows only where they end on the page.
        let without = laid.without(Hiding {
            footer: hiding.footer && portion.next.is_none(),
            ..hiding
        });
        for (position, fragment) in &mut portion.fragments {
            let index = indexes[*position];
            if let Some(without) = without
                && let (Some(drawn), Some(edges)) = (&laid.edges[index], &without.edges[index])
            {
                redraw_collapsed(fragment, drawn, edges);
            }
            fragment.rect.x = laid.cells[index].rect.x;
            if let Some(end) = end
                && fragment.rect.y + fragment.rect.height > end + pieces[*position].outset.1
            {
                fragment.rect.height =
                    (end + pieces[*position].outset.1 - fragment.rect.y).clamp_non_negative();
            }
        }
        Some(portion)
    }

    /// Adds the cells of a part of the rows `group`, placed at `y` and going on from `offset` in
    /// the rows (the part of them on the pages before), with the backgrounds of their rows and
    /// row groups, which are drawn for the rows whole and sliced (CSS Fragmentation 3, §5.4).
    fn place_part(
        &self,
        laid: &Laid,
        group: &[usize],
        part: &Portion,
        (y, offset): (Au, Au),
        children: &mut Vec<Fragment>,
    ) {
        let (Some(&first), Some(&last)) = (group.first(), group.last()) else {
            return;
        };
        let indexes: Vec<usize> = laid.grid.cells_in(first..=last).collect();
        let mut shadowed = None;
        for (position, cell) in &part.fragments {
            let Some(&index) = indexes.get(*position) else {
                continue;
            };
            let mut fragment = cell.clone();
            fragment.rect.y += y;
            let row = laid.grid.cells[index].row;
            let row_top = y + laid.row_tops[row] - laid.row_tops[first] - offset;
            let start = children.len();
            if shadowed != Some(row) {
                shadowed = Some(row);
                row_shadow(laid, row, row_top, children);
            }
            // (A cell that only draws its collapsed borders has no backgrounds of its row.)
            if !part.bare.contains(position) {
                part_backgrounds(laid, index, fragment.rect, row_top, children);
            }
            children.push(fragment);
            laid.placed_cells.borrow_mut().push((start..children.len(), row));
        }
    }

    /// The height of a page, for monolithic content of a table's rows that reaches past the end
    /// of one: Chrome lays the pages end to end in the flow of the document (in horizontal
    /// writing modes), so that such content reaches into the next pages. `None` in columns.
    fn overflow_page(&self) -> Option<Au> {
        (self.columns.get() == 0 && !self.writing_mode().is_vertical())
            .then(|| self.initial_containing_block.get().1)
    }

    /// Whether a cell spans from row `row` into row `next`.
    fn spans_into(&self, laid: &Laid, row: usize, next: usize) -> bool {
        laid.reach.get(row).is_some_and(|reach| *reach > row.max(next))
    }

    /// Adds the cells of row `row` with the backgrounds of their row and row group, with the
    /// row's top edge at `y`. Backgrounds are drawn per cell, so a cell that spans rows has the
    /// background of the row it starts in (CSS Tables 3, §6.1).
    fn place_row(&self, laid: &Laid, row: usize, y: Au, children: &mut Vec<Fragment>) {
        self.place_cells(laid, row, y, children, |index| laid.cells[index].clone());
    }

    /// Adds the cells of row `row` as `place_row` does, each from `cell` by its index.
    fn place_cells(
        &self,
        laid: &Laid,
        row: usize,
        y: Au,
        children: &mut Vec<Fragment>,
        cell: impl Fn(usize) -> Fragment,
    ) {
        let mut start = children.len();
        row_shadow(laid, row, y, children);
        for index in laid.grid.cells_in(row..=row) {
            let mut fragment = cell(index);
            fragment.rect.y += y;
            part_backgrounds(laid, index, fragment.rect, y, children);
            children.push(fragment);
            laid.placed_cells.borrow_mut().push((start..children.len(), row));
            start = children.len();
        }
    }

    /// The children of a table's fragment with the cells of positioned rows and row groups put
    /// in fragments of those boxes,
    /// as Chrome gives every row and row group a fragment: such a fragment is the containing
    /// block of the boxes, and moves its cells with its relative offset. It spans the columns
    /// and its cells on the page, and paints nothing itself (the cells draw the backgrounds of
    /// their parts).
    fn nest_positioned_rows(&self, laid: &Laid, children: Vec<Fragment>) -> Vec<Fragment> {
        let placed = laid.placed_cells.take();
        let positioned = |id: BoxId| self.tree.get(id).style.is_positioned();
        if placed.is_empty()
            || !*laid.positioned_parts.get_or_init(|| {
                laid.grid
                    .rows
                    .iter()
                    .any(|row| positioned(row.id) || positioned(row.group))
            })
        {
            return children;
        }
        let mut rows = vec![None; children.len()];
        for (range, row) in placed {
            for slot in rows.get_mut(range).into_iter().flatten() {
                *slot = Some(row);
            }
        }
        // The children with those of each positioned group and row gathered, in order.
        let mut nested: Vec<Nested> = Vec::new();
        for (child, row) in children.into_iter().zip(rows) {
            let Some(grid_row) = row.map(|row| laid.grid.rows[row]) else {
                nested.push(Nested::Plain(child));
                continue;
            };
            let group = Some(grid_row.group).filter(|&group| positioned(group));
            let own = Some(grid_row.id).filter(|&row| positioned(row));
            let mut list = &mut nested;
            for id in [group, own].into_iter().flatten() {
                if !matches!(list.last(), Some(Nested::Part(open, _)) if *open == id) {
                    list.push(Nested::Part(id, Vec::new()));
                }
                let Some(Nested::Part(_, inner)) = list.last_mut() else {
                    unreachable!("a part was just pushed")
                };
                list = inner;
            }
            list.push(Nested::Plain(child));
        }
        nested
            .into_iter()
            .map(|entry| entry.into_fragment(self, laid))
            .collect()
    }
}

/// A child of a table's fragment, or the children of a positioned row or row group gathered
/// in a fragment of their own (see [`Layouter::nest_positioned_rows`]).
enum Nested {
    Plain(Fragment),
    Part(BoxId, Vec<Nested>),
}

impl Nested {
    /// The fragment, in the coordinates of the table's fragment.
    fn into_fragment(self, layouter: &Layouter<'_>, laid: &Laid) -> Fragment {
        let (id, entries) = match self {
            Self::Plain(fragment) => return fragment,
            Self::Part(id, entries) => (id, entries),
        };
        let mut children: Vec<Fragment> = entries
            .into_iter()
            .map(|entry| entry.into_fragment(layouter, laid))
            .collect();
        let top = children
            .iter()
            .map(|child| child.rect.y)
            .min()
            .unwrap_or(Au::ZERO);
        let bottom = children
            .iter()
            .map(|child| child.rect.y + child.rect.height)
            .max()
            .unwrap_or(top);
        for child in &mut children {
            child.rect.x -= laid.grid_left;
            child.rect.y -= top;
        }
        Fragment {
            rect: Rect::new(laid.grid_left, top, laid.grid_width, bottom - top),
            kind: match (laid.part_styles.get(&id), layouter.tree.get(id)) {
                (Some(style), layout_box) => FragmentKind::Box {
                    style: Arc::clone(style),
                    node: layout_box.node,
                    sides: VisibleSides::ALL,
                },
                (None, layout_box) => layout_box.fragment_kind(VisibleSides::ALL),
            },
            children: children.into(),
        }
    }
}

/// The style of a relatively positioned row group, row or cell with `style` whose `top` or
/// `bottom` is a percentage, in the part around it with `parent`: the percentage of the `height`
/// the parent specifies as a length, or `auto` where it specifies none, as Blink resolves them
/// (the heights of table parts depend on their content). `None` where nothing changes.
fn resolved_part_insets(style: &ComputedStyle, parent: &ComputedStyle) -> Option<ComputedStyle> {
    use fagus_style::values::LengthPercentageAuto;
    let percentage = |inset: &LengthPercentageAuto| matches!(inset, LengthPercentageAuto::Length(length) if length.fixed().is_none());
    if style.position != Position::Relative
        || !(percentage(&style.inset.top) || percentage(&style.inset.bottom))
    {
        return None;
    }
    let basis = match &parent.height {
        Size::Length(length) => length.fixed(),
        _ => None,
    };
    let mut own = style.clone();
    let values = own.values_mut();
    for inset in [&mut values.inset.top, &mut values.inset.bottom] {
        if let LengthPercentageAuto::Length(length) = inset
            && length.fixed().is_none()
        {
            *inset = basis.map_or(LengthPercentageAuto::Auto, |basis| {
                LengthPercentageAuto::Length(LengthPercentage::Px(length.resolve(basis)))
            });
        }
    }
    Some(own)
}

/// Adds the outer shadows of row `row`, placed with its top at `row_top`, before its cells:
/// Chrome paints the shadow of each row around the row's box (from the first column to the
/// last), after the cells of the rows before it.
fn row_shadow(laid: &Laid, row: usize, row_top: Au, children: &mut Vec<Fragment>) {
    let Some(Some(style)) = laid.shadows.get(row) else {
        return;
    };
    children.push(Fragment {
        rect: Rect::new(laid.grid_left, row_top, laid.grid_width, laid.heights[row]),
        kind: FragmentKind::Box {
            style: Arc::clone(style),
            node: None,
            sides: VisibleSides::ALL,
        },
        children: Children::new(),
    });
}

/// Adds the backgrounds of the column groups, columns, row group and row of cell `index`, in
/// that order, drawn in the cell's rectangle `cell` (CSS 2.1, §17.5.1); `row_top` is where the
/// top of the cell's first row is placed. A cell that spans columns or rows has the backgrounds
/// of the column and the row it starts in, as in Chrome. The image of a background is
/// positioned in the box of its part: a column runs from the table's first row to its last, a
/// row or row group along all columns.
fn part_backgrounds(laid: &Laid, index: usize, cell: Rect, row_top: Au, children: &mut Vec<Fragment>) {
    let slot = &laid.grid.cells[index];
    // With collapsed borders, the cell's box spans all of its borders; the backgrounds reach
    // halfway into them, as the cell's own does (Chrome 157, measured: those of a row stop
    // halfway into the table's outer border).
    let cell = match laid.edges.get(index).and_then(Option::as_ref) {
        Some(edges) => {
            let half = |side: usize| Au::from_px(edges[side].used_width() / 2.0);
            Rect::new(
                cell.x + half(LEFT),
                cell.y + half(TOP),
                (cell.width - half(LEFT) - half(RIGHT)).clamp_non_negative(),
                (cell.height - half(TOP) - half(BOTTOM)).clamp_non_negative(),
            )
        }
        None => cell,
    };
    let column = if laid.grid.rtl {
        laid.grid.columns.saturating_sub(slot.column + slot.colspan)
    } else {
        slot.column
    };
    // Where the first row is placed.
    let top = row_top - laid.row_tops[slot.row];
    let bottom = |row: usize| laid.row_tops[row] + laid.heights[row];
    for part in &laid.columns {
        if (part.start..part.start + part.span).contains(&column) {
            let height = laid.heights.len().checked_sub(1).map_or(Au::ZERO, bottom);
            let rect = || Rect::new(part.x, top, part.width, height);
            children.push(part_background(&part.style, cell, rect));
        }
    }
    let grid_row = laid.grid.rows[slot.row];
    for id in [grid_row.group, grid_row.id] {
        if let Some((_, style)) = laid.backgrounds.iter().find(|(background, _)| *background == id) {
            let rect = || {
                // A row group runs from its first row to its last.
                let mut rows = (0..laid.grid.rows.len())
                    .filter(|&row| laid.grid.rows[row].id == id || laid.grid.rows[row].group == id);
                let first = rows.next().unwrap_or(slot.row);
                let last = rows.next_back().unwrap_or(first);
                let y = top + laid.row_tops[first];
                Rect::new(
                    laid.grid_left,
                    y,
                    laid.grid_width,
                    bottom(last) - laid.row_tops[first],
                )
            };
            children.push(part_background(style, cell, rect));
        }
    }
}

/// The table wrapper box of a table with `style` and its captions, in `rect`. Where the table
/// clips its overflow (`overflow`, paint containment), the wrapper clips the captions to its
/// sides as well, as in Chrome, whose table box holds them (WPT `contain-paint-table-002`).
fn wrapper(rect: Rect, children: Vec<Fragment>, style: &ComputedStyle) -> Fragment {
    let kind = if style.overflow_x == fagus_style::values::Overflow::Visible
        && style.overflow_y == fagus_style::values::Overflow::Visible
    {
        FragmentKind::Anonymous
    } else {
        FragmentKind::Box {
            style: Arc::new(ComputedStyle::from(ComputedValues {
                overflow_x: if style.overflow_x == fagus_style::values::Overflow::Visible {
                    fagus_style::values::Overflow::Visible
                } else {
                    fagus_style::values::Overflow::Clip
                },
                overflow_y: if style.overflow_y == fagus_style::values::Overflow::Visible {
                    fagus_style::values::Overflow::Visible
                } else {
                    fagus_style::values::Overflow::Clip
                },
                ..ComputedValues::default()
            })),
            node: None,
            sides: VisibleSides::ALL,
        }
    };
    Fragment {
        rect,
        kind,
        children: children.into(),
    }
}

/// The background of a table part with `style`, drawn in the rectangle of a cell: a box clipped
/// to the cell if the background has an image, which is positioned in the part's box `part`.
/// The color fills all the cell, also where it spans past the part, as in Chrome.
fn part_background(style: &Arc<ComputedStyle>, cell: Rect, part: impl FnOnce() -> Rect) -> Fragment {
    let has_image = style
        .background_image
        .iter()
        .any(|image| !matches!(image, fagus_style::values::Image::None));
    if !has_image {
        return Fragment {
            rect: cell,
            kind: FragmentKind::Box {
                style: Arc::clone(style),
                node: None,
                sides: VisibleSides::ALL,
            },
            children: Children::new(),
        };
    }
    let part = part();
    let clip = ComputedStyle::from(ComputedValues {
        overflow_x: fagus_style::values::Overflow::Hidden,
        overflow_y: fagus_style::values::Overflow::Hidden,
        background_color: style.background_color,
        ..ComputedValues::default()
    });
    let image = ComputedStyle::from(ComputedValues {
        background_color: Rgba::TRANSPARENT,
        ..ComputedValues::clone(style)
    });
    Fragment {
        rect: cell,
        kind: FragmentKind::Box {
            style: Arc::new(clip),
            node: None,
            sides: VisibleSides::ALL,
        },
        children: vec![Fragment {
            rect: Rect::new(part.x - cell.x, part.y - cell.y, part.width, part.height),
            kind: FragmentKind::Box {
                style: Arc::new(image),
                node: None,
                sides: VisibleSides::ALL,
            },
            children: Children::new(),
        }]
        .into(),
    }
}

/// The background of a column or column group, with the place of its columns in the table.
struct ColumnPart {
    /// The first of its columns in the source's order, and how many it spans.
    start: usize,
    span: usize,
    /// Where its columns begin from the table's left edge, and how wide they are together.
    x: Au,
    width: Au,
    /// Its style, without what a column does not draw.
    style: Arc<ComputedStyle>,
}

/// Grows `rows` so that a cell spanning them, `wanted` tall, fits, as Chrome does (Blink's
/// `DistributeExcessBlockSizeToRows` in `table_layout_utils.cc`, under its BSD license,
/// `CHROMIUM-LICENSE`): rows in which other spanning cells start take equal shares; otherwise
/// rows with content and without a fixed height grow in proportion to their heights; otherwise
/// the last row, if all are empty; otherwise all rows with content, in proportion.
/// How a row's `height` is given.
#[derive(Clone, Copy, PartialEq)]
enum RowHeight {
    Auto,
    Fixed,
    /// A fraction of the table's height.
    Percent(f32),
}

/// Grows the rows of a table to fill `room`: rows with a percentage height first, up to it,
/// then the rows of the table body without a height of their own, in proportion to their
/// heights (or evenly if they have none); without such rows, all body rows.
fn grow_rows(heights: &mut [Au], kinds: &[RowHeight], bodies: &[bool], room: Au) {
    let mut free = room - heights.iter().copied().sum::<Au>();
    for (height, kind) in heights.iter_mut().zip(kinds) {
        if let RowHeight::Percent(percentage) = kind
            && free > Au::ZERO
        {
            let wanted = Au::from_px(room.to_px() * percentage);
            let grow = (wanted - *height).clamp_non_negative().min(free);
            *height += grow;
            free -= grow;
        }
    }
    if free <= Au::ZERO {
        return;
    }
    // Body rows grow first; without them, the header and footer rows do (Blink's
    // `DistributeTableBlockSizeToSections`).
    let any_body = bodies.contains(&true);
    let grows = |index: usize| bodies[index] || !any_body;
    let auto: Vec<usize> = (0..heights.len())
        .filter(|&index| grows(index) && kinds[index] == RowHeight::Auto)
        .collect();
    let targets = if auto.is_empty() {
        (0..heights.len()).filter(|&index| grows(index)).collect()
    } else {
        auto
    };
    if targets.is_empty() {
        return;
    }
    let total: Au = targets.iter().map(|&index| heights[index]).sum();
    let count = targets.len() as i32;
    let mut given = Au::ZERO;
    for (position, &index) in targets.iter().enumerate() {
        // The last row takes what rounding leaves.
        let share = if position + 1 == targets.len() {
            free - given
        } else if total > Au::ZERO {
            Au::from_px(free.to_px() * heights[index].to_px() / total.to_px())
        } else {
            free / count
        };
        heights[index] += share;
        given += share;
    }
}

fn distribute_rowspan(
    heights: &mut [Au],
    (constrained, spans_start): (&[bool], &[bool]),
    rows: Range<usize>,
    wanted: Au,
    spacing: Au,
) {
    let count = i32::try_from(rows.len()).unwrap_or(i32::MAX);
    let total: Au = heights[rows.clone()].iter().copied().sum();
    let free = wanted - spacing * (count - 1) - total;
    if free <= Au::ZERO || rows.is_empty() {
        return;
    }
    let share = |heights: &mut [Au], targets: &[usize], weights: Option<Au>| {
        let mut left = free;
        for &row in targets {
            let delta = match weights {
                Some(total) if total > Au::ZERO => Au(i32::try_from(
                    i64::from(free.0) * i64::from(heights[row].0) / i64::from(total.0),
                )
                .unwrap_or(0)),
                _ => free / i32::try_from(targets.len()).unwrap_or(1),
            };
            heights[row] += delta;
            left -= delta;
        }
        if let Some(&last) = targets.last() {
            heights[last] += left;
        }
    };
    let originating: Vec<usize> = rows
        .clone()
        .filter(|&row| row != rows.start && spans_start[row])
        .collect();
    if !originating.is_empty() {
        share(heights, &originating, None);
        return;
    }
    let unconstrained: Vec<usize> = rows
        .clone()
        .filter(|&row| heights[row] > Au::ZERO && !constrained[row])
        .collect();
    if !unconstrained.is_empty() {
        let weight = unconstrained.iter().map(|&row| heights[row]).sum();
        share(heights, &unconstrained, Some(weight));
        return;
    }
    let non_empty: Vec<usize> = rows.clone().filter(|&row| heights[row] > Au::ZERO).collect();
    if non_empty.is_empty() {
        heights[rows.end - 1] += free;
        return;
    }
    share(heights, &non_empty, Some(total));
}

/// The height of the rows that the cell in `slot` spans and the spacing between them, without
/// the collapsed rows among them (`collapsed`).
fn spanned_height(heights: &[Au], collapsed: &[bool], slot: &Slot, spacing: Au) -> Au {
    let rows = (slot.row..slot.row + slot.rowspan).filter(|&row| !collapsed[row]);
    let (height, count) = rows.fold((Au::ZERO, 0), |(height, count), row| {
        (height + heights[row], count + 1)
    });
    height + spacing * (count - 1).max(0)
}

/// Whether a fragment holds anything that is drawn: text, images or boxes.
fn has_content(fragment: &Fragment) -> bool {
    fragment.children.iter().any(|child| {
        matches!(
            child.kind,
            FragmentKind::Text(_) | FragmentKind::Image(_) | FragmentKind::Box { .. }
        ) || has_content(child)
    })
}

/// The intrinsic widths and the given widths of a table's columns.
struct Columns {
    widths: Vec<(Au, Au)>,
    percentages: Vec<Option<f32>>,
    fixed: Vec<Option<Au>>,
    /// The widths `<col>` elements give their columns.
    given: Vec<Option<Au>>,
    /// The width each cell gives, without its content, by the cell's index in the grid; with
    /// collapsing borders, with half of the collapsed borders.
    cells: Vec<Option<Au>>,
    /// The border and padding of each cell across its columns, likewise.
    frames: Vec<Au>,
}

/// How `table-layout: fixed` sizes a column: by `<col>` and the cells of the first row, whose
/// content does not count (CSS 2.1 §17.5.2.1).
#[derive(Clone, Copy, Debug, PartialEq)]
enum FixedColumn {
    /// Without a width: it shares what the others leave.
    Auto,
    /// A width in pixels.
    Fixed(Au),
    /// A percentage of the table's columns and the border and padding of the cell that gives it,
    /// where that is of its content box (Blink's `percent_border_padding`).
    Percent(f32, Au),
}

/// The kinds of the columns of a table of `table-layout: fixed` laid out `width` wide (which
/// lengths with percentages refer to).
fn fixed_columns(columns: &Columns, grid: &Grid, layouter: &Layouter<'_>, width: Au) -> Vec<FixedColumn> {
    let mut kinds: Vec<Option<FixedColumn>> = (0..grid.columns)
        .map(|column| {
            columns.given[column].map(FixedColumn::Fixed).or_else(|| {
                columns.percentages[column].map(|percentage| FixedColumn::Percent(percentage, Au::ZERO))
            })
        })
        .collect();
    for (index, slot) in grid.cells.iter().enumerate().filter(|(_, slot)| slot.row == 0) {
        let style = &layouter.tree.get(slot.id).style;
        let span = slot.colspan as i32;
        let range = slot.column..(slot.column + slot.colspan).min(kinds.len());
        // A percentage of the content box takes the cell's border and padding on top, as in
        // Chrome, where no `<col>` gives the width.
        if let Size::Length(LengthPercentage::Percent(percentage)) = &style.width
            && columns.given[slot.column].is_none()
        {
            let frame = if style.box_sizing == BoxSizing::ContentBox {
                columns.frames.get(index).copied().unwrap_or(Au::ZERO)
            } else {
                Au::ZERO
            };
            for column in &mut kinds[range] {
                *column = Some(FixedColumn::Percent(percentage / span as f32, frame / span));
            }
            continue;
        }
        if kinds[slot.column].is_some() {
            continue;
        }
        let given = columns
            .cells
            .get(index)
            .copied()
            .flatten()
            .or_else(|| match &style.width {
                Size::Length(length) => Some(Au::from_px(length.resolve(width.to_px()))),
                _ => None,
            });
        if let Some(given) = given {
            for column in &mut kinds[range] {
                *column = column.or(Some(FixedColumn::Fixed(given / span)));
            }
        }
    }
    kinds
        .into_iter()
        .map(|kind| kind.unwrap_or(FixedColumn::Auto))
        .collect()
}

/// The least width of the columns of a table of `table-layout: fixed`: the sum of the widths
/// its columns are given (Blink's `ComputeGridInlineMinMax`).
fn fixed_min(columns: &Columns, grid: &Grid, layouter: &Layouter<'_>) -> Au {
    fixed_columns(columns, grid, layouter, Au::ZERO)
        .into_iter()
        .map(|kind| match kind {
            FixedColumn::Fixed(width) => width,
            _ => Au::ZERO,
        })
        .sum()
}

/// Column widths of `table-layout: fixed` that fill `width`, as Chrome shares it (Blink's
/// `SynchronizeAssignableTableInlineSizeAndColumnsFixed`): columns of fixed widths first,
/// scaled down to what the percentages leave where they do not fit, or up where no column is
/// auto; then columns of percentages, scaled the same way; auto columns share the rest. Columns
/// of width 0 are auto ones that get nothing unless all are such.
fn fixed_widths(columns: &Columns, grid: &Grid, layouter: &Layouter<'_>, width: Au) -> Vec<Au> {
    let kinds = fixed_columns(columns, grid, layouter, width);
    let percent = |percentage: f32, frame: Au| Au::from_px(width.to_px() * percentage) + frame;
    let (mut fixed_total, mut percent_total, mut autos, mut zeros) = (Au::ZERO, Au::ZERO, 0, 0);
    for kind in &kinds {
        match *kind {
            FixedColumn::Fixed(given) if given > Au::ZERO => fixed_total += given,
            FixedColumn::Fixed(_) => zeros += 1,
            FixedColumn::Percent(percentage, frame) => percent_total += percent(percentage, frame),
            FixedColumn::Auto => autos += 1,
        }
    }
    let scale = |total: Au, room: Au, up: bool| {
        if (total < room && up) || total > room {
            if total > Au::ZERO {
                Some(room.to_px() / total.to_px())
            } else {
                None
            }
        } else {
            Some(1.0)
        }
    };
    let mut result = vec![Au::ZERO; kinds.len()];
    let mut assigned = Au::ZERO;
    let mut last = None;
    if fixed_total > Au::ZERO {
        let room = (width - percent_total).clamp_non_negative();
        // Scaled down only where they do not fit the table, but to what the percentages leave.
        let factor = if fixed_total > width || (fixed_total < room && autos == 0) {
            room.to_px() / fixed_total.to_px()
        } else {
            1.0
        };
        for (index, kind) in kinds.iter().enumerate() {
            if let FixedColumn::Fixed(given) = *kind
                && given > Au::ZERO
            {
                result[index] = Au::from_px(given.to_px() * factor);
                assigned += result[index];
                last = Some(index);
            }
        }
    }
    if assigned >= width {
        return result;
    }
    if percent_total > Au::ZERO || kinds.iter().any(|kind| matches!(kind, FixedColumn::Percent(..))) {
        let room = width - assigned;
        let count = kinds
            .iter()
            .filter(|kind| matches!(kind, FixedColumn::Percent(..)))
            .count() as i32;
        let factor = scale(percent_total, room, autos == 0);
        for (index, kind) in kinds.iter().enumerate() {
            if let FixedColumn::Percent(percentage, frame) = *kind {
                result[index] = match factor {
                    Some(factor) => Au::from_px(percent(percentage, frame).to_px() * factor),
                    None => room / count,
                };
                assigned += result[index];
                last = Some(index);
            }
        }
    }
    let rest = width - assigned;
    let everything_zero = zeros == kinds.len();
    let sharing = if everything_zero { zeros } else { autos };
    for (index, kind) in kinds.iter().enumerate() {
        let shares = match kind {
            FixedColumn::Auto => !everything_zero,
            FixedColumn::Fixed(given) => *given <= Au::ZERO && everything_zero,
            FixedColumn::Percent(..) => false,
        };
        if shares && sharing > 0 {
            result[index] = rest / sharing as i32;
            assigned += result[index];
            last = Some(index);
        }
    }
    // What rounding leaves goes to the last column that got a width.
    if let Some(last) = last {
        result[last] += width - assigned;
    }
    result
}

/// A border as a style made for it has it, for [`CollapsedStyles`]: the bits of its width, its
/// style as drawn and the bits of its color.
type EdgeKey = (u32, std::mem::Discriminant<BorderStyle>, [u32; 4]);

impl Edge {
    /// What of the border the styles made for it take.
    fn key(&self) -> EdgeKey {
        let Rgba { r, g, b, a } = self.color;
        (
            self.used_width().to_bits(),
            std::mem::discriminant(&self.drawn_style()),
            [r.to_bits(), g.to_bits(), b.to_bits(), a.to_bits()],
        )
    }
}

/// The styles of the pieces of collapsed borders that tables draw, made once for each border
/// and direction: a long table has thousands of borders alike, and a style is large.
#[derive(Default)]
pub(crate) struct CollapsedStyles {
    /// The style of each piece, by its border and whether it is horizontal.
    pieces: FxHashMap<(EdgeKey, bool), Arc<ComputedStyle>>,
    /// The last styles cells got with collapsed borders, after the styles of the cells, by
    /// their borders and whether they draw them: the cells of a column usually come out alike,
    /// or alike in turns (striped rows), and each cell has a style of its own otherwise.
    cells: FxHashMap<([EdgeKey; 4], bool), Vec<CollapsedCell>>,
}

/// The style of a cell and the style it got with collapsed borders.
type CollapsedCell = (Arc<ComputedStyle>, Arc<ComputedStyle>);

impl CollapsedStyles {
    /// The most cell styles kept for each set of borders.
    const PER_BORDERS: usize = 4;

    /// Gives the fragment of a cell of style `style` `edges` (not to draw) as [`collapse_cell`]
    /// does, with a style shared with an earlier cell whose fragment had the same style and got
    /// the same borders: copying a style for each cell of a long table takes time, and keeping
    /// the copies memory.
    fn collapse_shared(&mut self, fragment: &mut Fragment, style: &ComputedStyle, edges: &[Edge; 4]) {
        collapse_geometry(fragment, style, edges);
        let FragmentKind::Box { style: source, .. } = &fragment.kind else {
            return;
        };
        let source = Arc::clone(source);
        let FragmentKind::Box { style: collapsed, .. } = &mut fragment.kind else {
            return;
        };
        let kept = self
            .cells
            .entry((edges.map(|edge| edge.key()), false))
            .or_default();
        if let Some((_, shared)) = kept.iter().find(|(kept, _)| **kept == *source) {
            *collapsed = Arc::clone(shared);
            return;
        }
        *collapsed = Arc::new(collapsed_style(&source, edges, false));
        if kept.len() >= Self::PER_BORDERS {
            kept.remove(0);
        }
        kept.push((source, Arc::clone(collapsed)));
    }
}

/// Gives a cell its collapsed borders: the fragment covers half of each border around it, and
/// its content moves by the difference to the cell's own borders.
fn collapse_cell(fragment: &mut Fragment, style: &ComputedStyle, edges: &[Edge; 4], draw: bool) {
    collapse_geometry(fragment, style, edges);
    if let FragmentKind::Box {
        style: cell_style, ..
    } = &mut fragment.kind
    {
        *cell_style = Arc::new(collapsed_style(cell_style, edges, draw));
    }
}

/// The geometry of [`collapse_cell`]: the fragment covers half of each border around it, and
/// its content moves by the difference to the cell's own borders (those of `style`).
fn collapse_geometry(fragment: &mut Fragment, style: &ComputedStyle, edges: &[Edge; 4]) {
    let own_left = Au::from_px(style.border_width.left);
    let own_top = Au::from_px(style.border_width.top);
    let own_x = own_left + Au::from_px(style.border_width.right);
    let own_y = own_top + Au::from_px(style.border_width.bottom);
    let width = |edge: &Edge| Au::from_px(edge.used_width());
    let dx = width(&edges[LEFT]) - own_left;
    let dy = width(&edges[TOP]) - own_top;
    for child in &mut fragment.children {
        child.rect.x += dx;
        child.rect.y += dy;
    }
    // The fragment spans the whole width of the borders; its column span covers half of each.
    let half_x = (width(&edges[LEFT]) + width(&edges[RIGHT])) / 2;
    fragment.rect.width = fragment.rect.width - own_x + half_x + half_x;
    fragment.rect.height = fragment.rect.height - own_y + width(&edges[TOP]) + width(&edges[BOTTOM]);
}

/// The style of a cell of style `cell_style` with the collapsed borders `edges`, which it draws
/// where `draw` says so.
fn collapsed_style(cell_style: &ComputedStyle, edges: &[Edge; 4], draw: bool) -> ComputedStyle {
    let mut collapsed = cell_style.clone();
    collapsed.border_width = fagus_style::values::Sides {
        top: edges[TOP].used_width(),
        right: edges[RIGHT].used_width(),
        bottom: edges[BOTTOM].used_width(),
        left: edges[LEFT].used_width(),
    };
    // Cells of whole rows leave their borders to the table (`collapsed_pieces`).
    collapsed.border_style = if draw {
        fagus_style::values::Sides {
            top: edges[TOP].drawn_style(),
            right: edges[RIGHT].drawn_style(),
            bottom: edges[BOTTOM].drawn_style(),
            left: edges[LEFT].drawn_style(),
        }
    } else {
        fagus_style::values::Sides::all(BorderStyle::None)
    };
    collapsed.border_color = fagus_style::values::Sides {
        top: Some(edges[TOP].color),
        right: Some(edges[RIGHT].color),
        bottom: Some(edges[BOTTOM].color),
        left: Some(edges[LEFT].color),
    };
    collapsed
}

/// Lets a cell laid out with collapsed borders that it does not draw draw them after all.
fn draw_collapsed(fragment: &mut Fragment, edges: &[Edge; 4]) {
    if let FragmentKind::Box { style, .. } = &mut fragment.kind {
        let mut drawn = (**style).clone();
        drawn.border_style = fagus_style::values::Sides {
            top: edges[TOP].drawn_style(),
            right: edges[RIGHT].drawn_style(),
            bottom: edges[BOTTOM].drawn_style(),
            left: edges[LEFT].drawn_style(),
        };
        *style = Arc::new(drawn);
    }
}

/// Gives the fragment of a cell that draws its collapsed borders `drawn` (see
/// [`Laid::draw_collapsed`]) the top and bottom borders `edges` instead, where they differ, on a
/// page that leaves rows next to the cell out (see [`Without`]). They are centered on the lines
/// between the rows as the others are; the cell's content stays where it is.
fn redraw_collapsed(fragment: &mut Fragment, drawn: &[Edge; 4], edges: &[Edge; 4]) {
    if drawn[TOP].key() == edges[TOP].key() && drawn[BOTTOM].key() == edges[BOTTOM].key() {
        return;
    }
    let FragmentKind::Box { style, sides, .. } = &mut fragment.kind else {
        return;
    };
    let half = |edge: &Edge| Au::from_px(edge.used_width() / 2.0);
    let mut redrawn = (**style).clone();
    if sides.top {
        let grows = half(&edges[TOP]) - half(&drawn[TOP]);
        fragment.rect.y -= grows;
        fragment.rect.height += grows;
        for child in &mut fragment.children {
            child.rect.y += grows;
        }
        redrawn.border_width.top = edges[TOP].used_width();
        redrawn.border_style.top = edges[TOP].drawn_style();
        redrawn.border_color.top = Some(edges[TOP].color);
    }
    if sides.bottom {
        fragment.rect.height += half(&edges[BOTTOM]) - half(&drawn[BOTTOM]);
        redrawn.border_width.bottom = edges[BOTTOM].used_width();
        redrawn.border_style.bottom = edges[BOTTOM].drawn_style();
        redrawn.border_color.bottom = Some(edges[BOTTOM].color);
    }
    *style = Arc::new(redrawn);
}

impl Laid {
    /// The collapsed borders on a page that leaves rows out as `hiding` says, if it leaves any.
    fn without(&self, hiding: Hiding) -> Option<&Without> {
        self.without.iter().find(|without| without.hiding == hiding)
    }

    /// Gives `fragment`, a copy of cell `index` as laid out, the cell's collapsed borders to draw
    /// (see [`draw_collapsed`]), in a style made once for the cell: a cell that goes on over
    /// thousands of pages would have a copy of it on each.
    fn draw_collapsed(&self, index: usize, fragment: &mut Fragment) {
        let Some(edges) = &self.edges[index] else {
            return;
        };
        if let Some(drawn) = self.drawn.borrow().get(&index)
            && let FragmentKind::Box { style, .. } = &mut fragment.kind
        {
            *style = Arc::clone(drawn);
            return;
        }
        draw_collapsed(fragment, edges);
        if let FragmentKind::Box { style, .. } = &fragment.kind {
            self.drawn.borrow_mut().insert(index, Arc::clone(style));
        }
    }
}

/// A collapsed border as Blink's table painter sees it: its index among the edges of the grid
/// (`None` past them) and the border.
type Joined = Option<(usize, Edge)>;

/// Which of two collapsed borders is drawn over the joint where they meet (Blink's
/// `CompareForPaint`): 1 the first, -1 the second, 0 both. The wider wins, then the stronger
/// style, then the border of the earlier box.
fn compare_for_paint(first: Joined, second: Joined) -> i8 {
    let paints =
        |edge: Joined| edge.is_some_and(|(_, edge)| edge.style.is_visible() && edge.used_width() > 0.0);
    let rank = |style: BorderStyle| match style {
        BorderStyle::None => 0,
        BorderStyle::Hidden => 1,
        BorderStyle::Inset => 2,
        BorderStyle::Groove => 3,
        BorderStyle::Outset => 4,
        BorderStyle::Ridge => 5,
        BorderStyle::Dotted => 6,
        BorderStyle::Dashed => 7,
        BorderStyle::Solid => 8,
        BorderStyle::Double => 9,
    };
    if first.map(|(index, _)| index) == second.map(|(index, _)| index) {
        return 0;
    }
    match (first, second) {
        (Some((_, a)), Some((_, b))) if paints(first) && paints(second) => {
            let (wa, wb) = (Au::from_px(a.used_width()), Au::from_px(b.used_width()));
            if wa != wb {
                return if wa > wb { 1 } else { -1 };
            }
            if a.style == b.style {
                return match a.order.cmp(&b.order) {
                    std::cmp::Ordering::Less => 1,
                    std::cmp::Ordering::Greater => -1,
                    std::cmp::Ordering::Equal => 0,
                };
            }
            if rank(a.style) > rank(b.style) { 1 } else { -1 }
        }
        _ => match (paints(first), paints(second)) {
            (false, false) => 0,
            (false, true) => -1,
            _ => 1,
        },
    }
}

/// The width of a collapsed border, nothing for none.
fn joined_width(edge: Joined) -> Au {
    edge.map_or(Au::ZERO, |(_, edge)| Au::from_px(edge.used_width()))
}

/// How a collapsed border meets the joint at one of its ends (Blink's `ComputeEdgeJoints`),
/// with the borders `before` and `after` it along the horizontal line and `over` and `under`
/// it along the vertical one: how far it reaches over the joint, half the width of the widest
/// border across, or less than it stops short of the joint.
fn joint_reach(around: [Joined; 4], horizontal: bool, start: bool) -> Au {
    let [before, after, over, under] = around;
    let inline = compare_for_paint(before, after);
    let block = compare_for_paint(over, under);
    let across = match (horizontal, inline, block) {
        (true, _, 1) => joined_width(over),
        (true, _, _) => joined_width(under),
        (false, 1, _) => joined_width(before),
        (false, _, _) => joined_width(after),
    };
    let versus = compare_for_paint(
        if inline == 1 { before } else { after },
        if block == 1 { over } else { under },
    );
    let wins = match (horizontal, start) {
        (true, true) => versus != -1 && inline != 1,
        (true, false) => versus != -1 && inline != -1,
        (false, true) => versus != 1 && block != 1,
        (false, false) => versus != 1 && block != -1,
    };
    if wins { across / 2 } else { -(across / 2) }
}

/// The collapsed borders of a table as Blink draws them (`TablePainter::PaintCollapsedBorders`
/// in `table_painters.cc`, under its BSD license, `CHROMIUM-LICENSE`): the border between two
/// slots is a rectangle centered on its grid line that reaches over the joints at its ends it
/// wins and stops short of the others. Per row: the borders of its top line and those along
/// it, from its top; and those of its bottom line.
fn collapsed_pieces(
    collapsed: &Collapsed,
    grid: &Grid,
    lines: &[Au],
    heights: &[Au],
    styles: &mut CollapsedStyles,
) -> (Vec<Vec<Fragment>>, Vec<Vec<Fragment>>) {
    let (rows, columns) = (grid.rows.len(), grid.columns);
    let mut borders = vec![Vec::new(); rows];
    let mut bottoms = vec![Vec::new(); rows];
    if rows == 0 || columns == 0 || lines.len() <= columns || heights.len() < rows {
        return (borders, bottoms);
    }
    // Lines inside a cell that spans rows or columns have no border.
    let mut inside_horizontal = vec![vec![false; columns]; rows + 1];
    let mut inside_vertical = vec![vec![false; columns + 1]; rows];
    for slot in &grid.cells {
        for row in slot.row..(slot.row + slot.rowspan).min(rows) {
            let end = (slot.column + slot.colspan).min(columns);
            for inside in inside_vertical[row].iter_mut().take(end).skip(slot.column + 1) {
                *inside = true;
            }
            if row > slot.row {
                for inside in inside_horizontal[row].iter_mut().take(end).skip(slot.column) {
                    *inside = true;
                }
            }
        }
    }
    // Blink's edges: per line of the grid, the vertical border at each column line and the
    // horizontal one over each column, in turn, and one more past the last; their neighbors
    // are found by index, as Blink finds them.
    let per_row = 2 * columns + 2;
    let count = (rows + 1) * per_row;
    let edge = |index: Option<usize>| -> Joined {
        let index = index.filter(|&index| index < count)?;
        let (row, place) = (index / per_row, index % per_row);
        let edge = if place % 2 == 1 {
            let column = place / 2;
            if column >= columns || inside_horizontal[row][column] {
                Edge::NONE
            } else {
                collapsed.horizontal[row][column]
            }
        } else if row < rows && !inside_vertical[row][place / 2] {
            collapsed.vertical[row][place / 2]
        } else {
            Edge::NONE
        };
        Some((index, edge))
    };
    let near = |index: usize, delta: isize| edge(index.checked_add_signed(delta));
    let per = isize::try_from(per_row).unwrap_or(isize::MAX);
    let drawn = |edge: Joined| edge.filter(|(_, edge)| edge.style.is_visible() && edge.used_width() > 0.0);
    for row in 0..=rows {
        for column in 0..columns {
            let index = row * per_row + 2 * column + 1;
            let this = edge(Some(index));
            let Some((_, border)) = drawn(this) else {
                continue;
            };
            let thickness = joined_width(this);
            let start = joint_reach(
                [near(index, -2), this, near(index, -(per + 1)), near(index, -1)],
                true,
                true,
            );
            let end = joint_reach(
                [this, near(index, 2), near(index, -(per - 1)), near(index, 1)],
                true,
                false,
            );
            let x = lines[column] - start;
            // A border that stops short of joints wider than its column is long has no length
            // left, not a negative one.
            let width = (lines[column + 1] + end - x).clamp_non_negative();
            if row < rows {
                borders[row].push(border_piece(
                    border,
                    Rect::new(x, -(thickness / 2), width, thickness),
                    true,
                    styles,
                ));
            }
            if row > 0 {
                let top = heights[row - 1] - thickness / 2;
                bottoms[row - 1].push(border_piece(
                    border,
                    Rect::new(x, top, width, thickness),
                    true,
                    styles,
                ));
            }
        }
    }
    for (row, &height) in heights.iter().enumerate().take(rows) {
        for (column, &line) in lines.iter().enumerate().take(columns + 1) {
            let index = row * per_row + 2 * column;
            let this = edge(Some(index));
            let Some((_, border)) = drawn(this) else {
                continue;
            };
            let thickness = joined_width(this);
            let start = joint_reach(
                [near(index, -1), near(index, 1), near(index, -per), this],
                false,
                true,
            );
            let end = joint_reach(
                [near(index, per - 1), near(index, per + 1), this, near(index, per)],
                false,
                false,
            );
            let length = (height + end + start).clamp_non_negative();
            let rect = Rect::new(line - thickness / 2, -start, thickness, length);
            borders[row].push(border_piece(border, rect, false, styles));
        }
    }
    (borders, bottoms)
}

/// A piece of a collapsed border: a box that draws only its top border (`horizontal`) or only its
/// left border.
fn border_piece(edge: Edge, rect: Rect, horizontal: bool, styles: &mut CollapsedStyles) -> Fragment {
    let style = styles.pieces.entry((edge.key(), horizontal)).or_insert_with(|| {
        let mut style = ComputedStyle::default();
        if horizontal {
            style.border_width.top = edge.used_width();
            style.border_style.top = edge.drawn_style();
            style.border_color.top = Some(edge.color);
        } else {
            style.border_width.left = edge.used_width();
            style.border_style.left = edge.drawn_style();
            style.border_color.left = Some(edge.color);
        }
        Arc::new(style)
    });
    Fragment {
        rect,
        kind: FragmentKind::Box {
            style: Arc::clone(style),
            node: None,
            sides: VisibleSides::ALL,
        },
        children: Children::new(),
    }
}

/// Makes the cells among `children` (a table's, on a page whose rows end with monolithic
/// content reaching past the end of their room on it, at `end`, above a repeated footer) that
/// reach past it without such content end there, as in Chrome, where only the cells with the
/// content overflow the page.
fn end_cells_with_page(children: &mut [Fragment], end: Au) {
    for child in children {
        if child.rect.y + child.rect.height > end
            && matches!(child.kind, FragmentKind::Box { .. })
            && crate::monolithic::content_bottom(child, Au::ZERO).is_none_or(|reach| reach <= end)
        {
            child.rect.height = (end - child.rect.y).clamp_non_negative();
        }
    }
}

/// The baseline of a laid-out table: that of its first row, from the top of `table`, as Blink's
/// `RowBaselineTabulator` finds it. It is the first baseline of the row's cells aligned on the
/// baseline; without one (as with `td`, which inherits `middle` from its row) the bottom content
/// edge of the row's cell with the least border and padding below it. The anonymous cell
/// around the content of a table without cells (as in `<span style="display: inline-table">`)
/// has an anonymous fragment, without border or padding, aligned on the baseline.
pub(crate) fn table_baseline(table: &Fragment, mode: fagus_style::values::WritingMode) -> Option<Au> {
    // The style of each cell, `None` for anonymous ones, with its top in the table.
    let mut cells: Vec<(&Fragment, Option<&Arc<ComputedStyle>>, Au)> = Vec::new();
    table_cells(table, Au::ZERO, &mut cells);
    let top = cells.iter().map(|&(_, _, y)| y).min()?;
    let first: Vec<_> = cells.into_iter().filter(|&(_, _, y)| y == top).collect();
    let ascent = first
        .iter()
        .filter(|(_, style, _)| style.is_none_or(|style| style.vertical_align == VerticalAlign::Baseline))
        .filter_map(|(cell, ..)| first_baseline(cell, mode))
        .max();
    if let Some(ascent) = ascent {
        return Some(top + ascent);
    }
    let first: Vec<_> = first.into_iter().map(|(cell, style, _)| (cell, style)).collect();
    let height = first.iter().map(|(cell, _)| cell.rect.height).min()?;
    let below = first
        .iter()
        .map(|(_, style)| {
            style.map_or(Au::ZERO, |style| {
                Au::from_px(
                    style.border_width.bottom + style.padding.bottom.resolve(table.rect.width.to_px()),
                )
            })
        })
        .min()?;
    Some(top + (height - below).clamp_non_negative())
}

/// Collects the cells among the children of `fragment` (a table, or a positioned row or row
/// group in it, see [`Layouter::nest_positioned_rows`]) that lies `y` below the table's top,
/// with their styles (`None` for anonymous cells) and tops in the table.
fn table_cells<'f>(
    fragment: &'f Fragment,
    y: Au,
    cells: &mut Vec<(&'f Fragment, Option<&'f Arc<ComputedStyle>>, Au)>,
) {
    use fagus_style::values::InternalDisplay;
    for child in &fragment.children {
        match &child.kind {
            FragmentKind::Box { style, .. }
                if style.display == Display::Internal(InternalDisplay::TableCell) =>
            {
                cells.push((child, Some(style), y + child.rect.y));
            }
            FragmentKind::Box { style, .. }
                if matches!(
                    style.display,
                    Display::Internal(
                        InternalDisplay::TableRow
                            | InternalDisplay::TableRowGroup
                            | InternalDisplay::TableHeaderGroup
                            | InternalDisplay::TableFooterGroup
                    )
                ) =>
            {
                table_cells(child, y + child.rect.y, cells);
            }
            FragmentKind::Anonymous => cells.push((child, None, y + child.rect.y)),
            _ => {}
        }
    }
}

/// Moves the outside marker of a list item that went into the first line of a table or of a
/// caption above it (`above`) to the baseline of the table (see [`table_baseline`]), which
/// Chrome aligns it with (`UnpositionedListMarker::ContentAlignmentBaseline`): with cells in
/// the middle, below the text of the first row.
fn align_marker(above: &mut [Fragment], table: &mut Fragment, mode: fagus_style::values::WritingMode) {
    if mode.is_vertical() {
        return;
    }
    let Some(baseline) = table_baseline(table, mode) else {
        return;
    };
    let target = table.rect.y + baseline;
    let mut parts: Vec<&mut Fragment> = above.iter_mut().chain([table]).collect();
    let Some(marker) = parts.iter().find_map(|part| marker_baseline(part, Au::ZERO)) else {
        return;
    };
    for part in &mut parts {
        if shift_marker(part, target - marker) {
            return;
        }
    }
}

/// Whether a fragment is the box of a list item, whose marker is its own.
fn is_list_item(fragment: &Fragment) -> bool {
    matches!(&fragment.kind, FragmentKind::Box { style, .. }
        if matches!(style.display, Display::Normal { list_item: true, .. }))
}

/// The baseline of the first outside marker in `fragment` (which lies `origin` down), outside
/// list items inside it.
fn marker_baseline(fragment: &Fragment, origin: Au) -> Option<Au> {
    let y = origin + fragment.rect.y;
    if is_list_item(fragment) {
        return None;
    }
    if matches!(fragment.kind, FragmentKind::Line { .. }) {
        return fragment.children.iter().find_map(|run| match &run.kind {
            FragmentKind::Text(text) if text.marker => Some(y + run.rect.y + text.baseline),
            _ => None,
        });
    }
    fragment
        .children
        .iter()
        .find_map(|child| marker_baseline(child, y))
}

/// Moves the runs of the first outside marker in `fragment` down `by`; whether it found one.
fn shift_marker(fragment: &mut Fragment, by: Au) -> bool {
    if is_list_item(fragment) {
        return false;
    }
    if matches!(fragment.kind, FragmentKind::Line { .. }) {
        let mut found = false;
        for run in &mut fragment.children {
            if matches!(&run.kind, FragmentKind::Text(text) if text.marker) {
                run.rect.y += by;
                found = true;
            }
        }
        return found;
    }
    fragment.children.iter_mut().any(|child| shift_marker(child, by))
}

/// The horizontal and vertical `border-spacing` of a table in whole pixels, as browsers keep
/// it: 2mm come out 7 pixels.
fn border_spacing(style: &ComputedStyle) -> (Au, Au) {
    let whole = |px: f32| Au::from_px(px.trunc());
    (whole(style.border_spacing.0), whole(style.border_spacing.1))
}

/// The width the columns of a table need for their percentages and max-content widths (from
/// Chrome's `ComputeGridInlineMinMax`, BSD-licensed, see `CHROMIUM-LICENSE`): a column of p % that
/// is w wide needs w / p, and the columns without percentages need their widths over the rest.
fn percentage_width(columns: &Columns) -> Au {
    let mut from_columns = Au::ZERO;
    let mut others = Au::ZERO;
    let mut total = 0.0;
    for (&(_, max), percentage) in columns.widths.iter().zip(&columns.percentages) {
        match percentage {
            Some(percentage) if *percentage > 0.0 => {
                if max > Au::ZERO {
                    from_columns = from_columns.max(Au::from_px(max.to_px() / percentage));
                }
                total += percentage;
            }
            _ => others += max,
        }
    }
    let total = f32::min(total, 1.0);
    if total <= 0.0 {
        return Au::ZERO;
    }
    let from_others = if others == Au::ZERO {
        Au::ZERO
    } else if total >= 1.0 {
        Au::MAX
    } else {
        Au::from_px(others.to_px() / (1.0 - total))
    };
    from_columns.max(from_others)
}

/// Distributes the width of a table's grid to its columns (CSS 2.1 §17.5.2.2, as browsers do):
/// every column gets at least its minimum width; percentage columns get their share of the
/// table; the rest goes to columns with a fixed width up to it, then to auto columns up to
/// their max-content width (Chrome's specified and max-content guesses, CSS Tables 3 §3.9.3).
fn distribute_columns(
    columns: &[(Au, Au)],
    percentages: &[Option<f32>],
    fixed: &[Option<Au>],
    width: Au,
) -> Vec<Au> {
    let min_sum: Au = columns.iter().map(|column| column.0).sum();
    if min_sum >= width {
        return columns.iter().map(|column| column.0).collect();
    }
    // Percentages beyond 100 % in all count for nothing: a column gets what is left of 100 %
    // after the columns before it, as in Blink.
    let mut total = 0.0_f32;
    let percentages: Vec<Option<f32>> = percentages
        .iter()
        .map(|percentage| {
            percentage.map(|percentage| {
                let capped = percentage.min((1.0 - total).max(0.0));
                total += capped;
                capped
            })
        })
        .collect();
    // Where the percentage columns at their share, and the others at their minimum, do not
    // fit, each column gets its minimum and a part of what its share asks for beyond it, the
    // same for all (the linear interpolation between the minimum and the percentage guesses of
    // CSS Tables 3, §3.9.3, as in Blink).
    let guesses: Vec<Au> = columns
        .iter()
        .zip(&percentages)
        .map(|(column, percentage)| match percentage {
            Some(percentage) => column.0.max(Au::from_px(width.to_px() * percentage)),
            None => column.0,
        })
        .collect();
    let guess_sum: Au = guesses.iter().copied().sum();
    if guess_sum >= width && guess_sum > min_sum {
        let ratio = (width - min_sum).to_px() / (guess_sum - min_sum).to_px();
        return columns
            .iter()
            .zip(&guesses)
            .map(|(column, &guess)| column.0 + Au::from_px((guess - column.0).to_px() * ratio))
            .collect();
    }
    let percentages = &percentages[..];
    // Percentage columns first, limited to what is left above the other columns' minimum.
    let mut used: Vec<Au> = columns.iter().map(|column| column.0).collect();
    let mut available = width - min_sum;
    for (index, percentage) in percentages.iter().enumerate() {
        if let Some(percentage) = percentage {
            let target = Au::from_px(width.to_px() * percentage);
            let extra = (target - used[index]).clamp_non_negative().min(available);
            used[index] += extra;
            available -= extra;
        }
    }
    let auto: Vec<usize> = (0..columns.len())
        .filter(|&index| percentages[index].is_none() && fixed[index].is_none())
        .collect();
    let others: Vec<usize> = (0..columns.len())
        .filter(|&index| percentages[index].is_none() && fixed[index].is_some())
        .collect();
    // Columns with a fixed width grow to it first, then auto columns to their max-content
    // width, as in Chrome (CSS Tables 3, §3.9.3; Blink's
    // `DistributeInlineSizeToComputedInlineSizeAuto`).
    for group in [&others, &auto] {
        let wanted: Au = group
            .iter()
            .map(|&index| (columns[index].1 - used[index]).clamp_non_negative())
            .sum();
        if wanted <= Au::ZERO || available <= Au::ZERO {
            continue;
        }
        let share = available.min(wanted);
        let wants: Vec<Au> = group
            .iter()
            .map(|&index| (columns[index].1 - used[index]).clamp_non_negative())
            .collect();
        for (&index, delta) in group.iter().zip(shares(share, &wants, wanted)) {
            used[index] += delta;
        }
        available -= share;
    }
    // What is still left widens auto columns in proportion to their width, or else all
    // non-percentage columns, or else the percentage columns, which all columns then are, in
    // proportion to their percentages (Blink's `DistributeInlineSizeToComputedInlineSizeAuto`).
    if available > Au::ZERO {
        let (targets, weights): (Vec<usize>, Vec<Au>) = if !auto.is_empty() {
            let weights = auto.iter().map(|&index| used[index]).collect();
            (auto, weights)
        } else if !others.is_empty() {
            let weights = others.iter().map(|&index| used[index]).collect();
            (others, weights)
        } else {
            let weights = percentages
                .iter()
                .map(|percentage| Au::from_px(percentage.unwrap_or(0.0) * 10_000.0))
                .collect();
            ((0..columns.len()).collect(), weights)
        };
        let total: Au = weights.iter().copied().sum();
        let widths: Vec<Au> = if total > Au::ZERO {
            weights
        } else {
            vec![Au::EPSILON; targets.len()]
        };
        let total = widths.iter().copied().sum();
        for (&index, delta) in targets.iter().zip(shares(available, &widths, total)) {
            used[index] += delta;
        }
    }
    used
}

/// `amount` shared in proportion to `weights` (which add up to `total`), as Blink shares
/// width among columns: each share in whole layout units, cut down (`LayoutUnit::MulDiv`), and
/// what that leaves to the last.
pub(crate) fn shares(amount: Au, weights: &[Au], total: Au) -> Vec<Au> {
    if total <= Au::ZERO {
        return vec![Au::ZERO; weights.len()];
    }
    let mut left = amount;
    let mut shares: Vec<Au> = weights
        .iter()
        .map(|weight| {
            let share = i64::from(amount.0) * i64::from(weight.0) / i64::from(total.0);
            let share = Au(i32::try_from(share).unwrap_or(0));
            left -= share;
            share
        })
        .collect();
    if let Some(last) = shares.last_mut() {
        *last += left;
    }
    shares
}

/// The top captions of a table (in `rect`) whose rows move to the next page, where the table
/// box starts with row `row`.
/// Where the captions of a table go on, if they broke on the page before: the caption (among
/// all of the table's) and the token inside it.
type CaptionResume<'t> = Option<(usize, Option<&'t BreakToken>)>;

/// The captions of a table on one side laid out on a page, and where they go on.
enum CaptionsPart {
    /// All of them, `Au` tall together.
    Done(Vec<Fragment>, Au),
    /// Those that fit, the last one broken, and the token they go on at (`Captions`).
    Broken(Vec<Fragment>, BreakToken),
    /// None: the first caption above the table does not fit, and the table moves to the next
    /// page.
    Moves,
}

/// The top captions of a table that break (`fragments`) in the table wrapper box, at
/// `margin_left`, `width` wide, filling the page to `limit`, going on at `token`.
fn captions_broken(
    fragments: Vec<Fragment>,
    (margin_left, width, limit): (Au, Au, Au),
    token: BreakToken,
    margin_top: Au,
) -> Outcome {
    let height = fragments
        .iter()
        .map(|fragment| fragment.rect.y + fragment.rect.height)
        .fold(limit, Au::max);
    Outcome {
        fragment: Fragment {
            rect: Rect::new(margin_left, Au::ZERO, width, height),
            kind: FragmentKind::Anonymous,
            children: fragments.into(),
        },
        token: Some(token),
        margin_bottom: Au::ZERO,
        collapsed_top: Margin::of(margin_top),
        collapsed_bottom: Margin::default(),
        through: false,
        passed_breaks: (BreakBetween::Auto, BreakBetween::Auto),
    }
}

fn captions_alone(above: Vec<Fragment>, rect: Rect, row: usize, margin_top: Au) -> Outcome {
    Outcome {
        fragment: Fragment {
            rect,
            kind: FragmentKind::Anonymous,
            children: above.into(),
        },
        token: Some(BreakToken::Table {
            row,
            cells: Vec::new(),
            offset: Au::ZERO,
            started: false,
            overflow: Au::ZERO,
            covered: false,
        }),
        margin_bottom: Au::ZERO,
        collapsed_top: Margin::of(margin_top),
        collapsed_bottom: Margin::default(),
        through: false,
        passed_breaks: (BreakBetween::Auto, BreakBetween::Auto),
    }
}
