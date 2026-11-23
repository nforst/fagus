//! The layouts of table cells broken across pages, kept for when the same cell is laid out again
//! in the same way.
//!
//! Rows that break are laid out more than once while the table finds its break (the rows again
//! with a later row pushed to the next page, again with room for the footer, the box around the
//! table again to end before its bottom border and padding), and each layout of a cell lays out
//! the tables inside it, which do the same with their own cells. Tables nested inside cells
//! would take time growing exponentially with their depth; with the layouts of their cells kept,
//! each is done once (as Blink keeps layout results in its fragment cache).
//!
//! A cell's layout depends on its box, the space and token it is laid out with and on the state
//! the [`Layouter`] keeps while it lays out (in `Cell`s), which it may change too. A layout is
//! reused only where all of that is the same, and leaves the state as the layout it repeats did.
//!
//! A flex or grid container that breaks across pages is laid out whole on each of them, and its
//! lines go on the page from where the page before ended: time would grow with the square of
//! the page count (and more with containers nested in its items, which break as well). Its
//! layout is kept from page to page in the same way ([`PagesMemo`]), so that each is done
//! once. So is the layout of each fixed box, which every page repeats: the pages share its
//! fragments rather than each having a copy of its own.
//!
//! A grid or flex container lays out its items more than once (at their content height, then
//! stretched to their area or line, or shrunk to a fixed row), and items that are containers do
//! the same with theirs: time would grow exponentially with the depth of containers nested in
//! each other. The layouts of items that are containers are kept ([`ItemMemo`]), so that each is
//! done once for each way it is laid out.
//!
//! A paragraph that goes on over many pages is shaped again on each, from its start, and with it
//! the atomic inlines in it (inline blocks and tables): time would grow with the square of their
//! number. Their layouts are kept ([`AtomicMemo`]), so that each is done once for each way it is
//! laid out.

use crate::block::{BreakToken, ContainerLine, Layouter, Outcome, Space};
use crate::fragment::Fragment;
use crate::tree::{BoxId, BoxKind, InlineItem};
use crate::units::Au;
use fagus_style::values::WritingMode;
use rustc_hash::FxHashMap;
use std::rc::Rc;

/// The layouts of broken cells kept on the page being laid out.
#[derive(Default)]
pub(crate) struct CellMemo {
    /// The page (and page count) the layouts were made for: they are dropped on the next.
    page: (usize, usize),
    /// The layouts by cell, room, whether the cell starts the page and token (written out);
    /// several may share those. A cell broken across a thousand columns of a page has a
    /// thousand tokens there, which were compared one by one for each layout.
    entries: FxHashMap<CellKey, Vec<Entry>>,
}

/// A cell, its room, whether it starts the page and its token, written out.
type CellKey = (BoxId, Option<Au>, bool, Option<String>);

/// The layouts of flex and grid containers laid out whole where they break across pages, and of
/// fixed boxes, kept for the pages after (see the module docs).
#[derive(Default)]
pub(crate) struct PagesMemo {
    /// The layouts by container; several may share it.
    entries: FxHashMap<BoxId, Vec<ContainerEntry>>,
    /// The layouts of fixed boxes.
    fixed: FxHashMap<FixedKey, Vec<FixedEntry>>,
    /// Whether layout depends on more than the state compares: on the page number (content with
    /// `counter(page)`), or on what grids pass to their subgrids. Found when first needed.
    page_dependent: Option<bool>,
    subgrids: Option<bool>,
}

/// The boxes whose content shows page numbers or named strings, which their intrinsic widths
/// depend on, with the page (and page count) their widths were last measured for.
pub(crate) type PageWidths = Option<(rustc_hash::FxHashSet<BoxId>, (usize, usize))>;

/// The layouts of flex and grid containers laid out whole as items (see the module docs).
#[derive(Default)]
pub(crate) struct ItemMemo {
    /// The layouts by container; several may share it.
    entries: FxHashMap<BoxId, Vec<ItemEntry>>,
}

/// The layouts of atomic inlines other than replaced elements and ruby columns (see the module
/// docs).
#[derive(Default)]
pub(crate) struct AtomicMemo {
    /// The layouts by box and the width of the line they are in; several may share those.
    entries: FxHashMap<(BoxId, Au), Vec<(Kept, crate::block::Atomic)>>,
}

