//! Grid layout (CSS Grid 2): explicit and implicit tracks, `fr`, `minmax()`, `fit-content()`,
//! `repeat()` with `auto-fill` and `auto-fit`, line numbers, named lines and named areas, spans,
//! auto-placement (also `dense` and by columns), alignment of items and tracks, gaps, and `subgrid`
//! (see [`crate::subgrid`]).
//!
//! Columns are sized first, from the items' intrinsic widths; rows then from the items laid out
//! at their column widths. A grid breaks between pages between rows that no item spans.

use std::sync::Arc;

use fagus_style::ComputedStyle;
use fagus_style::values::{
    ContentAlignment, GridAreas, GridLine, ItemAlignment, LengthPercentage, MAX_GRID_TRACKS, Size, TrackList,
    TrackSize, WritingMode,
};

use crate::block::{BOTTOM, ContainerLine, LEFT, Layouter, LineItem, RIGHT, TOP, distribute, item_alignment};
use crate::fragment::Fragment;
use crate::grid_contribution::percent_height;
use crate::grid_positioned::AxisTracks;
use crate::subgrid::{Axis, Inherited, edged, names_in, tracks_for};
use crate::tree::BoxId;
use crate::units::Au;
use rustc_hash::FxHashMap;

/// The minimum sizing function of a track.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Minimum {
    Fixed(Au),
    MinContent,
    MaxContent,
    Auto,
}

/// The maximum sizing function of a track.
#[derive(Clone, Copy, Debug)]
enum Maximum {
    Fixed(Au),
    MinContent,
    MaxContent,
    Auto,
    Flex(f32),
    FitContent(Au),
}

/// A track while it is sized: its base size and growth limit (`None` while infinite).
#[derive(Clone, Copy, Debug)]
struct Track {
    min: Minimum,
    max: Maximum,
    base: Au,
    limit: Option<Au>,
}

impl Track {
    fn new(size: &TrackSize, basis: Option<Au>) -> Self {
        let (min, max) = sizing(size, basis);
        let base = match min {
            Minimum::Fixed(value) => value,
            _ => Au::ZERO,
        };
        let limit = match max {
            Maximum::Fixed(value) => Some(value.max(base)),
            _ => None,
        };
        Self {
            min,
            max,
            base,
            limit,
        }
    }

    /// A collapsed track: of no size.
    fn collapsed() -> Self {
        Self {
            min: Minimum::Fixed(Au::ZERO),
            max: Maximum::Fixed(Au::ZERO),
            base: Au::ZERO,
            limit: Some(Au::ZERO),
        }
    }

    fn flexible(&self) -> bool {
        matches!(self.max, Maximum::Flex(_))
    }

    fn intrinsic_min(&self) -> bool {
        !matches!(self.min, Minimum::Fixed(_))
    }

    fn intrinsic_max(&self) -> bool {
        !matches!(self.max, Maximum::Fixed(_) | Maximum::Flex(_))
    }
}

/// The sizing functions of a track size. Percentages need a definite `basis`; without one they
/// behave like `auto`.
fn sizing(size: &TrackSize, basis: Option<Au>) -> (Minimum, Maximum) {
    let length = |length: &LengthPercentage| match length {
        LengthPercentage::Px(px) => Some(Au::from_px(*px)),
        other => basis.map(|basis| Au::from_px(other.resolve(basis.to_px()))),
    };
    let min = |size: &TrackSize| match size {
        TrackSize::Length(value) => length(value).map_or(Minimum::Auto, Minimum::Fixed),
        TrackSize::MinContent => Minimum::MinContent,
        TrackSize::MaxContent => Minimum::MaxContent,
        _ => Minimum::Auto,
    };
    let max = |size: &TrackSize| match size {
        TrackSize::Length(value) => length(value).map_or(Maximum::Auto, Maximum::Fixed),
        TrackSize::Fraction(fraction) => Maximum::Flex(*fraction),
        TrackSize::MinContent => Maximum::MinContent,
        TrackSize::MaxContent => Maximum::MaxContent,
        TrackSize::FitContent(limit) => length(limit).map_or(Maximum::MaxContent, Maximum::FitContent),
        _ => Maximum::Auto,
    };
    match size {
        TrackSize::MinMax(low, high) => (min(low), max(high)),
        TrackSize::Fraction(fraction) => (Minimum::Auto, Maximum::Flex(*fraction)),
        TrackSize::FitContent(_) => (Minimum::Auto, max(size)),
        other => (min(other), max(other)),
    }
}

/// A range of tracks: start index and end (exclusive).
pub(crate) type Span = (usize, usize);

/// What an item spanning some tracks needs of them: its min-content and max-content sizes, and
/// its minimum contribution, which `auto` minimums of tracks take (CSS Grid 2, §11.5): the
/// outer size from `min-width`, with `min-width: auto` the automatic minimum size.
pub(crate) type Contribution = (Span, (Au, Au), Au);

/// Whether an item spanning `span` crosses more than one track and one of them is flexible
/// (`flexible` tells for each track), so that its automatic minimum size is zero (CSS Grid 2,
/// §6.6): such an item does not make a flexible track grow for its content.
pub(crate) fn spans_flexible(flexible: &[bool], (start, end): Span) -> bool {
    end - start > 1 && flexible.get(start..end).is_some_and(|span| span.contains(&true))
}

/// Sets the shims of the items of a grid container (of style `style`) that are aligned by their
/// first baselines (`align-self: baseline`) in a row they alone span: each moves down as far as
/// its baseline lies above the lowest of them in its row, and its row grows to hold it so (CSS
/// Box Alignment 3, §9.3, as in Chrome). An item without a line, or with layout containment, has
/// its baseline at the bottom of its border box. An item orthogonal to the grid has its
/// baseline synthesized from its border box; one of another writing mode along the same axis
/// aligns with the items by their last baselines. In vertical writing modes the grid lays out
/// turned, and so do its items.
fn baseline_shims<'s>(
    style: &ComputedStyle,
    laid: &mut [Laid],
    style_of: impl Fn(BoxId) -> &'s ComputedStyle,
) {
    let mode = style.writing_mode;
    // An item orthogonal to the grid aligns by the baseline synthesized from its border box,
    // with the items of the grid's writing mode (Blink's `DetermineBaselineWritingMode` and
    // `GetSynthesizedLogicalBaseline`; WPT `grid-self-baseline-horiz-*`, `-003`): the
    // alphabetic baseline at its line-under edge, its bottom (the left in vertical writing
    // modes, the start of `vertical-lr`), or the central one halfway where vertical text stands
    // upright.
    let orthogonal = |item: &ComputedStyle| !item.writing_mode.is_parallel(mode);
    // An item of another writing mode along the same axis, as `vertical-rl` in a `vertical-lr`
    // grid, is in the group of the other baseline at the end of the row (see
    // [`row_baseline_group`]): its first line is the last in the grid's direction.
    let opposite = |item: &ComputedStyle| item.writing_mode != mode && !orthogonal(item);
    let synthesized = |height: Au| {
        if !mode.is_vertical() {
            height
        } else if style.text_orientation != fagus_style::values::TextOrientation::Sideways
            && !mode.is_sideways()
        {
            height / 2
        } else if mode == WritingMode::VerticalLr {
            Au::ZERO
        } else {
            height
        }
    };
    // An item's baseline below the top of its margin box, or for one aligned by its last
    // baseline, the room below that to the bottom of its margin box; with whether it is that.
    let baseline = |laid: &Laid| {
        let item = style_of(laid.item.id);
        let alignment = item_alignment(style, item);
        let last = row_baseline_group(style, item, alignment) == ItemAlignment::LastBaseline;
        (laid.item.rows.1 - laid.item.rows.0 == 1
            && matches!(alignment, ItemAlignment::Baseline | ItemAlignment::LastBaseline)
            && !item.margin.top.is_auto()
            && !item.margin.bottom.is_auto()
            && (item.writing_mode == mode || orthogonal(item) || opposite(item))
            // An item whose height depends on its row's does not take part (CSS Box Alignment 3,
            // §9.3; WPT `css-grid/alignment/grid-align-baseline-005`).
            && !matches!(&item.height, Size::Length(length) if length.fixed().is_none()))
        .then(|| {
            // Layout containment keeps the baselines of the content inside (CSS Containment
            // 2, §3.2); an orthogonal item has none along the column, its baseline is
            // synthesized at the bottom of its border box.
            let own = if orthogonal(item) {
                Some(synthesized(laid.fragment.rect.height))
            } else if item.contain_layout {
                None
            } else if last {
                crate::block::last_alignment_baseline(&laid.fragment, mode)
            } else {
                crate::block::first_baseline(&laid.fragment, mode)
            };
            let baseline = laid.margins.0 + own.unwrap_or(laid.fragment.rect.height);
            if last {
                (
                    laid.fragment.rect.height + laid.margins.0 + laid.margins.1 - baseline,
                    true,
                )
            } else {
                (baseline, false)
            }
        })
    };
    let baselines: Vec<Option<(Au, bool)>> = laid.iter().map(baseline).collect();
    let mut lowest: FxHashMap<(usize, bool), Au> = FxHashMap::default();
    for (laid, baseline) in laid.iter().zip(&baselines) {
        if let Some((baseline, last)) = *baseline {
            let row = lowest.entry((laid.item.rows.0, last)).or_insert(baseline);
            *row = (*row).max(baseline);
        }
    }
    // The shim of an item aligned by its last baseline is the room it leaves below it.
    for (laid, baseline) in laid.iter_mut().zip(baselines) {
        if let Some((baseline, last)) = baseline
            && let Some(&row) = lowest.get(&(laid.item.rows.0, last))
        {
            laid.shim = row - baseline;
        }
    }
}

/// The `align-self` of a baseline-aligned item of a grid container with `container` as the
/// baseline-sharing group it is in along the column: `last baseline` for an item of another
/// writing mode along the same axis aligned by its first baseline, and the other way around
/// (Blink's `DetermineBaselineGroup` for a parallel context). Other alignments stay as they
/// are.
fn row_baseline_group(
    container: &ComputedStyle,
    item: &ComputedStyle,
    alignment: ItemAlignment,
) -> ItemAlignment {
    let other =
        item.writing_mode != container.writing_mode && item.writing_mode.is_parallel(container.writing_mode);
    match alignment {
        ItemAlignment::Baseline if other => ItemAlignment::LastBaseline,
        ItemAlignment::LastBaseline if other => ItemAlignment::Baseline,
        alignment => alignment,
    }
}

/// How far the items of a vertical grid container (of style `style`, laid out turned) that are
/// aligned by their baselines across a column they alone span (`justify-self: baseline`) move
/// from its start, or for those in the group at its end (see [`baseline_group`]) from its end,
/// so that their baselines line up (CSS Box Alignment 3, §9.3, as in Chrome): a vertical item
/// has no baseline along the column, which is synthesized from its border box, at its
/// line-under edge where the grid's text stands sideways and halfway otherwise (Blink's
/// `GetSynthesizedLogicalBaseline`; WPT `grid-self-baseline-vertical-*-004` to `-007`). `None`
/// for the other items, which keep their fallback alignment. `model` gives an item's margins.
fn justify_baseline_shims<'s>(
    style: &ComputedStyle,
    laid: &[Laid],
    style_of: impl Fn(BoxId) -> &'s ComputedStyle,
    model: impl Fn(&ComputedStyle) -> crate::block::BoxModel,
) -> Vec<Option<Au>> {
    let mode = style.writing_mode;
    if !mode.is_vertical() || style.is_rtl() {
        return vec![None; laid.len()];
    }
    let central =
        style.text_orientation != fagus_style::values::TextOrientation::Sideways && !mode.is_sideways();
    // Each item's baseline from the start of its margin box, or for the group at the end, from
    // its end; with that group.
    let baselines: Vec<Option<(Au, bool)>> = laid
        .iter()
        .map(|laid| {
            let item = style_of(laid.item.id);
            let alignment = baseline_group(style, item, justify(style, item));
            let last = alignment == ItemAlignment::LastBaseline;
            let model = model(item);
            (laid.item.columns.1 - laid.item.columns.0 == 1
                && matches!(alignment, ItemAlignment::Baseline | ItemAlignment::LastBaseline)
                && item.writing_mode.is_vertical()
                && !item.margin.left.is_auto()
                && !item.margin.right.is_auto())
            .then(|| {
                let width = laid.fragment.rect.width;
                let synthesized = if central { width / 2 } else { width };
                if last {
                    (model.margin[RIGHT] + width - synthesized, true)
                } else {
                    (model.margin[LEFT] + synthesized, false)
                }
            })
        })
        .collect();
    let mut furthest: FxHashMap<(usize, bool), Au> = FxHashMap::default();
    for (laid, baseline) in laid.iter().zip(&baselines) {
        if let Some((baseline, last)) = *baseline {
            let column = furthest.entry((laid.item.columns.0, last)).or_insert(baseline);
            *column = (*column).max(baseline);
        }
    }
    laid.iter()
        .zip(baselines)
        .map(|(laid, baseline)| {
            let (baseline, last) = baseline?;
            Some(*furthest.get(&(laid.item.columns.0, last))? - baseline)
        })
        .collect()
}