/// The state that the [`Layouter`] was in when it made something, and the state it left, for
/// repeating it.
pub(crate) struct Kept {
    state: State,
    after: State,
    /// How many block-level boxes it placed (see `Layouter::blocks_placed`).
    placed: u64,
}

/// An item's layout and what it was made from.
struct ItemEntry {
    input: ContainerInput,
    /// In a document with subgrids, what the item's parent passed down to it, and what it
    /// reported of its rows (see [`Layouter::item_container`]).
    subgrid: Option<(
        crate::subgrid::SubgridInput,
        Option<Vec<crate::grid::Contribution>>,
    )>,
    outcome: Outcome,
    /// The state the layout left.
    after: State,
    /// How many block-level boxes the layout placed (see `Layouter::blocks_placed`).
    placed: u64,
}

/// A container's layout and what it was made from.
struct ContainerEntry {
    input: ContainerInput,
    outcome: Outcome,
    lines: Rc<[ContainerLine]>,
    /// The state the layout left.
    after: State,
    /// How many block-level boxes the layout placed (see `Layouter::blocks_placed`).
    placed: u64,
}

/// A fixed box, the width and height of its containing block and its static position.
pub(crate) type FixedKey = (BoxId, Au, Au, (Au, Au));

/// A fixed box's layout, the state it was made in and the state it left.
struct FixedEntry {
    state: State,
    fragment: Fragment,
    after: State,
    placed: u64,
}

/// What a container's layout is made from besides its box.
#[derive(PartialEq)]
struct ContainerInput {
    space: Space,
    width: Option<Au>,
    height: Option<Au>,
    state: State,
}

impl PagesMemo {
    /// The most layouts kept for a container: one for each way it is laid out on its pages (at
    /// the top of the page or below other content), and not one per page where those differ on
    /// each.
    const PER_BOX: usize = 4;
}

/// A cell's layout and what it was made from.
struct Entry {
    input: Input,
    outcome: Option<Outcome>,
    /// The state the layout left.
    after: State,
    /// How many block-level boxes the layout placed (see `Layouter::blocks_placed`).
    placed: u64,
}

/// What a cell's layout is made from besides its box, room, place at the top of the page and
/// token (written out in full: tokens compare equal whatever the pages before took, see
/// `Consumed`, which the layout depends on too).
#[derive(PartialEq)]
struct Input {
    space: Space,
    width: Option<Au>,
    height: Option<Au>,
    root: bool,
    state: State,
}

/// The state that the [`Layouter`] keeps while it lays out, which layout reads and changes.
#[derive(Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools, reason = "a copy of the layouter's flags")]
struct State {
    table_fills: bool,
    table_grow: Au,
    table_unsized: bool,
    page_number: (usize, usize),
    string_position: Option<usize>,
    inline_containing_height: Option<Au>,
    list_item_edges: Option<(Au, Au)>,
    in_list_item: bool,
    unplaced_marker: bool,
    contexts: usize,
    columns: u32,
    spanning: Option<BoxId>,
    below_spanner: bool,
    last_resort_column: bool,
    measuring: bool,
    after_forced_break: bool,
    forced_break: bool,
    lenient_top: bool,
    ending_early: bool,
    early_slack: Au,
    /// Whether block-level boxes have been placed since the box whose content ends early
    /// started (all that layout reads of `Layouter::blocks_placed`).
    blocks_placed: bool,
    table_frame_breaks: bool,
    last_line_alone: bool,
    content_sized: bool,
    fixed_item_height: bool,
    column_height: Option<Au>,
    space_shortage: Option<Au>,
    shortage_taken: bool,
    breaks_avoided_items: bool,
    poor_lines: bool,
    breaks_avoided_inside: bool,
    line_budget: Option<usize>,
    clamp_follows: bool,
    margin_above: Au,
    room_below: Au,
    trim_pending: (bool, bool),
    frame: WritingMode,
    initial_containing_block: (Au, Au),
    cross_room: Option<Au>,
    unfragmented: u32,
    inline_table: bool,
    provisional: bool,
    measured: bool,
}