/// An item's position along one axis, if definite, and its span.
type Lines = (Option<usize>, usize);

/// An item laid out at its column width, with its vertical margins and the width it was given,
/// and how far it moves down to align its baseline with the others of its row (see
/// [`baseline_shims`]).
struct Laid {
    item: Placed,
    /// Its place in order-modified document order, which it paints in.
    order: usize,
    fragment: Fragment,
    margins: (Au, Au),
    width: Option<Au>,
    shim: Au,
}

/// An item and the area it occupies: rows and columns as start index and end (exclusive).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placed {
    pub(crate) id: BoxId,
    pub(crate) rows: Span,
    pub(crate) columns: Span,
}

/// The named lines along one axis of a grid: the names of its template (`[name]`) and the lines
/// of its named areas (`name-start`, `name-end`), with the index of the line from zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LineNames(pub(crate) Vec<(Arc<str>, usize)>);

impl LineNames {
    /// The names of a template expanded to `tracks` tracks, and of `areas` along the rows or
    /// the columns.
    pub(crate) fn new(list: &TrackList, tracks: usize, areas: Option<&GridAreas>, rows: bool) -> Self {
        if list.subgrid {
            let mut names = Self::of_subgrid(list, tracks);
            names.0.extend(Self::of_areas(areas, rows).0);
            return names;
        }
        // Lines after repeated tracks moved by as many as were inserted.
        let (at, inserted) = list.auto_repeat.as_ref().map_or((usize::MAX, 0), |repeat| {
            (repeat.at, tracks.saturating_sub(list.tracks.len()))
        });
        let mut names: Vec<(Arc<str>, usize)> = list
            .names
            .iter()
            .map(|(line, name)| (Arc::clone(name), if *line > at { line + inserted } else { *line }))
            .collect();
        // The names inside each repetition, and those after the last.
        if let Some(repeat) = &list.auto_repeat {
            let length = repeat.tracks.len().max(1);
            for count in 0..inserted / length {
                for (line, name) in repeat.names.iter() {
                    names.push((Arc::clone(name), repeat.at + count * length + line));
                }
            }
            for (line, name) in repeat.after.iter() {
                names.push((Arc::clone(name), repeat.at + inserted + line));
            }
        }
        names.extend(Self::of_areas(areas, rows).0);
        Self(names)
    }

    /// The lines of `areas` along the rows or the columns (`name-start`, `name-end`).
    fn of_areas(areas: Option<&GridAreas>, rows: bool) -> Self {
        let mut names = Vec::new();
        for area in areas.map_or(&[][..], |areas| &areas.areas) {
            let (start, end) = if rows { area.rows } else { area.columns };
            names.push((Arc::from(format!("{}-start", area.name)), start));
            names.push((Arc::from(format!("{}-end", area.name)), end));
        }
        Self(names)
    }

    /// The names a `subgrid` value gives the lines of the `tracks` tracks it spans: a
    /// `repeat(auto-fill, …)` repeats as often as the lines leave room for after the other
    /// lists, and names past the last line are left out (CSS Grid 2, §9.3).
    fn of_subgrid(list: &TrackList, tracks: usize) -> Self {
        // A `subgrid` without a parent grid (it spans no tracks) is `none`: its lines have no
        // names (CSS Grid 2, §9).
        if tracks == 0 {
            return Self::default();
        }
        let lines = tracks + 1;
        let mut names: Vec<(Arc<str>, usize)> = list
            .names
            .iter()
            .map(|(line, name)| (Arc::clone(name), *line))
            .collect();
        if let Some(repeat) = &list.auto_repeat {
            let length = repeat.tracks.len().max(1);
            let room = lines.saturating_sub(repeat.at + repeat.after_lists);
            let count = room / length;
            for index in 0..count {
                for (line, name) in repeat.names.iter() {
                    names.push((Arc::clone(name), repeat.at + index * length + line));
                }
            }
            for (line, name) in repeat.after.iter() {
                names.push((Arc::clone(name), repeat.at + count * length + line));
            }
        }
        names.retain(|(_, line)| *line < lines);
        Self(names)
    }

    /// The names with their lines `count` further on, after as many implicit tracks before the
    /// explicit grid.
    pub(crate) fn shifted(self, count: usize) -> Self {
        Self(
            self.0
                .into_iter()
                .map(|(name, line)| (name, line + count))
                .collect(),
        )
    }

    /// The lines of a name, in order.
    fn lines(&self, name: &str) -> Vec<usize> {
        let mut lines: Vec<usize> = self
            .0
            .iter()
            .filter(|(known, _)| &**known == name)
            .map(|(_, line)| *line)
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// The `n`th line of a name (from the end for a negative `n`). Where there are too few, the
    /// implicit lines after the explicit grid (with `explicit` tracks) count as having the name,
    /// those before it as the first line.
    fn nth(&self, name: &str, n: i32, explicit: usize) -> usize {
        let lines = self.lines(name);
        let count = n.unsigned_abs() as usize;
        if n > 0 {
            lines
                .get(count - 1)
                .copied()
                .unwrap_or(explicit + count - lines.len())
        } else {
            lines.len().checked_sub(count).map_or(0, |index| lines[index])
        }
    }
}

/// Which edge of an item a line places.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Start,
    End,
}

/// [`resolve_lines_after`] in a grid without implicit tracks before its explicit ones.
#[cfg(test)]
pub(crate) fn resolve_lines(start: &GridLine, end: &GridLine, explicit: usize, names: &LineNames) -> Lines {
    resolve_lines_after(start, end, (0, explicit), names)
}

/// The start index (if definite) and span of an item along one axis, from its lines, in a grid
/// with `lead` implicit tracks before its explicit ones and `explicit` tracks up to the end of
/// those (the lead counting): positive line numbers count from the first explicit line, negative
/// ones from the last. Names resolve to the lines `names` has, counted from the first line of
/// all (CSS Grid 2, §8.3.1).
pub(crate) fn resolve_lines_after(
    start: &GridLine,
    end: &GridLine,
    (lead, explicit): (usize, usize),
    names: &LineNames,
) -> Lines {
    let line = |line: &GridLine, edge: Edge| -> Option<usize> {
        match line {
            GridLine::Line(n) if *n > 0 => Some((*n - 1) as usize + lead),
            GridLine::Line(n) => Some((explicit as i32 + 1 + n).max(0) as usize),
            // An area's own line, else a line of the name, else the first implicit line.
            GridLine::Area(name) => {
                let suffix = if edge == Edge::Start { "start" } else { "end" };
                let own = names.lines(&format!("{name}-{suffix}"));
                let named = if own.is_empty() { names.lines(name) } else { own };
                Some(named.first().copied().unwrap_or(explicit + 1))
            }
            GridLine::Named(name, n) => Some(names.nth(name, *n, explicit)),
            GridLine::Auto | GridLine::Span(_) | GridLine::SpanNamed(..) => None,
        }
    };
    let max = usize::from(MAX_GRID_TRACKS);
    match (line(start, Edge::Start), line(end, Edge::End)) {
        (Some(a), Some(b)) => (Some(a.min(b)), a.abs_diff(b).clamp(1, max)),
        (Some(a), None) => match end {
            GridLine::Span(span) => (Some(a), *span as usize),
            // To the `n`th line of the name after the start, implicit ones counting.
            GridLine::SpanNamed(name, n) => {
                let after: Vec<usize> = names.lines(name).into_iter().filter(|&line| line > a).collect();
                let n = *n as usize;
                let to = after
                    .get(n - 1)
                    .copied()
                    .unwrap_or(explicit.max(a) + n - after.len());
                (Some(a), (to - a).clamp(1, max))
            }
            _ => (Some(a), 1),
        },
        (None, Some(b)) => match start {
            GridLine::Span(span) => (Some(b.saturating_sub(*span as usize)), *span as usize),
            GridLine::SpanNamed(name, n) => {
                let before: Vec<usize> = names.lines(name).into_iter().filter(|&line| line < b).collect();
                let from = before
                    .len()
                    .checked_sub(*n as usize)
                    .map_or(0, |index| before[index]);
                (Some(from), (b - from).clamp(1, max))
            }
            _ => (Some(b.saturating_sub(1)), 1),
        },
        (None, None) => match (start, end) {
            (GridLine::Span(span), _) | (GridLine::Auto, GridLine::Span(span)) => (None, *span as usize),
            _ => (None, 1),
        },
    }
}

/// How many implicit tracks an item needs before the `explicit` tracks of a grid along one axis:
/// as many as its negative line numbers or a span back from its end line reach before the
/// first line (CSS Grid 2, §8.3).
pub(crate) fn leading(start: &GridLine, end: &GridLine, explicit: usize) -> usize {
    let explicit = i64::try_from(explicit).unwrap_or(i64::MAX);
    let number = |line: &GridLine| match line {
        GridLine::Line(n) if *n > 0 => Some(i64::from(*n) - 1),
        GridLine::Line(n) => Some(explicit + 1 + i64::from(*n)),
        _ => None,
    };
    let first = match (number(start), number(end)) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => match start {
            GridLine::Span(span) => b - i64::from(*span),
            GridLine::Auto => b - 1,
            _ => 0,
        },
        (None, None) => 0,
    };
    usize::try_from(-first)
        .unwrap_or(0)
        .min(usize::from(MAX_GRID_TRACKS))
}

/// The rows that items are placed in at most; further items overlap the last ones.
const MAX_ROWS: usize = 10_000;

/// How many cells placing the items of a grid may look at. Beyond that, items are placed without
/// looking and may overlap, so that grids of huge items cannot take forever.
const PLACEMENT_BUDGET: usize = 1 << 26;

/// Which cells of the grid are taken, row by row.
struct Occupancy {
    columns: usize,
    cells: Vec<bool>,
    /// Cells left to look at.
    budget: usize,
}

impl Occupancy {
    /// Spends the work of looking at an area; false once the budget is spent.
    fn spend(&mut self, area: usize) -> bool {
        let left = self.budget.checked_sub(area);
        self.budget = left.unwrap_or(0);
        left.is_some()
    }

    fn free(&mut self, row: usize, column: usize, rows: usize, columns: usize) -> bool {
        if column + columns > self.columns {
            return false;
        }
        if row + rows >= MAX_ROWS || !self.spend(rows * columns) {
            return true;
        }
        (row..row + rows).all(|row| {
            (column..column + columns).all(|column| {
                !self
                    .cells
                    .get(row * self.columns + column)
                    .copied()
                    .unwrap_or(false)
            })
        })
    }

    fn take(&mut self, row: usize, column: usize, rows: usize, columns: usize) {
        if row + rows >= MAX_ROWS || !self.spend(rows * columns) {
            return;
        }
        let needed = (row + rows) * self.columns;
        if self.cells.len() < needed {
            self.cells.resize(needed, false);
        }
        for row in row..row + rows {
            for column in column..column + columns {
                self.cells[row * self.columns + column] = true;
            }
        }
    }
}

/// The explicit tracks of a template, with an automatic repetition expanded for `available`
/// space. Returns the tracks and the range of repeated tracks of `auto-fit`, which collapse when
/// empty.
pub(crate) fn expand(
    list: &TrackList,
    available: Option<Au>,
    gap: Au,
) -> (Vec<TrackSize>, Option<std::ops::Range<usize>>) {
    expand_in(list, available.map(Room::Available), gap)
}

/// The room tracks repeated automatically have: the available space, which as many as fit
/// take, or a minimum size of the container, which as few as fill it take (CSS Grid 2,
/// §7.2.3.2).
#[derive(Clone, Copy)]
pub(crate) enum Room {
    Available(Au),
    Minimum(Au),
}

/// [`expand`] for the `room` of the container.
pub(crate) fn expand_in(
    list: &TrackList,
    room: Option<Room>,
    gap: Au,
) -> (Vec<TrackSize>, Option<std::ops::Range<usize>>) {
    let available = match room {
        Some(Room::Available(available) | Room::Minimum(available)) => Some(available),
        None => None,
    };
    let mut tracks = list.tracks.to_vec();
    // A `subgrid` value repeats names, not tracks.
    let Some(repeat) = list.auto_repeat.as_ref().filter(|_| !list.subgrid) else {
        return (tracks, None);
    };
    // Each track counts with its maximum size if that is fixed, else its minimum (CSS Grid 2,
    // §7.2.3.2).
    let size = |track: &TrackSize| match sizing(track, available) {
        // A fixed maximum below a fixed minimum is that minimum.
        (Minimum::Fixed(min), Maximum::Fixed(max)) => Some(max.max(min)),
        (_, Maximum::Fixed(value)) | (Minimum::Fixed(value), _) => Some(value),
        _ => None,
    };
    let count = match (available, repeat.tracks.iter().map(size).sum::<Option<Au>>()) {
        (Some(available), Some(repeated)) if repeated + gap * repeat.tracks.len() as i32 > Au::ZERO => {
            let others: Au = tracks.iter().filter_map(size).sum::<Au>() + gap * tracks.len() as i32;
            let unit = repeated + gap * repeat.tracks.len() as i32;
            let count = (available - others + gap).to_px() / unit.to_px();
            let count = if matches!(room, Some(Room::Minimum(_))) {
                count.ceil()
            } else {
                count.floor()
            };
            (count as usize).max(1)
        }
        _ => 1,
    };
    let room = usize::from(MAX_GRID_TRACKS).saturating_sub(tracks.len()) / repeat.tracks.len().max(1);
    let count = count.min(room.max(1));
    let at = repeat.at.min(tracks.len());
    let repeated: Vec<TrackSize> = (0..count).flat_map(|_| repeat.tracks.iter().cloned()).collect();
    let range = at..at + repeated.len();
    tracks.splice(at..at, repeated);
    (tracks, repeat.fit.then_some(range))
}

/// The explicit grid of a container: its columns and rows, as many as its templates and its
/// named areas need (a grid without columns still lays out in one, see [`Layouter::grid`]),
/// and the named lines of both.
pub(crate) fn explicit_grid(
    style: &ComputedStyle,
    columns: &[TrackSize],
    rows: &[TrackSize],
) -> ((usize, usize), (LineNames, LineNames)) {
    let areas = style.grid_template_areas.as_deref();
    let explicit = (
        columns.len().max(areas.map_or(0, |areas| areas.columns)),
        rows.len().max(areas.map_or(0, |areas| areas.rows)),
    );
    let names = (
        LineNames::new(&style.grid_template_columns, columns.len(), areas, false),
        LineNames::new(&style.grid_template_rows, rows.len(), areas, true),
    );
    (explicit, names)
}

/// A grid in its first pass of two, or an item laid out for its height before it is laid out
/// again, counted in `Layouter::provisional_grids` or `Layouter::measured_items` while it lasts.
struct Provisional<'a>(&'a std::cell::Cell<u32>);

impl<'a> Provisional<'a> {
    fn enter(count: &'a std::cell::Cell<u32>) -> Self {
        count.set(count.get() + 1);
        Self(count)
    }
}