impl Layouter<'_> {
    /// [`Layouter::block_container_with`] for a table cell laid out on a page, reusing its layout
    /// where the cell was laid out the same way before on the page (see the module docs).
    pub(crate) fn cell_container(
        &self,
        id: BoxId,
        space: Space,
        token: Option<&BreakToken>,
        width: Option<Au>,
        height: Option<Au>,
        root: bool,
    ) -> Option<Outcome> {
        let state = self.state();
        let written = token.map(|token| format!("{token:?}"));
        let key = (id, space.remaining, space.at_top, written);
        {
            let mut memo = self.cell_memo.borrow_mut();
            if memo.page != state.page_number {
                memo.page = state.page_number;
                memo.entries.clear();
            }
            let found = memo.entries.get(&key).and_then(|entries| {
                entries.iter().find(|entry| {
                    let input = &entry.input;
                    input.state == state
                        && input.space == space
                        && input.width == width
                        && input.height == height
                        && input.root == root
                })
            });
            if let Some(entry) = found {
                self.restore(&entry.after, entry.placed);
                return entry.outcome.clone();
            }
        }
        let placed = self.blocks_placed.get().0;
        let growth = self.grid_growth.borrow().clone();
        let outcome = self.block_container_with(id, space, token, width, height, root);
        // A layout that changed what grids keep for their next pages is not repeated.
        if *self.grid_growth.borrow() != growth {
            return outcome;
        }
        let after = self.state();
        let entry = Entry {
            input: Input {
                space,
                width,
                height,
                root,
                state,
            },
            outcome: outcome.clone(),
            after,
            placed: self.blocks_placed.get().0 - placed,
        };
        let mut memo = self.cell_memo.borrow_mut();
        if memo.page == state.page_number {
            memo.entries.entry(key).or_default().push(entry);
        }
        outcome
    }

    /// [`Layouter::container_lines`] for a container laid out whole to break across pages (with
    /// `space.remaining` `None`), reusing its layout where it was laid out the same way before,
    /// on this page or one before (see the module docs).
    pub(crate) fn whole_container(
        &self,
        id: BoxId,
        space: Space,
        width: Option<Au>,
        height: Option<Au>,
    ) -> (Outcome, Rc<[ContainerLine]>) {
        if self.memo_limits().1 {
            let (outcome, lines) = self.container_lines(id, space, width, height);
            return (outcome, lines.into());
        }
        let (state, page_number) = self.memo_state();
        let input = ContainerInput {
            space,
            width,
            height,
            state,
        };
        {
            let memo = self.pages_memo.borrow();
            let found = memo
                .entries
                .get(&id)
                .and_then(|entries| entries.iter().find(|entry| entry.input == input));
            if let Some(entry) = found {
                let after = State {
                    page_number,
                    ..entry.after
                };
                self.restore(&after, entry.placed);
                return (entry.outcome.clone(), Rc::clone(&entry.lines));
            }
        }
        let placed = self.blocks_placed.get().0;
        let growth = self.grid_growth.borrow().clone();
        let (outcome, lines) = self.container_lines(id, space, width, height);
        let lines: Rc<[ContainerLine]> = lines.into();
        // A layout that changed what grids keep for their next pages is not repeated.
        if *self.grid_growth.borrow() != growth {
            return (outcome, lines);
        }
        let entry = ContainerEntry {
            input,
            outcome: outcome.clone(),
            lines: Rc::clone(&lines),
            after: self.state(),
            placed: self.blocks_placed.get().0 - placed,
        };
        let mut memo = self.pages_memo.borrow_mut();
        let entries = memo.entries.entry(id).or_default();
        if entries.len() >= PagesMemo::PER_BOX {
            entries.remove(0);
        }
        entries.push(entry);
        (outcome, lines)
    }

    /// [`Layouter::container_lines`] for a flex or grid container laid out whole as an item,
    /// reusing its layout where it was laid out the same way before (see the module docs).
    pub(crate) fn item_container(
        &self,
        id: BoxId,
        space: Space,
        width: Option<Au>,
        height: Option<Au>,
    ) -> Outcome {
        // In a document with subgrids, an item's layout depends on what its parent passed down
        // to it too, and the parent reads what it reports back.
        let subgrids = self.memo_limits().1;
        let given = subgrids.then(|| self.subgrid_input(id));
        let (state, page_number) = self.memo_state();
        let input = ContainerInput {
            space,
            width,
            height,
            state,
        };
        {
            let memo = self.item_memo.borrow();
            let found = memo.entries.get(&id).and_then(|entries| {
                entries.iter().find(|entry| {
                    entry.input == input && entry.subgrid.as_ref().map(|(given, _)| given) == given.as_ref()
                })
            });
            if let Some(entry) = found {
                let after = State {
                    page_number,
                    ..entry.after
                };
                self.restore(&after, entry.placed);
                if let Some((_, report)) = &entry.subgrid {
                    self.restore_report(id, report.clone());
                }
                return entry.outcome.clone();
            }
        }
        let placed = self.blocks_placed.get().0;
        let growth = self.grid_growth.borrow().clone();
        let (outcome, _) = self.container_lines(id, space, width, height);
        // A layout that changed what grids keep for their next pages is not repeated.
        if *self.grid_growth.borrow() != growth {
            return outcome;
        }
        let entry = ItemEntry {
            input,
            subgrid: given.map(|given| (given, self.unread_report(id))),
            outcome: outcome.clone(),
            after: self.state(),
            placed: self.blocks_placed.get().0 - placed,
        };
        let mut memo = self.item_memo.borrow_mut();
        let entries = memo.entries.entry(id).or_default();
        if entries.len() >= PagesMemo::PER_BOX {
            entries.remove(0);
        }
        entries.push(entry);
        outcome
    }

    /// The atomic inline `id` in a line of width `available`, as `lay_out` lays it out: as it was
    /// laid out before in the same state, where it was (see the module docs).
    pub(crate) fn kept_atomic(
        &self,
        id: BoxId,
        available: Au,
        lay_out: impl FnOnce() -> crate::block::Atomic,
    ) -> crate::block::Atomic {
        // Replaced elements and ruby columns are quick to lay out; replaced elements also
        // depend on the height of the cell they are in, which the state leaves out.
        if matches!(self.tree.get(id).kind, BoxKind::Replaced { .. })
            || self.tree.ruby_columns.contains_key(&id)
        {
            return lay_out();
        }
        let key = (id, available);
        {
            let memo = self.atomic_memo.borrow();
            let found = memo
                .entries
                .get(&key)
                .and_then(|entries| entries.iter().find(|(kept, _)| self.repeats(kept)));
            if let Some((kept, atomic)) = found {
                self.repeat(kept);
                return atomic.clone();
            }
        }
        let (atomic, kept) = self.keeping(lay_out);
        if let Some(kept) = kept {
            let mut memo = self.atomic_memo.borrow_mut();
            let entries = memo.entries.entry(key).or_default();
            if entries.len() >= PagesMemo::PER_BOX {
                entries.remove(0);
            }
            entries.push((kept, atomic.clone()));
        }
        atomic
    }

    /// What `lay_out` makes, and the state it was made in and left, for repeating it where it is
    /// made in the same state again (see [`Layouter::repeats`]); `None` where it may not be
    /// repeated: where layout depends on what grids pass to their subgrids, or the layout
    /// changed what grids keep for their next pages.
    pub(crate) fn keeping<T>(&self, lay_out: impl FnOnce() -> T) -> (T, Option<Kept>) {
        if self.memo_limits().1 {
            return (lay_out(), None);
        }
        let (state, _) = self.memo_state();
        let placed = self.blocks_placed.get().0;
        let growth = self.grid_growth.borrow().clone();
        let made = lay_out();
        if *self.grid_growth.borrow() != growth {
            return (made, None);
        }
        let kept = Kept {
            state,
            after: self.state(),
            placed: self.blocks_placed.get().0 - placed,
        };
        (made, Some(kept))
    }

    /// Whether what was made as `kept` tells would be made the same way now.
    pub(crate) fn repeats(&self, kept: &Kept) -> bool {
        !self.memo_limits().1 && self.memo_state().0 == kept.state
    }

    /// Leaves the state as making what `kept` tells left it (where [`Layouter::repeats`]).
    pub(crate) fn repeat(&self, kept: &Kept) {
        let after = State {
            page_number: self.page_number.get(),
            ..kept.after
        };
        self.restore(&after, kept.placed);
    }

    /// Drops the layouts kept for container `id`, which are not needed again: it fits on its
    /// page.
    pub(crate) fn forget_container(&self, id: BoxId) {
        self.pages_memo.borrow_mut().entries.remove(&id);
    }

    /// The fragment of a fixed box on a page that `lay_out` makes, by the box, the width and
    /// height of its containing block and its static position (`key`): the one made on a page
    /// before where the box was laid out in the same way there (see the module docs).
    pub(crate) fn fixed_box(&self, key: FixedKey, lay_out: impl FnOnce() -> Fragment) -> Fragment {
        if self.memo_limits().1 {
            return lay_out();
        }
        let (state, page_number) = self.memo_state();
        {
            let memo = self.pages_memo.borrow();
            let found = memo
                .fixed
                .get(&key)
                .and_then(|entries| entries.iter().find(|entry| entry.state == state));
            if let Some(entry) = found {
                let after = State {
                    page_number,
                    ..entry.after
                };
                self.restore(&after, entry.placed);
                return entry.fragment.clone();
            }
        }
        let placed = self.blocks_placed.get().0;
        let fragment = lay_out();
        let entry = FixedEntry {
            state,
            fragment: fragment.clone(),
            after: self.state(),
            placed: self.blocks_placed.get().0 - placed,
        };
        let mut memo = self.pages_memo.borrow_mut();
        let entries = memo.fixed.entry(key).or_default();
        if entries.len() >= PagesMemo::PER_BOX {
            entries.remove(0);
        }
        entries.push(entry);
        fragment
    }

    /// The state that layout reads, with the page number left out where nothing in the
    /// document depends on it (see [`PagesMemo`]), and the page number.
    fn memo_state(&self) -> (State, (usize, usize)) {
        let mut state = self.state();
        let page_number = state.page_number;
        if !self.memo_limits().0 {
            state.page_number = (0, 0);
        }
        (state, page_number)
    }

    /// Whether the layout of containers depends on the page number, and whether the document
    /// has subgrids, whose containers are not kept (see [`PagesMemo`]).
    fn memo_limits(&self) -> (bool, bool) {
        let mut memo = self.pages_memo.borrow_mut();
        let tree = self.tree;
        let page_dependent = *memo.page_dependent.get_or_insert_with(|| {
            tree.any_box(|layout_box| {
                matches!(&layout_box.kind, BoxKind::Inline { items }
                    if items.iter().any(|item| matches!(item, InlineItem::PageNumber { .. })))
            })
        });
        let subgrids = *memo.subgrids.get_or_insert_with(|| {
            tree.any_box(|layout_box| {
                matches!(layout_box.kind, BoxKind::Grid { .. })
                    && (layout_box.style.grid_template_columns.subgrid
                        || layout_box.style.grid_template_rows.subgrid)
            })
        });
        (page_dependent, subgrids)
    }

    /// Forgets the intrinsic widths of the boxes whose content shows page numbers or named
    /// strings when the page (or the page count) changed since they were measured: a running
    /// element whose grid column is sized by "Page 10 of 12" is measured on each page with that
    /// page's number, not once with the first page's.
    pub(crate) fn forget_page_widths(&self) {
        let page = self.page_number.get();
        let mut widths = self.page_widths.borrow_mut();
        let tree = self.tree;
        let (boxes, measured) = widths.get_or_insert_with(|| (page_dependent_boxes(tree), page));
        if *measured == page || boxes.is_empty() {
            return;
        }
        *measured = page;
        self.intrinsic_cache
            .borrow_mut()
            .retain(|id, _| !boxes.contains(id));
        self.content_cache
            .borrow_mut()
            .retain(|id, _| !boxes.contains(id));
    }

    /// The state that layout reads and changes.
    fn state(&self) -> State {
        let (count, start) = self.blocks_placed.get();
        State {
            table_fills: self.table_fills.get(),
            table_grow: self.table_grow.get(),
            table_unsized: self.table_unsized.get(),
            page_number: self.page_number.get(),
            string_position: self.string_position.get(),
            inline_containing_height: self.inline_containing_height.get(),
            list_item_edges: self.list_item_edges.get(),
            in_list_item: self.in_list_item.get(),
            unplaced_marker: self.unplaced_marker.get(),
            contexts: self.contexts.borrow().len(),
            columns: self.columns.get(),
            spanning: self.spanning.get(),
            below_spanner: self.below_spanner.get(),
            last_resort_column: self.last_resort_column.get(),
            measuring: self.measuring.get(),
            after_forced_break: self.after_forced_break.get(),
            forced_break: self.forced_break.get(),
            lenient_top: self.lenient_top.get(),
            ending_early: self.ending_early.get(),
            early_slack: self.early_slack.get(),
            blocks_placed: count > start,
            table_frame_breaks: self.table_frame_breaks.get(),
            last_line_alone: self.last_line_alone.get(),
            content_sized: self.content_sized.get(),
            fixed_item_height: self.fixed_item_height.get(),
            column_height: self.column_height.get(),
            space_shortage: self.space_shortage.get(),
            shortage_taken: self.shortage_taken.get() > 0,
            breaks_avoided_items: self.breaks_avoided_items.get(),
            poor_lines: self.poor_lines.get(),
            breaks_avoided_inside: self.breaks_avoided_inside.get(),
            line_budget: self.line_budget.get(),
            clamp_follows: self.clamp_follows.get(),
            margin_above: self.margin_above.get(),
            room_below: self.room_below.get(),
            trim_pending: self.trim_pending.get(),
            frame: self.frame.get(),
            initial_containing_block: self.initial_containing_block.get(),
            cross_room: self.cross_room.get(),
            unfragmented: self.unfragmented.get(),
            inline_table: self.inline_table.get(),
            provisional: self.provisional_grids.get() > 0,
            measured: self.measured_items.get() > 0,
        }
    }

    /// Leaves the state as a layout left it that placed `placed` block-level boxes.
    fn restore(&self, state: &State, placed: u64) {
        let (count, start) = self.blocks_placed.get();
        self.blocks_placed.set((count + placed, start));
        self.table_fills.set(state.table_fills);
        self.table_grow.set(state.table_grow);
        self.table_unsized.set(state.table_unsized);
        self.page_number.set(state.page_number);
        self.string_position.set(state.string_position);
        self.inline_containing_height.set(state.inline_containing_height);
        self.list_item_edges.set(state.list_item_edges);
        self.in_list_item.set(state.in_list_item);
        self.unplaced_marker.set(state.unplaced_marker);
        self.columns.set(state.columns);
        self.spanning.set(state.spanning);
        self.below_spanner.set(state.below_spanner);
        self.last_resort_column.set(state.last_resort_column);
        self.measuring.set(state.measuring);
        self.after_forced_break.set(state.after_forced_break);
        self.forced_break.set(state.forced_break);
        self.lenient_top.set(state.lenient_top);
        self.ending_early.set(state.ending_early);
        self.early_slack.set(state.early_slack);
        self.table_frame_breaks.set(state.table_frame_breaks);
        self.last_line_alone.set(state.last_line_alone);
        self.content_sized.set(state.content_sized);
        self.fixed_item_height.set(state.fixed_item_height);
        self.column_height.set(state.column_height);
        self.space_shortage.set(state.space_shortage);
        self.breaks_avoided_items.set(state.breaks_avoided_items);
        self.poor_lines.set(state.poor_lines);
        self.breaks_avoided_inside.set(state.breaks_avoided_inside);
        self.line_budget.set(state.line_budget);
        self.clamp_follows.set(state.clamp_follows);
        self.margin_above.set(state.margin_above);
        self.room_below.set(state.room_below);
        self.trim_pending.set(state.trim_pending);
        self.frame.set(state.frame);
        self.initial_containing_block.set(state.initial_containing_block);
        self.cross_room.set(state.cross_room);
        self.unfragmented.set(state.unfragmented);
        self.inline_table.set(state.inline_table);
    }
}

/// The boxes of `tree` that hold page numbers or named strings in their content, themselves or
/// in boxes inside them.
fn page_dependent_boxes(tree: &crate::tree::BoxTree) -> rustc_hash::FxHashSet<BoxId> {
    let mut found = rustc_hash::FxHashSet::default();
    let Some(root) = tree.root else {
        return found;
    };
    // The boxes in document order, each before the boxes inside it; then taken from the last,
    // so that a box comes after all boxes inside it.
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        order.push(id);
        stack.extend(tree.children(id));
    }
    for &id in order.iter().rev() {
        let own = matches!(&tree.get(id).kind, BoxKind::Inline { items }
            if items.iter().any(|item| matches!(item, InlineItem::PageNumber { .. } | InlineItem::NamedString { .. })));
        if own || tree.children(id).iter().any(|child| found.contains(child)) {
            found.insert(id);
        }
    }
    found
}