impl Drop for Provisional<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl Layouter<'_> {
    /// Lays out the items of grid container `id` whose content box is `width` wide and, if
    /// `height` is given, that tall at least. Returns the fragments, the content height and the
    /// groups of rows the grid may break between.
    pub(crate) fn grid(
        &self,
        id: BoxId,
        children: &[BoxId],
        width: Au,
        height: Option<Au>,
    ) -> (Vec<Fragment>, Au, Vec<ContainerLine>) {
        let (laid, rows) = self.grid_pass(id, children, width, height, None);
        // Items with an aspect ratio that stretch to rows sized by their content contribute the
        // width the rows give them, and the tracks are sized once more, as in Chrome (CSS Grid
        // 2, §11.1). Not in the first pass of a grid around, which only finds the heights of
        // its rows: grids nested in each other would take time growing exponentially with their
        // depth.
        match rows {
            Some(rows) if self.provisional_grids.get() == 0 => {
                self.grid_pass(id, children, width, height, Some(&rows)).0
            }
            _ => laid,
        }
    }

    /// A pass of [`Layouter::grid`], with the row sizes of an earlier pass (`rows`) for items
    /// with an aspect ratio; also returns the row sizes if they need another pass.
    #[allow(clippy::type_complexity, reason = "the result of grid and the rows")]
    fn grid_pass(
        &self,
        id: BoxId,
        children: &[BoxId],
        width: Au,
        height: Option<Au>,
        sized_rows: Option<&[Au]>,
    ) -> ((Vec<Fragment>, Au, Vec<ContainerLine>), Option<Vec<Au>>) {
        let style = &self.tree.get(id).style;
        let gap = |value: &Option<LengthPercentage>| {
            Au::from_px(value.as_ref().map_or(0.0, |gap| gap.resolve(width.to_px())))
        };
        // A subgrid takes the tracks its parent passed down, and its `normal` gaps.
        let [inherited_columns, inherited_rows] = self.inherited(id);
        let inherited_columns = inherited_columns.filter(|_| style.grid_template_columns.subgrid);
        let inherited_rows = inherited_rows.filter(|_| style.grid_template_rows.subgrid);
        let own_gap =
            |value: &Option<LengthPercentage>, inherited: Option<&Inherited>| match (value, inherited) {
                (None, Some(inherited)) => inherited.gap,
                _ => gap(value),
            };
        let column_gap = own_gap(&style.column_gap, inherited_columns.as_deref());
        // A percentage row gap refers to the height: without a definite one, its percentage
        // counts as zero while the rows are sized and refers to the height they give after (as
        // in Chrome, CSS Box Alignment 3, §8.3).
        let row_gap = match (&style.row_gap, inherited_rows.as_deref()) {
            (None, Some(inherited)) => inherited.gap,
            (Some(value), _) => Au::from_px(value.resolve(height.map_or(0.0, Au::to_px))),
            (None, None) => Au::ZERO,
        };
        let given = |inherited: Option<&Inherited>| {
            inherited.map(|inherited| (inherited.sizes.len(), inherited.names.clone()))
        };
        let (given_columns, given_rows) = (
            given(inherited_columns.as_deref()),
            given(inherited_rows.as_deref()).or_else(|| {
                style
                    .grid_template_rows
                    .subgrid
                    .then(|| self.row_span(id))
                    .flatten()
            }),
        );
        let ((column_template, fit), row_template, (explicit, leads), names, placed) = self.grid_items(
            style,
            children,
            (Some(width), height),
            ((column_gap, row_gap), [&given_columns, &given_rows]),
        );
        let columns = placed
            .iter()
            .map(|item| item.columns.1)
            .max()
            .unwrap_or(0)
            .max(explicit.0)
            .max(1);
        let rows = placed
            .iter()
            .map(|item| item.rows.1)
            .max()
            .unwrap_or(0)
            .max(explicit.1);

        // A subgrid in rows places its items in as many rows as it spans, also for its widths.
        self.note_row_spans(&placed, &names.1);

        // Which items take their widths from the height of their rows (see
        // `crate::grid_contribution`): not where the rows come from a parent grid.
        let dependent: Vec<bool> = if inherited_rows.is_none()
            && !placed.iter().any(|item| self.is_subgrid(item.id, Axis::Columns))
        {
            placed
                .iter()
                .map(|item| self.area_dependent(style, item.id))
                .collect()
        } else {
            vec![false; placed.len()]
        };
        let dependent_items = dependent.contains(&true);
        // Whether such an item spans rows that are not fixed.
        let mut unfixed = false;
        // Columns: intrinsic widths, then the free space.
        let implicit = |list: &[TrackSize], index: usize| {
            if list.is_empty() {
                TrackSize::Auto
            } else {
                list[index % list.len()].clone()
            }
        };
        let (column_sizes, column_offsets) = if let Some(inherited) = &inherited_columns {
            (inherited.sizes.clone(), inherited.offsets.clone())
        } else {
            let mut column_tracks: Vec<Track> = (0..columns)
                .map(|index| {
                    let size = column_template
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| implicit(&style.grid_auto_columns, index - column_template.len()));
                    Track::new(&size, Some(width))
                })
                .collect();
            // `auto-fit` tracks without items collapse, with their gaps.
            let collapsed: Vec<bool> = (0..columns)
                .map(|index| {
                    fit.as_ref().is_some_and(|range| range.contains(&index))
                        && !placed
                            .iter()
                            .any(|item| (item.columns.0..item.columns.1).contains(&index))
                })
                .collect();
            for (track, &collapsed) in column_tracks.iter_mut().zip(&collapsed) {
                if collapsed {
                    *track = Track::collapsed();
                }
            }
            let visible = collapsed.iter().filter(|collapsed| !**collapsed).count();
            let column_gaps = column_gap * visible.saturating_sub(1) as i32;
            // `left` and `right` go by the sides: columns start on the right in right-to-left
            // text.
            let justify_content = style.justify_content.sided(!style.is_rtl());
            let flexible: Vec<bool> = column_tracks.iter().map(Track::flexible).collect();
            let mut contributions =
                self.grid_column_contributions(&placed, (&names.0, &names.1), (width, column_gap), &flexible);
            // Items whose widths depend on the height of their rows contribute what they need in
            // fixed rows (or the rows of an earlier pass), as in Chrome.
            if dependent_items {
                let row_size = |index: usize| {
                    if let Some(rows) = sized_rows {
                        return rows.get(index).copied();
                    }
                    let size = row_template
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| implicit(&style.grid_auto_rows, index - row_template.len()));
                    match sizing(&size, height) {
                        (Minimum::Fixed(min), Maximum::Fixed(max)) if min == max => Some(min),
                        _ => None,
                    }
                };
                for ((item, contribution), _) in placed
                    .iter()
                    .zip(contributions.iter_mut())
                    .zip(&dependent)
                    .filter(|(_, dependent)| **dependent)
                {
                    let fixed: Option<Au> = (item.rows.0..item.rows.1).map(row_size).sum();
                    let Some(fixed) = fixed else {
                        unfixed = true;
                        continue;
                    };
                    let area = fixed + row_gap * (item.rows.1 - item.rows.0).saturating_sub(1) as i32;
                    let sizes = self.contributions_in_area(style, item.id, width, area);
                    contribution.1 = sizes;
                    contribution.2 = self.minimum_contribution(
                        item.id,
                        width,
                        sizes.0,
                        spans_flexible(&flexible, item.columns),
                    );
                }
            }
            size_tracks(
                &mut column_tracks,
                &contributions,
                Some(width),
                column_gap,
                column_gaps,
                stretches(justify_content),
            );
            let column_sizes: Vec<Au> = column_tracks.iter().map(|track| track.base).collect();
            let column_space = distribute(
                justify_content,
                (width - column_sizes.iter().copied().sum::<Au>() - column_gaps).clamp_non_negative(),
                visible,
            );
            let column_offsets = offsets(&column_sizes, column_gap, &collapsed, column_space);
            (column_sizes, column_offsets)
        };
        // A first pass that only finds the sizes of the rows: grids inside size their tracks
        // once (see `Layouter::grid`).
        let _provisional =
            (unfixed && sized_rows.is_none()).then(|| Provisional::enter(&self.provisional_grids));
        let span_width =
            |(start, end): Span| column_offsets[end - 1] + column_sizes[end - 1] - column_offsets[start];

        // Items at their column widths, aligned along the row axis. Subgrids get the columns
        // they span; those in rows lay out on their own first, to tell what their items need.
        let mut laid: Vec<Laid> = placed
            .iter()
            .enumerate()
            .filter_map(|(order, item)| {
                let child_style = &self.tree.get(item.id).style;
                let area = span_width(item.columns);
                if self.is_subgrid(item.id, Axis::Columns) {
                    let frame = self.subgrid_frame(item.id, Axis::Columns, area);
                    let own = child_style
                        .column_gap
                        .as_ref()
                        .map(|value| Au::from_px(value.resolve(area.to_px())));
                    let names = LineNames(names_in(&names.0, item.columns));
                    let tracks = tracks_for(
                        (&column_sizes, &column_offsets),
                        item.columns,
                        frame,
                        (column_gap, own),
                        names,
                    );
                    self.pass_down(item.id, Axis::Columns, Some(tracks));
                }
                self.pass_down(item.id, Axis::Rows, None);
                // A subgrid in rows lays out on its own first, in as many rows as it spans.
                if self.is_subgrid(item.id, Axis::Rows) {
                    let names = LineNames(names_in(&names.1, item.rows));
                    self.note_row_span(item.id, (item.rows.1 - item.rows.0, names));
                }
                let width = self.grid_width(style, item.id, area);
                // An item laid out again at the height of its area for the percentage heights
                // inside (see below) is laid out here for its height alone: the grids inside it
                // do not lay out their own such items again, which took time growing
                // exponentially with the depth of such grids in subgrids (whose layouts are
                // not kept, see `crate::memo`).
                let relaid = !self.tree.is_replaced(item.id)
                    && !matches!(self.tree.get(item.id).kind, crate::tree::BoxKind::Table { .. })
                    && self.stretches_in_area(style, item.id)
                    && self.has_percentage_heights(item.id);
                let measured = relaid.then(|| Provisional::enter(&self.measured_items));
                let outcome = self.item(item.id, area, width, None);
                drop(measured);
                let outcome = outcome?;
                let margins = (
                    self.vertical_margin(child_style, area, true),
                    outcome.margin_bottom,
                );
                Some(Laid {
                    item: *item,
                    order,
                    fragment: outcome.fragment,
                    margins,
                    width,
                    shim: Au::ZERO,
                })
            })
            .collect();
        baseline_shims(style, &mut laid, |id| &*self.tree.get(id).style);

        // Rows: the items' heights, then the free space of a fixed or minimum height.
        let row_track = |index: usize| {
            let size = row_template
                .get(index)
                .cloned()
                .unwrap_or_else(|| implicit(&style.grid_auto_rows, index - row_template.len()));
            Track::new(&size, height)
        };
        let flexible_rows: Vec<bool> = if inherited_rows.is_some() {
            Vec::new()
        } else {
            (0..rows).map(|index| row_track(index).flexible()).collect()
        };
        // Whether an item in a row alone makes it grow where its breaks take room from it, as
        // in Chrome (Blink's `MinBlockSizeShouldEncompassIntrinsicSize` for grid items): not in a
        // row of fixed sizes, nor in one of a fixed minimum in a grid whose height is not `auto`.
        // Its content then reaches out of it.
        let growing_rows: Vec<bool> = if inherited_rows.is_some() {
            Vec::new()
        } else {
            (0..rows)
                .map(|index| {
                    let track = row_track(index);
                    track.intrinsic_min()
                        || !matches!(track.max, Maximum::Fixed(_)) && matches!(style.height, Size::Auto)
                })
                .collect()
        };
        let mut contributions: Vec<Contribution> = Vec::with_capacity(laid.len());
        for laid in &laid {
            let reported = self
                .is_subgrid(laid.item.id, Axis::Rows)
                .then(|| self.reported_rows(laid.item.id))
                .flatten();
            if let Some(reported) = reported {
                // A subgrid's items need the rows they span, with its frame at its edges.
                let area = span_width(laid.item.columns);
                let frame = self.subgrid_frame(laid.item.id, Axis::Rows, area);
                let span = laid.item.rows.1 - laid.item.rows.0;
                let own_gap = self.tree.get(laid.item.id).style.row_gap.as_ref();
                let own_gap = own_gap.map(|value| Au::from_px(value.resolve(area.to_px())));
                contributions.extend(edged(
                    reported,
                    (laid.item.rows.0, span),
                    frame,
                    (row_gap, own_gap),
                ));
                continue;
            }
            let outer = laid.fragment.rect.height + laid.margins.0 + laid.margins.1 + laid.shim;
            // An item of `height: auto` and `min-height: auto` across several rows, one of them
            // flexible, needs no more than its margins, border and padding of them at least.
            // A scroll container's automatic minimum is nothing either (CSS Grid 2, §6.6), but
            // not that of a form control, as in Chrome.
            let child_style = &self.tree.get(laid.item.id).style;
            let scroller = crate::sizing::is_scroll_container(child_style)
                && !self.tree.control_widths.contains_key(&laid.item.id)
                && !self.tree.is_replaced(laid.item.id);
            let minimum = if (spans_flexible(&flexible_rows, laid.item.rows)
                && matches!(child_style.height, Size::Auto)
                || scroller)
                && matches!(child_style.min_height, Size::Auto)
            {
                let model = self.box_model(child_style, span_width(laid.item.columns), laid.width);
                let frame =
                    model.border[TOP] + model.border[BOTTOM] + model.padding[TOP] + model.padding[BOTTOM];
                (frame + laid.margins.0 + laid.margins.1).min(outer)
            } else if let Some(min_height) = self.row_minimum(laid.item.id, span_width(laid.item.columns)) {
                (min_height + laid.margins.0 + laid.margins.1 + laid.shim).min(outer)
            } else {
                outer
            };
            contributions.push((laid.item.rows, (outer, outer), minimum));
        }
        if style.grid_template_rows.subgrid && inherited_rows.is_none() {
            // A subgrid without items needs no more of all the rows it spans than its frame,
            // which its parent adds at its edges (see `crate::subgrid::edged`, which ends the
            // span at the subgrid's last row).
            let reported = if contributions.is_empty() {
                vec![((0, usize::MAX), (Au::ZERO, Au::ZERO), Au::ZERO)]
            } else {
                contributions.clone()
            };
            self.report_rows(id, reported);
        }
        // Whether each row has a minimum from its content (`auto`, `min-content`, `fr`, …),
        // which keeps an item spanning it from shrinking below its own content.
        let mut content_minimums = vec![true; rows];
        let mut rerun_height = None;
        let mut final_row_gap = row_gap;
        let (row_sizes, row_offsets, row_gaps) = if let Some(inherited) = &inherited_rows {
            let gaps = row_gap * rows.saturating_sub(1) as i32;
            (inherited.sizes.clone(), inherited.offsets.clone(), gaps)
        } else {
            let mut row_tracks: Vec<Track> = (0..rows).map(row_track).collect();
            // `auto-fit` rows without items collapse, with their gaps, as the columns do.
            let fit = expand(&style.grid_template_rows, height, row_gap)
                .1
                .map(|range| range.start + leads.1..range.end + leads.1);
            let collapsed_rows: Vec<bool> = (0..rows)
                .map(|index| {
                    fit.as_ref().is_some_and(|range| range.contains(&index))
                        && !placed
                            .iter()
                            .any(|item| (item.rows.0..item.rows.1).contains(&index))
                })
                .collect();
            for (track, &collapsed) in row_tracks.iter_mut().zip(&collapsed_rows) {
                if collapsed {
                    *track = Track::collapsed();
                }
            }
            for (minimum, track) in content_minimums.iter_mut().zip(&row_tracks) {
                *minimum = !matches!(track.min, Minimum::Fixed(_));
            }
            let visible_rows = collapsed_rows.iter().filter(|collapsed| !**collapsed).count();
            let row_gaps = row_gap * visible_rows.saturating_sub(1) as i32;
            size_tracks(
                &mut row_tracks,
                &contributions,
                height,
                row_gap,
                row_gaps,
                stretches(style.align_content),
            );
            // The height the rows give the grid where it has none of its own.
            let mut height = height;
            if height.is_none() {
                maximize_indefinite(&mut row_tracks);
                expand_flexible_indefinite(&mut row_tracks, &contributions, row_gap);
                // Flexible rows whose factors add up to less than one take their share of the
                // height they gave the grid, sized once more (CSS Grid 2, §12.1, as in Chrome):
                // two rows of `minmax(0, 0.4fr)` for an item 100px tall make the grid 80px tall,
                // and are 32px each.
                let factors: f32 = row_tracks
                    .iter()
                    .map(|track| match track.max {
                        Maximum::Flex(factor) => factor,
                        _ => 0.0,
                    })
                    .sum();
                if factors > 0.0 && factors < 1.0 {
                    let total = row_tracks.iter().map(|track| track.base).sum::<Au>() + row_gaps;
                    let mut again: Vec<Track> = (0..rows).map(row_track).collect();
                    for (track, &collapsed) in again.iter_mut().zip(&collapsed_rows) {
                        if collapsed {
                            *track = Track::collapsed();
                        }
                    }
                    size_tracks(
                        &mut again,
                        &contributions,
                        Some(total),
                        row_gap,
                        row_gaps,
                        stretches(style.align_content),
                    );
                    row_tracks = again;
                    height = Some(total);
                }
            }
            let row_sizes: Vec<Au> = row_tracks.iter().map(|track| track.base).collect();
            let row_space = height.map_or((Au::ZERO, Au::ZERO), |height| {
                let free = height - row_sizes.iter().copied().sum::<Au>() - row_gaps;
                distribute(style.align_content, free.clamp_non_negative(), visible_rows)
            });
            // The percentage of a row gap refers to the height the rows gave.
            let laid_gap = match &style.row_gap {
                Some(value) if value.fixed().is_none() && height.is_none() => {
                    let content = row_sizes.iter().copied().sum::<Au>() + row_gaps;
                    Au::from_px(value.resolve(content.to_px()))
                }
                _ => row_gap,
            };
            let row_offsets = offsets(&row_sizes, laid_gap, &collapsed_rows, row_space);
            rerun_height = height;
            final_row_gap = laid_gap;
            (row_sizes, row_offsets, row_gaps)
        };
        let row_gap = final_row_gap;
        let span_height = |(start, end): Span| row_offsets[end - 1] + row_sizes[end - 1] - row_offsets[start];
        // The rows sized from the content of items with an aspect ratio size the columns again.
        let again = unfixed.then(|| row_sizes.clone());

        // Stretch and align along the column axis, then place.
        laid.sort_by_key(|laid| laid.item.rows.0);
        let justify_shims = justify_baseline_shims(
            style,
            &laid,
            |id| &*self.tree.get(id).style,
            |style| self.box_model(style, Au::ZERO, None),
        );
        let mut fragments = Vec::with_capacity(laid.len());
        let mut starts = Vec::with_capacity(laid.len());
        let mut boxes = Vec::with_capacity(laid.len());
        let mut orders = Vec::with_capacity(laid.len());
        for (
            Laid {
                item,
                order,
                mut fragment,
                margins,
                width: width_override,
                shim,
            },
            justify_shim,
        ) in laid.into_iter().zip(justify_shims)
        {
            let child_style = &self.tree.get(item.id).style;
            let area = (span_width(item.columns), span_height(item.rows));
            let alignment = item_alignment(style, child_style);
            let subgrid_rows = self.is_subgrid(item.id, Axis::Rows);
            if subgrid_rows {
                let frame = self.subgrid_frame(item.id, Axis::Rows, area.0);
                let own = child_style
                    .row_gap
                    .as_ref()
                    .map(|value| Au::from_px(value.resolve(area.0.to_px())));
                let names = LineNames(names_in(&names.1, item.rows));
                let tracks = tracks_for(
                    (&row_sizes, &row_offsets),
                    item.rows,
                    frame,
                    (row_gap, own),
                    names,
                );
                self.pass_down(item.id, Axis::Rows, Some(tracks));
            }
            let mut stretched = false;
            // `normal` does not stretch a replaced element.
            let replaced = self.tree.is_replaced(item.id);
            // A table counts as stretched also where it is as tall as its area: its rows grow
            // where its breaks take room from it (see `parallel::split_line`).
            let table = matches!(self.tree.get(item.id).kind, crate::tree::BoxKind::Table { .. });
            // Auto margins along the column take the free space of the area, before alignment,
            // and keep the item from stretching (CSS Grid 2, §11.2).
            let auto_margins = (
                child_style.margin.top.is_auto(),
                child_style.margin.bottom.is_auto(),
            );
            // Nor a box whose height follows its width through an aspect ratio.
            let stretches = (alignment == ItemAlignment::Stretch
                || (alignment == ItemAlignment::Normal && !replaced && !self.ratio_item(item.id)))
                && matches!(child_style.height, Size::Auto)
                && auto_margins == (false, false);
            // A stretched item taller than its area shrinks to it, its content overflowing, unless
            // its automatic minimum height keeps it as tall as its content: where it spans a
            // row with a minimum from the content and is not a scroll container (CSS Grid 2,
            // §6.6), as an event clipped to its hours in a calendar's grid.
            let outer = fragment.rect.height + margins.0 + margins.1;
            let content_minimum = matches!(child_style.min_height, Size::Auto)
                && child_style.overflow_y != fagus_style::values::Overflow::Hidden
                && !spans_flexible(&flexible_rows, item.rows)
                && (item.rows.0..item.rows.1).any(|row| content_minimums.get(row).copied().unwrap_or(true));
            let shrinks = stretches && !table && !subgrid_rows && outer > area.1 && !content_minimum;
            // A replaced element stretched along the column takes its width from that height
            // through its ratio, unless it stretches along the row too, as in Chrome.
            let ratio_width = ((replaced || self.ratio_item(item.id)) && stretches)
                .then(|| self.stretched_replaced_width(style, item.id, area, margins))
                .flatten();
            // The height it shrinks to is its own, not a minimum below its taller content.
            self.fixed_item_height.set(shrinks || ratio_width.is_some());
            // An item as tall as its area has the height it would be stretched to; it counts as
            // stretched where it breaks (see the flex items in `block`).
            // Percentage heights inside a stretched item refer to its stretched height, which is
            // definite (CSS Grid 2, §6.1): it is laid out again with it.
            let percentages_inside = stretches
                && !replaced
                && self.measured_items.get() == 0
                && self.has_percentage_heights(item.id);
            if stretches
                && !table
                && !subgrid_rows
                && !percentages_inside
                && fragment.rect.height + margins.0 + margins.1 == area.1
            {
                stretched = true;
            // `height: stretch` fills the area whatever the alignment.
            } else if (subgrid_rows
                || shrinks
                || ratio_width.is_some()
                || percentages_inside && !table
                || stretches
                    && (fragment.rect.height + margins.0 + margins.1 < area.1
                        || table && fragment.rect.height + margins.0 + margins.1 == area.1)
                || matches!(child_style.height, Size::Stretch) && !replaced)
                && let Some(outcome) = self.item(
                    item.id,
                    area.0,
                    ratio_width.or(width_override),
                    Some(area.1 - margins.0 - margins.1),
                )
            {
                fragment = outcome.fragment;
                stretched = true;
            } else if percent_height(child_style)
                && let Some(outcome) = self.item_in(item.id, (area.0, Some(area.1)), width_override, None)
            {
                // Percentage heights refer to the height of the grid area, once the rows are
                // sized (CSS Grid 2, §6.1): the item is laid out again in it, as in Chrome.
                fragment = outcome.fragment;
            }
            self.fixed_item_height.set(false);
            let free_y = area.1 - fragment.rect.height - margins.0 - margins.1;
            let model = self.box_model(child_style, area.0, None);
            let free_x = area.0 - fragment.rect.width - model.margin[LEFT] - model.margin[RIGHT];
            // In right-to-left text the columns go from the right, and items start on the right
            // of their areas.
            let rtl = style.is_rtl();
            let area_left = if rtl {
                width - column_offsets[item.columns.0] - area.0
            } else {
                column_offsets[item.columns.0]
            };
            // Auto margins along the row take the free space of the area, before alignment
            // (CSS Grid 2, §11.2).
            let auto_x = (
                child_style.margin.left.is_auto(),
                child_style.margin.right.is_auto(),
            );
            fragment.rect.x = area_left
                + model.margin[LEFT]
                + match (
                    auto_x,
                    (
                        baseline_group(style, child_style, safe_justify(style, child_style, free_x)),
                        rtl,
                    ),
                ) {
                    ((true, true), _) => free_x.clamp_non_negative() / 2,
                    ((true, false), _) => free_x.clamp_non_negative(),
                    ((false, true), _) => Au::ZERO,
                    // Items aligned by their baselines across a column (see
                    // `justify_baseline_shims`).
                    (_, (ItemAlignment::Baseline, false)) if let Some(justify_shim) = justify_shim => {
                        justify_shim
                    }
                    (_, (ItemAlignment::LastBaseline, false)) if let Some(justify_shim) = justify_shim => {
                        free_x - justify_shim
                    }
                    (_, alignment) => match alignment {
                        (ItemAlignment::Center, _) => free_x / 2,
                        // `last baseline` across falls back to `safe end` (CSS Box Alignment 3,
                        // §4.4).
                        (ItemAlignment::End, false) => free_x,
                        (ItemAlignment::LastBaseline, false) => free_x.clamp_non_negative(),
                        (ItemAlignment::End | ItemAlignment::LastBaseline, true) | (_, false) => Au::ZERO,
                        (_, true) => free_x,
                    },
                };
            fragment.rect.y = row_offsets[item.rows.0]
                + margins.0
                + match (
                    auto_margins,
                    row_baseline_group(
                        style,
                        child_style,
                        crate::block::safe_alignment(style, child_style, free_y),
                    ),
                ) {
                    ((true, true), _) => free_y.clamp_non_negative() / 2,
                    ((true, false), _) => free_y.clamp_non_negative(),
                    ((false, true), _) => Au::ZERO,
                    (_, ItemAlignment::Center) => free_y / 2,
                    (_, ItemAlignment::End) => free_y,
                    (_, ItemAlignment::Baseline) => shim,
                    (_, ItemAlignment::LastBaseline) => free_y - shim,
                    _ => Au::ZERO,
                };
            starts.push(item.rows);
            orders.push(order);
            fragments.push(fragment);
            boxes.push(LineItem {
                id: item.id,
                containing_width: area.0,
                width: width_override,
                containing_height: Some(area.1),
                stretched,
                grows: item.rows.1 - item.rows.0 == 1
                    && growing_rows.get(item.rows.0).is_none_or(|grows| *grows),
                margins,
            });
        }

        // Groups of rows that no item spans across; the grid breaks only between them. The items
        // go by their first rows: those starting above a line end, at most, where the furthest of
        // them does.
        let reach: Vec<usize> = starts
            .iter()
            .scan(0, |reach, &(_, stop)| {
                *reach = stop.max(*reach);
                Some(*reach)
            })
            .collect();
        let spans_over = |end: usize| {
            starts
                .partition_point(|&(start, _)| start < end)
                .checked_sub(1)
                .is_some_and(|last| reach[last] > end)
        };
        let mut lines = Vec::new();
        let mut row = 0;
        let mut index = 0;
        while row < rows {
            let mut end = row + 1;
            while end < rows && spans_over(end) {
                end += 1;
            }
            let first = index;
            while index < starts.len() && starts[index].0 < end {
                index += 1;
            }
            // The items of a group paint in order-modified document order, whatever their rows
            // (CSS Grid 2, §5.4), as in Chrome.
            let mut group: Vec<usize> = (first..index).collect();
            group.sort_by_key(|&at| orders[at]);
            if group.iter().enumerate().any(|(offset, &at)| at != first + offset) {
                let mut taken: Vec<Option<Fragment>> = fragments.drain(first..index).map(Some).collect();
                let reordered: Vec<Fragment> =
                    group.iter().filter_map(|&at| taken[at - first].take()).collect();
                fragments.splice(first..first, reordered);
                let line_items: Vec<LineItem> = group.iter().map(|&at| boxes[at]).collect();
                boxes[first..index].copy_from_slice(&line_items);
            }
            lines.push(ContainerLine {
                items: first..index,
                top: row_offsets[row],
                bottom: row_offsets[end - 1] + row_sizes[end - 1],
                boxes: boxes[first..index].to_vec(),
            });
            row = end;
        }
        if self.notes_grid_tracks(id) {
            self.note_grid_tracks(
                id,
                (
                    AxisTracks {
                        sizes: column_sizes.clone(),
                        offsets: column_offsets.clone(),
                        explicit: explicit.0,
                        lead: leads.0,
                        names: names.0.clone(),
                    },
                    AxisTracks {
                        sizes: row_sizes.clone(),
                        offsets: row_offsets.clone(),
                        explicit: explicit.1,
                        lead: leads.1,
                        names: names.1.clone(),
                    },
                ),
                (
                    width,
                    (row_sizes.iter().copied().sum::<Au>() + row_gaps)
                        .max(height.or(rerun_height).unwrap_or(Au::ZERO)),
                ),
            );
        }
        let content_height =
            (row_sizes.iter().copied().sum::<Au>() + row_gaps).max(rerun_height.unwrap_or(Au::ZERO));
        // A subgrid fills the rows it was given.
        let content_height = match &inherited_rows {
            Some(_) => row_offsets
                .last()
                .zip(row_sizes.last())
                .map_or(content_height, |(offset, size)| *offset + *size),
            None => content_height,
        };
        ((fragments, content_height, lines), again)
    }

    /// The content height of a grid whose height size containment leaves out its items: its
    /// `contain-intrinsic-size`, else the height of its tracks without the items, as in Blink
    /// (CSS Containment 2, §3.1; WPT `contain-size-grid-002`).
    pub(crate) fn contained_grid_height(&self, id: BoxId, width: Au) -> Au {
        let style = &self.tree.get(id).style;
        match style.contain_intrinsic_size.1 {
            Some(height) => Au::from_px(height),
            None if style.size_containment().1 => self.grid(id, &[], width, None).1,
            None => Au::ZERO,
        }
    }

    /// Notes how many rows each subgrid in rows among `placed` spans, with the names of their
    /// lines (`names` of the grid's rows), before the columns are sized: it places its items in
    /// those rows also where its widths are found (WPT `subgrid/standalone-axis-size-007`).
    fn note_row_spans(&self, placed: &[Placed], names: &LineNames) {
        for item in placed {
            if self.is_subgrid(item.id, Axis::Rows) {
                let span_names = LineNames(names_in(names, item.rows));
                self.note_row_span(item.id, (item.rows.1 - item.rows.0, span_names));
            }
        }
    }

    /// The min-content and max-content widths of a grid container's content: its columns sized
    /// for the items' contributions alone.
    pub(crate) fn grid_intrinsic(&self, id: BoxId, style: &ComputedStyle, children: &[BoxId]) -> (Au, Au) {
        // Percentages of gaps count as zero for the intrinsic sizes (`calc(20px + 5%)` is 20px).
        let gap = Au::from_px(style.column_gap.as_ref().map_or(0.0, |gap| gap.resolve(0.0)));
        // A subgrid in rows has as many as it spans in its parent (see `Self::note_row_spans`).
        let given_rows = if self.is_subgrid(id, Axis::Rows) {
            self.row_span(id)
        } else {
            None
        };
        let ((template, _), row_template, (explicit, _), names, placed) = self.grid_items(
            style,
            children,
            (None, None),
            ((gap, Au::ZERO), [&None, &given_rows]),
        );
        self.note_row_spans(&placed, &names.1);
        let columns = placed
            .iter()
            .map(|item| item.columns.1)
            .max()
            .unwrap_or(0)
            .max(explicit.0)
            .max(1);
        let gaps = gap * (columns as i32 - 1);
        let column_size = |index: usize| {
            template.get(index).cloned().unwrap_or_else(|| {
                let auto = &style.grid_auto_columns;
                if auto.is_empty() {
                    TrackSize::Auto
                } else {
                    auto[(index - template.len()) % auto.len()].clone()
                }
            })
        };
        let flexible: Vec<bool> = (0..columns)
            .map(|index| Track::new(&column_size(index), None).flexible())
            .collect();
        let mut contributions =
            self.grid_column_contributions(&placed, (&names.0, &names.1), (Au::ZERO, gap), &flexible);
        // Items whose widths depend on the height of their rows, where those are fixed (see
        // `crate::grid_contribution`).
        let row_gap = Au::from_px(style.row_gap.as_ref().map_or(0.0, |gap| gap.resolve(0.0)));
        let fixed_row = |index: usize| {
            let size = row_template.get(index).cloned().unwrap_or_else(|| {
                let auto = &style.grid_auto_rows;
                if auto.is_empty() {
                    TrackSize::Auto
                } else {
                    auto[(index - row_template.len()) % auto.len()].clone()
                }
            });
            match sizing(&size, None) {
                (Minimum::Fixed(min), Maximum::Fixed(max)) if min == max => Some(min),
                _ => None,
            }
        };
        for (item, contribution) in placed.iter().zip(contributions.iter_mut()) {
            if let Some(minimum) = self.ratio_minimum(item.id) {
                contribution.1 = (contribution.1.0.max(minimum), contribution.1.1.max(minimum));
            }
            if !self.area_dependent(style, item.id) {
                continue;
            }
            let fixed: Option<Au> = (item.rows.0..item.rows.1).map(fixed_row).sum();
            if let Some(fixed) = fixed {
                let area = fixed + row_gap * (item.rows.1 - item.rows.0).saturating_sub(1) as i32;
                let sizes = self.contributions_in_area(style, item.id, Au::ZERO, area);
                contribution.1 = sizes;
                contribution.2 = self.minimum_contribution(item.id, Au::ZERO, sizes.0, false);
            }
        }
        // Under a min-content constraint the `auto` minimums of tracks take the items' minimum
        // contributions, as in Blink (`CalculateIntrinsicMinimumContribution`): nothing for a
        // scroll container, whatever its content or aspect ratio.
        let width = |pick: fn((Au, Au)) -> Au, minimum: bool| {
            let mut tracks: Vec<Track> = (0..columns)
                .map(|index| Track::new(&column_size(index), None))
                .collect();
            // The fixed maximums of the tracks an item spans, with the gaps between them, where
            // all are fixed.
            let fixed_maximum = |(start, end): Span| -> Option<Au> {
                let sum: Option<Au> = tracks[start..end]
                    .iter()
                    .map(|track| match track.max {
                        Maximum::Fixed(value) => Some(value),
                        _ => None,
                    })
                    .sum();
                sum.map(|sum| sum + gap * (end - start - 1) as i32)
            };
            // Under a min-content constraint, an item across a flexible track needs no more of
            // them than its minimum contribution (CSS Grid 2, §11.5, step 4), as in Chrome:
            // nothing where its minimum is automatic (§6.6). One across several tracks of fixed
            // maximums needs its contribution limited by them, but no less than its minimum
            // contribution (§11.5, "limited contribution").
            let items: Vec<_> = contributions
                .iter()
                .map(|&(span, sizes, least)| {
                    let wanted = if minimum && spans_flexible(&flexible, span) {
                        least
                    } else if span.1 - span.0 > 1
                        && let Some(limit) = fixed_maximum(span)
                    {
                        pick(sizes).min(limit).max(least)
                    } else if minimum {
                        least
                    } else {
                        pick(sizes)
                    };
                    (span, (pick(sizes), pick(sizes)), wanted)
                })
                .collect();
            size_tracks(&mut tracks, &items, None, gap, gaps, false);
            tracks
        };
        let total = |tracks: &[Track]| tracks.iter().map(|track| track.base).sum::<Au>() + gaps;
        let min = total(&width(|sizes| sizes.0, true));
        // Under a max-content constraint the flexible tracks grow to hold the items in them,
        // with the size of `1fr` they need most (CSS Grid 2, §12.7): two columns of
        // `minmax(0, 1fr)` are both as wide as the widest item in either.
        let mut tracks = width(|sizes| sizes.1, false);
        let items: Vec<Contribution> = contributions
            .iter()
            .map(|&(span, sizes, _)| (span, (sizes.1, sizes.1), sizes.1))
            .collect();
        expand_flexible_indefinite(&mut tracks, &items, gap);
        let max = total(&tracks);
        (min, max.max(min))
    }

    /// Places the items on the grid: definite positions first, then rows, then auto-placement
    /// (CSS Grid 2, §8.5). With `grid-auto-flow: column` the same steps run with rows and
    /// columns swapped: the cursor moves down the column first and on to new columns.
    pub(crate) fn place(
        &self,
        style: &ComputedStyle,
        order: &[BoxId],
        (explicit_columns, explicit_rows): (usize, usize),
        (column_names, row_names): (&LineNames, &LineNames),
        (lead_columns, lead_rows): (usize, usize),
    ) -> Vec<Placed> {
        let column_flow = style.grid_auto_flow_column;
        // The lines along the axis that the cursor moves along first ("columns") and those along
        // the other one ("rows"), which grows as items need it.
        let resolved: Vec<(BoxId, Lines, Lines)> = order
            .iter()
            .map(|&id| {
                let child = &self.tree.get(id).style;
                let rows = resolve_lines_after(
                    &child.grid_row_start,
                    &child.grid_row_end,
                    (lead_rows, explicit_rows),
                    row_names,
                );
                let columns = resolve_lines_after(
                    &child.grid_column_start,
                    &child.grid_column_end,
                    (lead_columns, explicit_columns),
                    column_names,
                );
                if column_flow {
                    (id, columns, rows)
                } else {
                    (id, rows, columns)
                }
            })
            .collect();
        let explicit_columns = if column_flow {
            explicit_rows
        } else {
            explicit_columns
        };
        let columns = resolved
            .iter()
            .map(|(_, _, (start, span))| start.unwrap_or(0) + span)
            .max()
            .unwrap_or(0)
            .max(explicit_columns);
        let mut grid = Occupancy {
            columns,
            cells: Vec::new(),
            budget: PLACEMENT_BUDGET,
        };
        let mut placed: Vec<Option<Placed>> = vec![None; resolved.len()];
        let mut put =
            |index: usize, row: usize, column: usize, rows: usize, span: usize, grid: &mut Occupancy| {
                let row = row.min(MAX_ROWS.saturating_sub(rows));
                grid.take(row, column, rows, span);
                placed[index] = Some(Placed {
                    id: resolved[index].0,
                    rows: (row, row + rows),
                    columns: (column, column + span),
                });
            };
        // Items with a definite row and column.
        for (index, &(_, (row, rows), (column, span))) in resolved.iter().enumerate() {
            if let (Some(row), Some(column)) = (row, column) {
                put(index, row, column, rows, span, &mut grid);
            }
        }
        // Items with a definite row only, in the first columns that are free.
        for (index, &(_, (row, rows), (column, span))) in resolved.iter().enumerate() {
            if let (Some(row), None) = (row, column) {
                let column = (0..columns)
                    .find(|&column| grid.free(row, column, rows, span))
                    .unwrap_or(0);
                put(index, row, column, rows, span, &mut grid);
            }
        }
        // The others, with a cursor that moves on (or starts over for `dense`).
        let (mut cursor_row, mut cursor_column) = (0, 0);
        for (index, &(_, (row, rows), (column, span))) in resolved.iter().enumerate() {
            if row.is_some() {
                continue;
            }
            if style.grid_auto_flow_dense {
                (cursor_row, cursor_column) = (0, 0);
            }
            match column {
                Some(column) => {
                    if column < cursor_column {
                        cursor_row += 1;
                    }
                    while !grid.free(cursor_row, column, rows, span) {
                        cursor_row += 1;
                    }
                    put(index, cursor_row, column, rows, span, &mut grid);
                    cursor_column = column + span;
                }
                None => loop {
                    if cursor_column + span > columns.max(span) {
                        cursor_row += 1;
                        cursor_column = 0;
                        continue;
                    }
                    if grid.free(cursor_row, cursor_column, rows, span.min(columns)) {
                        put(
                            index,
                            cursor_row,
                            cursor_column,
                            rows,
                            span.min(columns),
                            &mut grid,
                        );
                        cursor_column += span;
                        break;
                    }
                    cursor_column += 1;
                },
            }
        }
        placed
            .into_iter()
            .flatten()
            .map(|placed| {
                if column_flow {
                    Placed {
                        rows: placed.columns,
                        columns: placed.rows,
                        ..placed
                    }
                } else {
                    placed
                }
            })
            .collect()
    }

    /// The content width of a grid item that does not stretch along the row axis, or that does
    /// with an automatic width: `None` for items with a width of their own.
    fn grid_width(&self, container: &ComputedStyle, child: BoxId, area: Au) -> Option<Au> {
        let style = &self.tree.get(child).style;
        if !matches!(style.width, Size::Auto) {
            return None;
        }
        // `normal` is `start` for a replaced element: it keeps its own size (CSS Box Alignment 3,
        // §6.1).
        if self.tree.is_replaced(child) && justify(container, style) == ItemAlignment::Normal {
            return None;
        }
        // So does a box with an aspect ratio and a height of its own: its width follows that
        // through the ratio, also in a vertical writing mode, where the width is its block size.
        if (self.ratio_item(child)
            || style.is_vertical() && !self.tree.is_replaced(child) && crate::ratio::ratio(style).is_some())
            && justify(container, style) == ItemAlignment::Normal
            && !matches!(style.height, Size::Auto)
        {
            return None;
        }
        let model = self.box_model(style, area, None);
        let border_padding =
            model.border[LEFT] + model.border[RIGHT] + model.padding[LEFT] + model.padding[RIGHT];
        let frame = border_padding + model.margin[LEFT] + model.margin[RIGHT];
        let auto_margins = style.margin.left.is_auto() || style.margin.right.is_auto();
        let outer = match justify(container, style) {
            _ if auto_margins => {
                let (min, max) = self.intrinsic(child);
                max.min(area).max(min)
            }
            // Fit-content: no narrower than its min-content width, however narrow the area (CSS
            // Box Alignment 3, §6.1). Baseline alignment does not stretch either.
            ItemAlignment::Start
            | ItemAlignment::End
            | ItemAlignment::Center
            | ItemAlignment::Baseline
            | ItemAlignment::LastBaseline => {
                let (min, max) = self.intrinsic(child);
                max.min(area).max(min)
            }
            _ => area,
        };
        // `calc-size()` of the width an automatic one would be.
        let content = crate::sizing::calculated(
            style,
            0,
            (outer - frame).clamp_non_negative(),
            border_padding,
            area,
        );
        Some(self.clamped_width(child, content, area, area))
    }
}

/// The `justify-self` of a baseline-aligned item of a grid container with `container` as the
/// baseline-sharing group it is in: `baseline` for the group at the start of its column, `last
/// baseline` for the one at the end. Which it is goes by the direction the item's baselines run
/// across the column (the block flow of an orthogonal item, as `vertical-rl` lines stack from
/// the right) and by the container's direction, as in Blink's `DetermineBaselineGroup`; an item
/// alone in its group takes its fallback alignment, the start or end (WPT
/// `css-grid/alignment/grid-justify-baseline-001`). Other alignments stay as they are.
pub(crate) fn baseline_group(
    container: &ComputedStyle,
    item: &ComputedStyle,
    alignment: ItemAlignment,
) -> ItemAlignment {
    let last = match alignment {
        ItemAlignment::Baseline => false,
        ItemAlignment::LastBaseline => true,
        other => return other,
    };
    let ltr = !container.is_rtl();
    // The writing mode of the baselines across the column: the item's own where it is
    // orthogonal to the container, else one orthogonal to the item's.
    let mode = if item.writing_mode.is_parallel(container.writing_mode) {
        match (item.writing_mode, ltr) {
            (WritingMode::HorizontalTb, true) => WritingMode::VerticalLr,
            (WritingMode::HorizontalTb, false) => WritingMode::VerticalRl,
            _ => WritingMode::HorizontalTb,
        }
    } else {
        item.writing_mode
    };
    let at_start = match mode {
        WritingMode::VerticalRl | WritingMode::SidewaysRl => !ltr,
        _ => ltr,
    };
    if at_start == last {
        ItemAlignment::LastBaseline
    } else {
        ItemAlignment::Baseline
    }
}

/// `justify-self`, or the container's `justify-items` for `auto`.
pub(crate) fn justify(container: &ComputedStyle, item: &ComputedStyle) -> ItemAlignment {
    match item.justify_self {
        ItemAlignment::Auto => container.justify_items,
        other => other,
    }
}

/// An item's `justify-self` (see [`justify`]) where it has `free` room, which is negative
/// where it overflows, as the `start` or `end` of the container's direction: `left` and
/// `right` go by the sides, `self-start` and `self-end` by the item's own direction, and `safe`
/// alignment of an item that overflows is `start` (CSS Box Alignment 3, §4.4, §6.1).
pub(crate) fn safe_justify(container: &ComputedStyle, item: &ComputedStyle, free: Au) -> ItemAlignment {
    use fagus_style::values::JustifyPosition;
    let alignment = justify(container, item);
    let written = match item.justify_self {
        ItemAlignment::Auto => container.justify_items_block,
        _ => item.justify_self_block,
    };
    let rtl = container.is_rtl();
    let alignment = match (alignment, written.position) {
        (ItemAlignment::Start | ItemAlignment::End, JustifyPosition::Left) => {
            if rtl {
                ItemAlignment::End
            } else {
                ItemAlignment::Start
            }
        }
        (ItemAlignment::Start | ItemAlignment::End, JustifyPosition::Right) => {
            if rtl {
                ItemAlignment::Start
            } else {
                ItemAlignment::End
            }
        }
        (ItemAlignment::Start | ItemAlignment::End, JustifyPosition::SelfStart) => {
            if item.is_rtl() == rtl {
                ItemAlignment::Start
            } else {
                ItemAlignment::End
            }
        }
        (ItemAlignment::Start | ItemAlignment::End, JustifyPosition::SelfEnd) => {
            if item.is_rtl() == rtl {
                ItemAlignment::End
            } else {
                ItemAlignment::Start
            }
        }
        (alignment, _) => alignment,
    };
    if free < Au::ZERO
        && written.safe == Some(true)
        && matches!(alignment, ItemAlignment::Center | ItemAlignment::End)
    {
        ItemAlignment::Start
    } else {
        alignment
    }
}

/// The start of each track, with `gap` between tracks that are not collapsed. `space` is where
/// `justify-content` or `align-content` puts the first track and what it adds between tracks.
fn offsets(sizes: &[Au], gap: Au, collapsed: &[bool], (start, between): (Au, Au)) -> Vec<Au> {
    let mut offsets = Vec::with_capacity(sizes.len());
    let mut position = start;
    let mut seen = false;
    for (index, &size) in sizes.iter().enumerate() {
        if !collapsed[index] {
            if seen {
                position += gap + between;
            }
            seen = true;
        }
        offsets.push(position);
        position += size;
    }
    offsets
}

/// Whether `auto` tracks stretch into the free space: only when `justify-content` or
/// `align-content` does not distribute it otherwise (CSS Grid 2, §11.8).
fn stretches(alignment: ContentAlignment) -> bool {
    matches!(alignment, ContentAlignment::Normal | ContentAlignment::Stretch)
}

/// Grows flexible tracks where the free space is indefinite (CSS Grid 2, §12.7.1): the size of
/// `1fr` is the largest that a track's base size (by its factor, if above one) or an item in
/// flexible tracks (its max-content contribution, less the other tracks it spans, by their
/// factors) needs; each flexible track gets as many times that as its factor.
fn expand_flexible_indefinite(tracks: &mut [Track], items: &[Contribution], gap: Au) {
    let factor = |track: &Track| match track.max {
        Maximum::Flex(factor) => Some(factor),
        _ => None,
    };
    // The size of `1fr` in units of 1/64 pixel.
    let mut fraction: f32 = 0.0;
    for track in tracks.iter() {
        if let Some(factor) = factor(track) {
            fraction = fraction.max(track.base.0 as f32 / factor.max(1.0));
        }
    }
    for &((start, end), (_, max), _) in items {
        let span = &tracks[start..end];
        let factors: f32 = span.iter().filter_map(factor).sum();
        if !span.iter().any(Track::flexible) {
            continue;
        }
        let fixed: Au = span
            .iter()
            .filter(|track| !track.flexible())
            .map(|track| track.base)
            .sum();
        let leftover = max - fixed - gap * (end - start - 1) as i32;
        fraction = fraction.max(leftover.0.max(0) as f32 / factors.max(1.0));
    }
    let flexible: Vec<bool> = tracks.iter().map(|track| factor(track).is_some()).collect();
    grow_by_fraction(tracks, fraction, &flexible);
}

/// Grows the flexible tracks marked in `eligible` to `fraction` (the size of `1fr` in units of
/// 1/64 pixel) times their factor, as Chrome does: each cut to whole units, the fractions cut
/// off carried on to the next track, so that the tracks fill the space between them (Blink's
/// `GridTrackSizingAlgorithm::ExpandFlexibleTracks`). Equal columns of `repeat(5, 1fr)` in
/// 710.6875 px are 142.125, 142.140625, 142.140625, 142.140625 and 142.125 px wide.
fn grow_by_fraction(tracks: &mut [Track], fraction: f32, eligible: &[bool]) {
    let mut carried: f32 = 0.0;
    for (track, &eligible) in tracks.iter_mut().zip(eligible) {
        let Maximum::Flex(factor) = track.max else {
            continue;
        };
        if !eligible {
            continue;
        }
        let share = fraction * factor + carried;
        // Blink adds an epsilon to round up values very close to the next unit.
        let units = (share + f32::EPSILON).clamp(-(i32::MAX / 2) as f32, (i32::MAX / 2) as f32) as i32;
        if units >= track.base.0 {
            track.base = Au(units);
            carried = (share - units as f32).max(0.0);
        }
    }
}

/// Grows the base sizes of the tracks `targets` of `span` by `missing` together: in equal shares,
/// each up to its growth limit (where a track has one), the rest in equal shares past them (CSS
/// Grid 2, §11.5.1, "distribute space up to limits"), as in Chrome, where a spanned track that
/// holds an item of its own leaves what the spanning item needs more to the others.
fn grow_bases(span: &mut [Track], targets: &[usize], missing: Au) {
    let room = |track: &Track| track.limit.map(|limit| (limit - track.base).clamp_non_negative());
    let mut open: Vec<usize> = targets.to_vec();
    let mut left = missing;
    // Tracks whose room is less than an equal share take their room and stop.
    loop {
        if open.is_empty() {
            break;
        }
        let share = left / open.len() as i32;
        let full: Vec<usize> = open
            .iter()
            .copied()
            .filter(|&index| room(&span[index]).is_some_and(|room| room < share))
            .collect();
        if full.is_empty() {
            for (index, share) in shares(left, &open) {
                span[index].base += share;
            }
            return;
        }
        for index in full {
            let grow = room(&span[index]).unwrap_or(Au::ZERO);
            span[index].base += grow;
            left -= grow;
            open.retain(|&other| other != index);
        }
    }
    // All tracks reached their limits: the rest goes past them.
    if left > Au::ZERO && !targets.is_empty() {
        for (index, share) in shares(left, targets) {
            span[index].base += share;
        }
    }
}

/// `space` in equal shares over the tracks `targets`, as Chrome splits it (Blink's
/// `DistributeExtraSpaceToSets`): each takes what is left divided by the tracks left, so that the
/// shares add up to all of it, the last ones a layout unit more where it does not divide.
fn shares(space: Au, targets: &[usize]) -> impl Iterator<Item = (usize, Au)> + '_ {
    let mut left = space;
    targets.iter().enumerate().map(move |(position, &index)| {
        let share = left / (targets.len() - position) as i32;
        left -= share;
        (index, share)
    })
}

/// Grows the growth limits of the tracks `targets` of `span` by `missing` together, in equal
/// shares: only those of the tracks that are `growable` where there are any (the others are
/// frozen at their limits), else those of all targets (CSS Grid 2, §11.5.1), as in Chrome, where
/// a row of `minmax(40px, auto)` that holds an item of its own leaves what an item spanning it
/// needs more to the rows that do not.
fn grow_limits(span: &mut [Track], targets: &[usize], growable: &[bool], missing: Au) {
    let open: Vec<usize> = targets
        .iter()
        .copied()
        .filter(|&index| growable.get(index).copied().unwrap_or(false))
        .collect();
    let open = if open.is_empty() { targets.to_vec() } else { open };
    for (index, share) in shares(missing, &open) {
        let limit = span[index].limit.unwrap_or(span[index].base);
        span[index].limit = Some(limit + share);
    }
}

/// Grows the tracks to their growth limits where the free space is indefinite, as for the rows
/// of a grid without a definite height: the free space then counts as infinite (CSS Grid 2,
/// §12.6), so that a row of `minmax(40px, auto)` is as tall as its items, as in Chrome.
fn maximize_indefinite(tracks: &mut [Track]) {
    for track in tracks.iter_mut().filter(|track| !track.flexible()) {
        track.base = track.base.max(track.limit.unwrap_or(track.base));
    }
}

/// Sizes tracks from the items' min-content and max-content contributions (CSS Grid 2, §11.3 to
/// §11.8, simplified): intrinsic sizes, then free space up to the growth limits, then flexible
/// tracks, then `auto` tracks stretched if `stretch` says so. `gaps` is the total size of the
/// gaps between tracks.
fn size_tracks(
    tracks: &mut [Track],
    items: &[Contribution],
    available: Option<Au>,
    gap: Au,
    gaps: Au,
    stretch: bool,
) {
    let raise =
        |limit: &mut Option<Au>, value: Au| *limit = Some(limit.map_or(value, |limit| limit.max(value)));
    // Items in one track.
    for &((start, end), (min, max), minimum) in items {
        if end - start != 1 {
            continue;
        }
        let track = &mut tracks[start];
        match track.min {
            Minimum::MinContent => track.base = track.base.max(min),
            // The automatic minimum stays within a fixed maximum (CSS Grid 2, §6.6).
            Minimum::Auto => {
                let minimum = match track.max {
                    Maximum::Fixed(limit) => minimum.min(limit),
                    _ => minimum,
                };
                track.base = track.base.max(minimum);
            }
            Minimum::MaxContent => track.base = track.base.max(max),
            Minimum::Fixed(_) => {}
        }
        match track.max {
            Maximum::MinContent => raise(&mut track.limit, min),
            Maximum::MaxContent | Maximum::Auto => raise(&mut track.limit, max),
            Maximum::FitContent(clamp) => raise(&mut track.limit, max.min(clamp.max(min))),
            Maximum::Fixed(_) | Maximum::Flex(_) => {}
        }
    }
    // Items across several tracks: what the tracks lack goes to those with intrinsic sizes.
    let mut spanning: Vec<_> = items
        .iter()
        .filter(|((start, end), _, _)| end - start > 1)
        .collect();
    spanning.sort_by_key(|((start, end), _, _)| end - start);
    // Tracks without a growth limit from an item of their own grow theirs without bound for the
    // items that span them (they are "infinitely growable", CSS Grid 2, §11.5).
    // The others' growth limits are at least their base sizes (§11.5, step 2).
    let growable: Vec<bool> = tracks.iter().map(|track| track.limit.is_none()).collect();
    for track in tracks.iter_mut() {
        track.limit = track.limit.map(|limit| limit.max(track.base));
    }
    for &&((start, end), (min, max), minimum) in &spanning {
        let span = &mut tracks[start..end];
        let inner_gaps = gap * (end - start - 1) as i32;
        let flexible = span.iter().any(Track::flexible);
        let targets: Vec<usize> = (0..span.len())
            .filter(|&index| span[index].intrinsic_min() && (!flexible || span[index].flexible()))
            .collect();
        if !targets.is_empty() {
            // Only `auto` minimums take the minimum contribution.
            let wanted = if targets.iter().all(|&index| span[index].min == Minimum::Auto) {
                minimum
            } else {
                min
            };
            let missing = wanted - span.iter().map(|track| track.base).sum::<Au>() - inner_gaps;
            if missing > Au::ZERO {
                grow_bases(span, &targets, missing);
                for track in span.iter_mut() {
                    track.limit = track.limit.map(|limit| limit.max(track.base));
                }
            }
        }
        // The growth limits of the tracks with intrinsic maximums grow for the item's
        // max-content contribution.
        let limit_targets: Vec<usize> = (0..span.len())
            .filter(|&index| span[index].intrinsic_max())
            .collect();
        if !flexible && !limit_targets.is_empty() {
            let limits: Au = span.iter().map(|track| track.limit.unwrap_or(track.base)).sum();
            let missing = max - limits - inner_gaps;
            if missing > Au::ZERO {
                grow_limits(span, &limit_targets, &growable[start..end], missing);
            }
        }
    }
    for track in tracks.iter_mut() {
        track.limit = Some(track.limit.unwrap_or(track.base).max(track.base));
    }
    let Some(available) = available else {
        return;
    };
    // Free space grows the tracks up to their limits.
    let mut free = available - tracks.iter().map(|track| track.base).sum::<Au>() - gaps;
    while free > Au::ZERO {
        let growing: Vec<usize> = (0..tracks.len())
            .filter(|&index| {
                !tracks[index].flexible()
                    && tracks[index]
                        .limit
                        .is_some_and(|limit| limit > tracks[index].base)
            })
            .collect();
        if growing.is_empty() {
            break;
        }
        let share = (free / growing.len() as i32).max(Au::EPSILON);
        for index in growing {
            let track = &mut tracks[index];
            let room = track.limit.unwrap_or(track.base) - track.base;
            let grow = share.min(room).min(free);
            track.base += grow;
            free -= grow;
            if free <= Au::ZERO {
                break;
            }
        }
    }
    // Flexible tracks share the rest by their factors, never below their base size.
    if tracks.iter().any(Track::flexible) {
        let mut inflexible: Vec<bool> = tracks.iter().map(|track| !track.flexible()).collect();
        let fraction = loop {
            let leftover = available
                - gaps
                - tracks
                    .iter()
                    .zip(&inflexible)
                    .filter(|(_, fixed)| **fixed)
                    .map(|(track, _)| track.base)
                    .sum::<Au>();
            let factors: f32 = tracks
                .iter()
                .zip(&inflexible)
                .filter(|(_, fixed)| !**fixed)
                .map(|(track, _)| match track.max {
                    Maximum::Flex(factor) => factor,
                    _ => 0.0,
                })
                .sum();
            // In units of 1/64 pixel, as Chrome finds it.
            let fraction = leftover.0.max(0) as f32 / factors.max(1.0);
            let mut changed = false;
            for (track, fixed) in tracks.iter().zip(inflexible.iter_mut()) {
                if let Maximum::Flex(factor) = track.max
                    && !*fixed
                    && fraction * factor < track.base.0 as f32
                {
                    *fixed = true;
                    changed = true;
                }
            }
            if !changed {
                break fraction;
            }
        };
        let eligible: Vec<bool> = inflexible.iter().map(|fixed| !fixed).collect();
        grow_by_fraction(tracks, fraction, &eligible);
    } else if stretch {
        // `auto` tracks stretch into what is left.
        let free = available - tracks.iter().map(|track| track.base).sum::<Au>() - gaps;
        let auto: Vec<usize> = (0..tracks.len())
            .filter(|&index| matches!(tracks[index].max, Maximum::Auto))
            .collect();
        if free > Au::ZERO && !auto.is_empty() {
            let share = free / auto.len() as i32;
            for index in auto {
                tracks[index].base += share;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(value: f32) -> Au {
        Au::from_px(value)
    }

    #[test]
    fn resolves_lines_and_spans() {
        let none = LineNames::default();
        let resolve = |start: GridLine, end: GridLine| resolve_lines(&start, &end, 3, &none);
        assert_eq!(resolve(GridLine::Line(2), GridLine::Line(4)), (Some(1), 2));
        assert_eq!(resolve(GridLine::Line(1), GridLine::Line(-1)), (Some(0), 3));
        assert_eq!(resolve(GridLine::Span(2), GridLine::Auto), (None, 2));
        assert_eq!(resolve(GridLine::Auto, GridLine::Span(3)), (None, 3));
        assert_eq!(resolve(GridLine::Line(3), GridLine::Span(2)), (Some(2), 2));
        assert_eq!(resolve(GridLine::Span(2), GridLine::Line(4)), (Some(1), 2));
    }

    #[test]
    fn resolves_named_lines_and_areas() {
        // Lines `[full] … [main] … [main] … [full]`, and an area `head` over columns 2 to 4.
        let names = LineNames(vec![
            (Arc::from("full"), 0),
            (Arc::from("main"), 1),
            (Arc::from("main"), 3),
            (Arc::from("full"), 4),
            (Arc::from("head-start"), 1),
            (Arc::from("head-end"), 3),
        ]);
        let resolve = |start: GridLine, end: GridLine| resolve_lines(&start, &end, 4, &names);
        let name = |name: &str| Arc::<str>::from(name);
        assert_eq!(
            resolve(GridLine::Area(name("head")), GridLine::Area(name("head"))),
            (Some(1), 2)
        );
        assert_eq!(
            resolve(GridLine::Area(name("full")), GridLine::Named(name("full"), -1)),
            (Some(0), 4)
        );
        assert_eq!(
            resolve(GridLine::Named(name("main"), 2), GridLine::Auto),
            (Some(3), 1)
        );
        assert_eq!(
            resolve(GridLine::Line(1), GridLine::SpanNamed(name("main"), 2)),
            (Some(0), 3)
        );
        assert_eq!(
            resolve(GridLine::SpanNamed(name("full"), 1), GridLine::Line(3)),
            (Some(0), 2)
        );
        // A name without lines: the first implicit line after the explicit grid.
        assert_eq!(
            resolve(GridLine::Area(name("nowhere")), GridLine::Auto),
            (Some(5), 1)
        );
    }

    #[test]
    fn sizes_fractions_and_content() {
        // `repeat(3, minmax(0, 1fr))` with 10px gaps in 320px: three equal columns.
        let zero_fr = TrackSize::MinMax(
            Box::new(TrackSize::Length(LengthPercentage::Px(0.0))),
            Box::new(TrackSize::Fraction(1.0)),
        );
        let mut tracks: Vec<Track> = (0..3).map(|_| Track::new(&zero_fr, Some(px(320.0)))).collect();
        let items = [((0, 1), (px(150.0), px(200.0)), px(150.0))];
        size_tracks(&mut tracks, &items, Some(px(320.0)), px(10.0), px(20.0), true);
        assert_eq!(
            tracks.iter().map(|track| track.base).collect::<Vec<_>>(),
            [px(100.0); 3]
        );

        // `1fr` keeps its min-content; `auto` takes the max-content and `200px` is fixed.
        let mut tracks = vec![
            Track::new(&TrackSize::Fraction(1.0), None),
            Track::new(&TrackSize::Auto, None),
            Track::new(&TrackSize::Length(LengthPercentage::Px(200.0)), None),
        ];
        let items = [
            ((0, 1), (px(150.0), px(300.0)), px(150.0)),
            ((1, 2), (px(40.0), px(80.0)), px(40.0)),
        ];
        size_tracks(&mut tracks, &items, Some(px(500.0)), Au::ZERO, Au::ZERO, true);
        assert_eq!(
            tracks.iter().map(|track| track.base).collect::<Vec<_>>(),
            [px(220.0), px(80.0), px(200.0)]
        );
    }

    #[test]
    fn carries_the_units_cut_off_flexible_tracks_to_the_next_as_chrome() {
        // `repeat(5, 1fr)` with gaps of 725 units in 756 px (Chrome 157, measured with
        // `getBoundingClientRect`): 45484 units for five columns of 9096.8, cut to whole units
        // with the rest carried on.
        let fr = TrackSize::Fraction(1.0);
        let mut tracks: Vec<Track> = (0..5).map(|_| Track::new(&fr, Some(px(756.0)))).collect();
        let items: Vec<_> = (0..5)
            .map(|index| ((index, index + 1), (Au::ZERO, Au::ZERO), Au::ZERO))
            .collect();
        size_tracks(&mut tracks, &items, Some(px(756.0)), Au(725), Au(4 * 725), true);
        assert_eq!(
            tracks.iter().map(|track| track.base.0).collect::<Vec<_>>(),
            [9096, 9097, 9097, 9097, 9096]
        );
    }

    #[test]
    fn auto_minimums_take_the_minimum_contribution() {
        // `1fr 50px` in 150px, the first item with `min-width: 0` and a wide min-content.
        let mut tracks = vec![
            Track::new(&TrackSize::Fraction(1.0), None),
            Track::new(&TrackSize::Length(LengthPercentage::Px(50.0)), None),
        ];
        let items = [((0, 1), (px(186.0), px(186.0)), Au::ZERO)];
        size_tracks(&mut tracks, &items, Some(px(150.0)), Au::ZERO, Au::ZERO, true);
        assert_eq!(tracks[0].base, px(100.0));

        // `minmax(auto, 50px)`: the automatic minimum stays within the fixed maximum.
        let mut tracks = vec![Track::new(
            &TrackSize::MinMax(
                Box::new(TrackSize::Auto),
                Box::new(TrackSize::Length(LengthPercentage::Px(50.0))),
            ),
            None,
        )];
        let items = [((0, 1), (px(68.0), px(300.0)), px(68.0))];
        size_tracks(&mut tracks, &items, Some(px(400.0)), Au::ZERO, Au::ZERO, false);
        assert_eq!(tracks[0].base, px(50.0));
    }

    #[test]
    fn repeats_tracks_that_fit() {
        let list = TrackList {
            subgrid: false,
            tracks: Arc::from([]),
            names: Arc::from([]),
            auto_repeat: Some(fagus_style::values::AutoRepeat {
                at: 0,
                tracks: Arc::from([TrackSize::MinMax(
                    Box::new(TrackSize::Length(LengthPercentage::Px(100.0))),
                    Box::new(TrackSize::Fraction(1.0)),
                )]),
                fit: false,
                names: Arc::from([]),
                after: Arc::from([]),
                after_lists: 0,
            }),
        };
        // 100px tracks with 10px gaps in 350px: three fit.
        let (tracks, _) = expand(&list, Some(px(350.0)), px(10.0));
        assert_eq!(tracks.len(), 3);
    }
}
