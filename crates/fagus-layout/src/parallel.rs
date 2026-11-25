//! Lines of boxes side by side that break across pages together: the lines of a row flex
//! container, the groups of rows of a grid and of a table (CSS Fragmentation 3, §2, parallel
//! flows).
//!
//! The boxes of a line are laid out unbroken first, which gives each one its place in the line
//! and its height. Where the line reaches past the end of the page, every box that starts on the
//! page is laid out again up to there and breaks inside if it does not fit, as in Chrome; the
//! line then goes on at the top of the next page, where the boxes that broke continue and those
//! that start further down follow at their places in the line. A box that does not fit and has
//! no break inside moves to the next page with the boxes beside it and below it (its row), or
//! the whole line does when that is its first row. Boxes whose height follows the line (stretched
//! flex and grid items, table cells) extend to the end of the line's part on each page.

use crate::block::BreakToken;
use crate::fragment::{Fragment, FragmentKind};
use crate::units::Au;

/// A box of a line, as it is in the unbroken layout.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Piece {
    /// The top of its border box in the line.
    pub(crate) offset: Au,
    /// The height of its border box.
    pub(crate) height: Au,
    pub(crate) margin_bottom: Au,
    /// Whether its height follows the line: it then ends where the line ends, if it reaches
    /// that far, and extends to the end of the page where the line breaks.
    pub(crate) stretched: bool,
    /// Whether it draws its borders at the breaks too (`box-decoration-break: clone`).
    pub(crate) clones: bool,
    /// Whether the line grows where its content needs more room after a break. A box that
    /// spans several rows of a grid or table does not, as in Chrome, nor does one in a grid
    /// row of a fixed size (see `Layouter::grid`): its content then reaches out of it.
    pub(crate) grows: bool,
    /// How far its fragment reaches above and below its border box (table cells with
    /// collapsing borders, which share them with the rows around).
    pub(crate) outset: (Au, Au),
    /// How it grows, if it is stretched, where its breaks leave it less room than its content
    /// needs on its last page (see [`LineBoxes::expand`]).
    pub(crate) growth: Growth,
}

/// How a stretched box of a line grows where the pages it broke on took more room from its
/// area than its content (the empty space at their ends), as in Chrome. Its content then needs
/// more room than its area leaves it on its last page: the box is laid out again taller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Growth {
    /// It does not grow (its content stays as it is).
    None,
    /// As a grid item: its area grows by what its content needs beyond the area's end (its
    /// bottom margin included), and its row ends where the box ended before it grew.
    Grid,
    /// As a flex item: it grows by what its content needs beyond its own end, and the line ends
    /// below the grown box and its bottom margin.
    Flex,
}

/// The part of a line on one page.
#[derive(Debug)]
pub(crate) struct Portion {
    /// The fragments of the boxes on the page by their index in the line, with `rect.y` from
    /// the top of the part.
    pub(crate) fragments: Vec<(usize, Fragment)>,
    pub(crate) height: Au,
    /// Where the line continues on the next page: a token per box (`None` for boxes that are
    /// done or have not started) and the place in the line at the top of the next page.
    pub(crate) next: Option<(Vec<Option<BreakToken>>, Au)>,
    /// Where the boxes that do not span rows end in the part, if the line breaks only between
    /// its rows (no box breaks inside its content).
    pub(crate) rows_end: Option<Au>,
    /// The boxes (by index in the line) whose fragments in the part draw only their borders
    /// (see [`LineBoxes::ends_open`]).
    pub(crate) bare: Vec<usize>,
    /// Where the line ends on the page while boxes of it go on past its end (see
    /// [`split_line`]): a token per box, `None` for the boxes that are done.
    pub(crate) outlived: Option<Vec<Option<BreakToken>>>,
}

/// How a line lays out its boxes on a page.
pub(crate) trait LineBoxes {
    /// Lays out box `index` from `token` on, with `remaining` below the top of its border box.
    /// A box `at_top` of the page always returns something; `None` means that it does not fit.
    fn lay(
        &mut self,
        index: usize,
        token: Option<&BreakToken>,
        remaining: Au,
        at_top: bool,
    ) -> Option<(Fragment, Option<BreakToken>)>;

    /// Whether box `index` broke at `token` only in its bottom padding and border, the last
    /// resort among breaks.
    fn last_resort(&self, index: usize, token: &BreakToken) -> bool;

    /// Whether only the bottom padding and border of box `index` go on at `token` (see
    /// [`LineBoxes::frame_below_rest`]), which a box taller than its content may break before
    /// without it being the last resort.
    fn frame_rest(&self, index: usize, token: &BreakToken) -> bool {
        self.last_resort(index, token)
    }

    /// Whether box `index` avoids breaks inside it (`break-inside: avoid`), which its line then
    /// avoids too.
    fn avoids_breaking(&self, _index: usize) -> bool {
        false
    }

    /// Whether the line breaks as badly as the worst break of its boxes, as a table row does in
    /// Chrome, rather than as well as the best.
    fn worst_break_counts(&self) -> bool {
        false
    }

    /// Whether a line that starts the page (`at_top` in [`split_line`]) has nothing at all
    /// above it there (see [`crate::block::Space::flush`]). A line below only the padding of a
    /// box around it whose first row breaks only in a bottom padding and border moves to the
    /// next page, as Blink breaks before a table row that does not fit unless nothing at all
    /// precedes it (`refuse_break_before` in `MovePastBreakpoint`).
    fn flush(&self) -> bool {
        true
    }

    /// Whether a line that breaks grows to hold the monolithic content of its boxes that go on
    /// in the next fragmentainer and reaches past the end of this one, as a flex line does,
    /// rather than leaving that to the next page (see [`LineBoxes::overflow_page`]): the rows of
    /// a table in columns, whose next column starts at the top (Blink's fragment takes the
    /// block size of its content, `FinishFragmentation`).
    fn grows_to_content(&self) -> bool {
        false
    }

    /// Whether the line lies in the columns of a multi-column container rather than on pages.
    fn in_columns(&self) -> bool {
        false
    }

    /// An empty part of box `index`, for a box whose content is done while its height goes on.
    fn filler(&mut self, index: usize) -> Fragment;

    /// Lays out box `index` again from `token` on (as [`LineBoxes::lay`]) with its height `by`
    /// more than the line gives it, for a stretched box whose content takes that height (a
    /// table, whose rows share it). `None` for boxes that stay as they are.
    fn expand(
        &mut self,
        _index: usize,
        _token: &BreakToken,
        _remaining: Au,
        _at_top: bool,
        _by: Au,
    ) -> Option<(Fragment, Option<BreakToken>)> {
        None
    }

    /// For boxes that end below the rest of a box of the line whose content ended with the
    /// page before, where only its bottom padding and border (see [`LineBoxes::last_resort`])
    /// go on, as table cells do in Chrome: the bottom padding and border of box `index` and
    /// whether its content ends above them in the unbroken line (it is stretched), in which case
    /// it ends below that rest with its own. `None` for boxes that end with the line.
    fn frame_below_rest(&self, _index: usize) -> Option<(Au, bool)> {
        None
    }

    /// For lines that go on below monolithic content reaching past the end of the page (an
    /// image taller than the rest of it), as the rows of a table do in Chrome: the height of a
    /// page, the room the next one has. `None` for lines that do not (see [`split_line`]).
    fn overflow_page(&self) -> Option<Au> {
        None
    }

    /// How far below the end of the room the line has on the page monolithic content of its
    /// boxes may reach without the line going on below it (see [`monolithic_overflow`]): that
    /// of a footer repeated below table rows, which Chrome draws over such content in its place
    /// at the end of the page.
    fn overflow_room(&self) -> Au {
        Au::ZERO
    }

    /// Whether box `index` was laid out whole past the end of the page, its bottom padding and
    /// border with it, as Chrome lays out table cells whose content ends on the page: the box
    /// then overflows as monolithic content does (see [`monolithic_overflow`]).
    fn laid_whole(&self, _index: usize) -> bool {
        false
    }

    /// The bottom padding and border of box `index` as they go on below monolithic overflow
    /// (see [`split_line`]).
    fn bottom_frame(&self, index: usize) -> Au {
        self.frame_below_rest(index).map_or(Au::ZERO, |(frame, _)| frame)
    }

    /// The bottom padding and border of box `index` where it broke at `token` inside them and is
    /// taller than its content: its part on the page ends above them, as in Chrome (Blink's
    /// `FinishFragmentation` breaks before the trailing border and padding where it can).
    fn frame_cut(&self, _index: usize, _token: &BreakToken) -> Au {
        Au::ZERO
    }

    /// Whether box `index` broke at a forced break inside it where it was last laid out, for
    /// lines whose boxes that are done end with the line's part where it breaks only at forced
    /// breaks, as the cells of a table row do in Chrome (see [`split_line`]). Always `false` for
    /// lines whose boxes go on (flex and grid lines).
    fn broke_forced(&self, _index: usize) -> bool {
        false
    }

    /// Moves the content of box `index`, done on the page, as its alignment does where its
    /// fragment grew `by` to end with the line's part (see [`LineBoxes::broke_forced`]).
    fn realign(&self, _index: usize, _fragment: &mut Fragment, _by: Au) {}

    /// Whether box `index`, ended beside a forced break (see [`LineBoxes::broke_forced`]), still
    /// draws its side borders through the line's parts on the pages after, without its
    /// background, as table cells with collapsed borders do in Chrome (the table draws the
    /// borders of its grid in each part of a row).
    fn ends_open(&self, _index: usize) -> bool {
        false
    }

    /// The part of box `index` that [`LineBoxes::ends_open`] draws: its borders without its
    /// background.
    fn bare_filler(&mut self, index: usize) -> Fragment {
        self.filler(index)
    }

    /// Whether the rows that start on a page move down below the boxes of the rows above that go
    /// on from the page before, where those need more room than their rows leave them there, as
    /// the rows of a grid and of a table do (a grid line knows this from its boxes' growth).
    fn shifts_rows(&self) -> bool {
        false
    }
}

/// Where the line goes on below monolithic content of its boxes that reaches past the end of
/// the page, as table rows do in Chrome (Blink's monolithic overflow).
struct Overflow {
    /// Where the lowest such content ends, from the top of the line's part.
    end: Au,
    /// The positions in `placed` of the boxes whose content reaches that far, and their ends.
    boxes: Vec<(usize, Au)>,
    /// How much of the line's height goes on below the overflow, around the boxes that go on
    /// with their content.
    rest: Au,
    /// The positions in `placed` of the boxes whose content is done, which end on the page.
    done: Vec<usize>,
    /// Where the overflow covers the next page and other boxes go on with their content: the
    /// positions of the overflowing boxes whose bottom padding and border go on below that
    /// content on the page after, as in Chrome, and their height.
    below: Vec<(usize, Au)>,
}

/// Finds the boxes of a line's part (`placed`, as in [`split_line`], in `pieces`) whose
/// monolithic content reaches past `available`, the end of the page (their content done there,
/// or going on below it on the next page), and
/// makes the line go on below that content as Chrome does: the part is as tall as the content
/// (Blink's row grows to the cells laid out whole), and the next page starts below the overflow
/// (a table moves its rows below it, see `BreakToken::Table`, and the page loop the content
/// after a line that ends, see `monolithic`). There the line goes on
/// with what is left of the other boxes: their bottom padding and border (stretched to the end
/// of the page, they break before them), or as much as the line's height has left if that is
/// more; the box with the overflow keeps its bottom padding and border only as far as the
/// line's height has room for them. Where the overflow covers the next page as well, Chrome
/// leaves that page to it and goes on at the top of the one after, with the overflowing box's
/// rest below the others' padding and border. The boxes whose content is done end on this page
/// (`Overflow::done`); the line's rest is left for the next one in `Overflow::rest`.
fn monolithic_overflow(
    pieces: &[Piece],
    start: Au,
    available: Au,
    placed: &[(usize, Fragment, Au, Option<BreakToken>)],
    boxes: &impl LineBoxes,
) -> Option<Overflow> {
    let page = boxes.overflow_page()?;
    // Whether a box's content is done (it goes on at most with its bottom padding and border,
    // or the empty space above them).
    let done = |index: usize, token: Option<&BreakToken>| {
        token.is_none_or(|token| matches!(token, BreakToken::Rest { .. }) || boxes.last_resort(index, token))
    };
    let overflowing: Vec<(usize, Au)> = placed
        .iter()
        .enumerate()
        .filter_map(|(position, (index, fragment, top, _))| {
            let piece = &pieces[*index];
            // A box whose content goes on below the monolithic content goes on below the
            // overflow too, as in Chrome (Blink's block flow resumes past it).
            if !piece.stretched {
                return None;
            }
            let reach = if boxes.laid_whole(*index) {
                fragment.rect.height - piece.outset.1
            } else {
                fragment
                    .children
                    .iter()
                    .filter_map(|child| crate::monolithic::content_bottom(child, Au::ZERO))
                    .max()?
            };
            let end = *top + reach - piece.outset.0;
            (end > available).then_some((position, end))
        })
        .collect();
    let end = overflowing.iter().map(|&(_, end)| end).max()?;
    if end <= available + boxes.overflow_room() {
        return None;
    }
    let consumed = start + end;
    let beyond = end - available >= page;
    // What is left of the line's height below the overflow, for the box at `index`.
    let left = |index: usize| {
        let piece = &pieces[index];
        (piece.offset + piece.height - consumed).clamp_non_negative()
    };
    let frame = |index: usize| boxes.bottom_frame(index);
    let (mut others, mut frames, mut own) = (Au::ZERO, Au::ZERO, Au::ZERO);
    let mut done_boxes = Vec::new();
    let mut below = Vec::new();
    let content_goes_on = placed
        .iter()
        .any(|(index, _, _, token)| pieces[*index].stretched && !done(*index, token.as_ref()));
    for (position, (index, _, _, token)) in placed.iter().enumerate() {
        if !pieces[*index].stretched || !done(*index, token.as_ref()) {
            continue;
        }
        if overflowing.iter().any(|&(other, _)| other == position) {
            let rest = frame(*index).min(left(*index));
            own = own.max(rest);
            if beyond && content_goes_on && rest > Au::ZERO {
                below.push((position, rest));
            }
        } else {
            frames = frames.max(frame(*index));
            others = others.max(frame(*index).max(left(*index)));
        }
        done_boxes.push(position);
    }
    let rest = if beyond { own + frames } else { own.max(others) };
    Some(Overflow {
        end,
        boxes: overflowing,
        rest,
        done: done_boxes,
        below,
    })
}

/// Lays out the part of a line whose boxes are `pieces` (in the order of their offsets) that
/// fits in `available` from the top of the page's part of the line. `line_height` is the
/// line's unbroken height; `continued` tells where the boxes continue if the line started on
/// an earlier page. `None` means that the line moves to the next page as a whole: its first
/// row does not fit and it does not start the page (`at_top`).
///
/// A flex or grid line does not grow for a box that ends on the page while the content that
/// overflows it goes on, nor do the rows of a grid for the boxes that keep the height of their
/// areas (see [`Piece::grows`]). Where the rest of the line fits on the page and only such
/// boxes go on, the line ends there as it does unbroken, and they go on past its end
/// ([`Portion::outlived`]) in parallel with what follows the line, as in Chrome.
pub(crate) fn split_line(
    pieces: &[Piece],
    line_height: Au,
    continued: Option<(&[Option<BreakToken>], Au)>,
    available: Au,
    at_top: bool,
    boxes: &mut impl LineBoxes,
) -> Option<Portion> {
    split_line_from(pieces, line_height, continued, available, at_top, boxes, None)
}

/// [`split_line`], with the rows from `push_from` on (a place in the line) moved to the next
/// page.
pub(crate) fn split_line_from(
    pieces: &[Piece],
    line_height: Au,
    continued: Option<(&[Option<BreakToken>], Au)>,
    available: Au,
    at_top: bool,
    boxes: &mut impl LineBoxes,
    push_from: Option<Au>,
) -> Option<Portion> {
    // A flex line that goes on at the top of a column too short to reach the next item (one of
    // no height, or a pixel tall), none of its items going on from the one before, goes on with
    // the first item that starts below there, at the top: the room above it (its top margin) is
    // truncated by the break, as in Chrome (it would otherwise make no progress).
    let continued = match continued {
        Some((tokens, offset))
            if at_top
                && push_from.is_none()
                && tokens.iter().all(Option::is_none)
                && pieces.first().is_some_and(|piece| piece.growth == Growth::Flex) =>
        {
            let first = pieces
                .iter()
                .map(|piece| piece.offset)
                .filter(|&top| top >= offset)
                .min()
                .unwrap_or(offset);
            if available <= Au::ZERO || first - offset >= available {
                Some((tokens, first.max(offset)))
            } else {
                Some((tokens, offset))
            }
        }
        other => other,
    };
    // The place in the line at the top of the page.
    let start = continued.map_or(Au::ZERO, |(_, offset)| offset);
    let available = if at_top {
        available.max(Au::EPSILON)
    } else {
        available
    };
    let incoming = |index: usize| continued.and_then(|(tokens, _)| tokens.get(index)?.as_ref());
    // What the fragment of a box that ends on the page reaches below its place in the line (the
    // lower half of a collapsed border, which Blink lays out with the row below it) needs no
    // room there; a box that breaks keeps its breaks where they are.
    let lower_half = |piece: &Piece, start: Au| {
        if piece.offset + piece.height - start <= available {
            piece.outset.1
        } else {
            Au::ZERO
        }
    };
    // The boxes laid out on the page: index, fragment, top in the part, token.
    let mut placed: Vec<(usize, Fragment, Au, Option<BreakToken>)> = Vec::new();
    // Boxes whose content is done but whose height goes on into this page.
    let mut fillers = Vec::new();
    // Boxes that ended on a page before beside a forced break, or end on this one.
    let mut ended: Vec<usize> = Vec::new();
    let mut bare: Vec<usize> = Vec::new();
    // Boxes that end on this page with the bottom padding and border left from the page before
    // after their content, and those that end below them with their own.
    let mut frame_rests = Vec::new();
    let mut below_rests = Vec::new();
    // The place in the line of the first row that moves to the next page.
    let mut pushed: Option<Au> = push_from;
    // The items of a flex line that do not fit below the top of the line and move to the next
    // page alone, starting at its top, while the items beside them stay (see `Before`).
    let mut moved: Vec<usize> = Vec::new();
    let flex = pieces.first().is_some_and(|piece| piece.growth == Growth::Flex);
    let mut waiting = false;
    // The heights of boxes laid out again taller (see `LineBoxes::expand`) before they were.
    let mut measured: Vec<Option<Au>> = vec![None; pieces.len()];
    // How far the rows of a grid line that start on the page move down below the items of the
    // rows above that go on from the page before and need more room than their areas leave
    // them: the rows those items end in grow by as much, as in Chrome (Blink's grid layout
    // shifts the rows below by the expansion of the rows its items grew).
    // (A box laid out again taller ends its row where it ended before, see `content_end`.)
    let shifts_rows = boxes.shifts_rows();
    let rows_shift =
        |offset: Au, placed: &[(usize, Fragment, Au, Option<BreakToken>)], measured: &[Option<Au>]| {
            if pieces.first().is_none_or(|piece| piece.growth != Growth::Grid) && !shifts_rows
                || continued.is_none()
            {
                return Au::ZERO;
            }
            placed
                .iter()
                .filter_map(|(index, fragment, top, token)| {
                    let above = &pieces[*index];
                    let area_end = above.offset + above.height + above.margin_bottom;
                    if !(above.grows && above.offset < start && area_end <= offset) {
                        return None;
                    }
                    let margin = if token.is_none() && measured[*index].is_none() {
                        above.margin_bottom
                    } else {
                        Au::ZERO
                    };
                    let height = measured[*index].unwrap_or(fragment.rect.height);
                    let end = *top + height - above.outset.0 - above.outset.1 + margin;
                    Some(end - (area_end - start))
                })
                .fold(Au::ZERO, Au::max)
        };
    for (index, piece) in pieces.iter().enumerate() {
        let started = continued.is_some() && piece.offset < start;
        if started {
            match incoming(index) {
                // A box that ended beside a forced break has no part here (see `forced_end`).
                Some(BreakToken::Ended) if boxes.ends_open(index) => bare.push(index),
                Some(BreakToken::Ended) => ended.push(index),
                // A flex item that moved here from the page before starts at the top.
                Some(BreakToken::Before) => {
                    let remaining = available + piece.outset.0;
                    match boxes.lay(index, None, remaining, at_top) {
                        Some((fragment, token)) => placed.push((index, fragment, Au::ZERO, token)),
                        None => moved.push(index),
                    }
                }
                Some(incoming) => {
                    let remaining = available + piece.outset.0 + lower_half(piece, start);
                    let (mut fragment, mut token) = boxes.lay(index, Some(incoming), remaining, at_top)?;
                    // A stretched box that continues past the end of its row (its height goes to
                    // the pages that its breaks took) makes the row grow by what it needs, as in
                    // Chrome; it is then laid out again at the grown height, its rows sharing the
                    // room, while the line ends where the box needed it to before.
                    let area_end = piece.offset + piece.height + piece.margin_bottom - start;
                    if piece.stretched && piece.grows && piece.growth != Growth::None && area_end <= available
                    {
                        let end = if token.is_some() {
                            available + Au::EPSILON
                        } else {
                            fragment.rect.height - piece.outset.0 - piece.outset.1
                        };
                        let by = match piece.growth {
                            Growth::Flex => end + piece.margin_bottom - area_end,
                            _ => end - area_end,
                        };
                        if by > Au::ZERO
                            && let Some(grown) = boxes.expand(index, incoming, remaining, at_top, by)
                        {
                            if piece.growth == Growth::Grid {
                                measured[index] = Some(fragment.rect.height);
                            }
                            (fragment, token) = grown;
                        }
                    }
                    if token.is_none()
                        && let Some((frame, stretched)) = boxes.frame_below_rest(index)
                        && boxes.frame_rest(index, incoming)
                    {
                        if stretched {
                            below_rests.push(frame);
                        } else {
                            frame_rests.push(index);
                        }
                    }
                    placed.push((index, fragment, Au::ZERO, token));
                }
                None if piece.stretched
                    && (piece.offset + piece.height > start
                        || piece.offset + piece.height >= line_height) =>
                {
                    fillers.push(index);
                }
                None => {}
            }
            continue;
        }
        if pushed.is_some_and(|row| piece.offset >= row) {
            continue;
        }
        let top = piece.offset - start + rows_shift(piece.offset, &placed, &measured);
        if top >= available {
            waiting = true;
            continue;
        }
        let first_row = top <= Au::ZERO;
        let remaining = available - top + piece.outset.0 + lower_half(piece, start);
        if let Some((fragment, token)) = boxes.lay(index, None, remaining, at_top && first_row) {
            placed.push((index, fragment, top, token));
            continue;
        }
        if first_row {
            return None;
        }
        // A flex item moves to the next page alone, the items beside it staying where they fit
        // and those below it keeping their places in the line, as in Chrome (Blink lays the
        // items of a row out in parallel flows and breaks before the one that does not fit).
        if flex && push_from.is_none() {
            moved.push(index);
            continue;
        }
        // The box's row moves to the next page, with the boxes beside it placed so far.
        pushed = Some(piece.offset);
        placed.retain(|(placed_index, ..)| {
            let other = &pieces[*placed_index];
            other.offset < piece.offset || (continued.is_some() && other.offset < start)
        });
    }
    // A line of which nothing fits moves to the next page, as does one in which no box breaks
    // inside its content (but at most in its bottom padding and border), unless it starts the
    // page: the break before it is the better one.
    let breaks_inside = placed.iter().any(|(index, _, _, token)| {
        token
            .as_ref()
            .is_some_and(|token| !boxes.last_resort(*index, token))
    });
    // A box below the first row that breaks in its bottom padding and border moves to the next
    // page with its row, even where a box of a row above (spanning into it) breaks inside its
    // content: the break between the rows above it is the better one, as in Chrome (where a
    // row's break is as bad as the worst of its cells').
    // Whether a box breaks inside its content (not in its bottom padding and border, nor in the
    // empty space below it).
    let content_breaks = placed.iter().any(|(index, _, _, token)| {
        token
            .as_ref()
            .is_some_and(|token| !boxes.last_resort(*index, token) && !token.splits_empty_space())
    });
    // A stretched box that avoids breaks inside it and reaches past the end of the page would
    // break in the room below its content, which Chrome avoids like any break inside it: its
    // row moves to the next page, the whole line where that is its first row (which does not
    // start the page).
    let avoided = |index: usize| {
        let piece = &pieces[index];
        piece.stretched && piece.offset + piece.height - start > available && boxes.avoids_breaking(index)
    };
    if continued.is_none()
        && !at_top
        && placed
            .iter()
            .any(|(index, _, top, _)| *top <= Au::ZERO && avoided(*index))
    {
        return None;
    }
    // The boxes that broke at a forced break inside them (see `LineBoxes::broke_forced`), and
    // the lowest of their places in the line.
    let forced: Vec<usize> = placed
        .iter()
        .filter(|(index, .., token)| token.is_some() && boxes.broke_forced(*index))
        .map(|(index, ..)| *index)
        .collect();
    let forced_row = forced.iter().map(|&index| pieces[index].offset).max();
    // The rows below a row that breaks at a forced break inside one of its boxes follow it on
    // the next page, below the rest of its content, as table rows do in Chrome: a box that spans
    // into them ends with its own height on this page.
    if push_from.is_none()
        && let Some(forced_row) = forced_row
        && let Some(row) = pieces
            .iter()
            .map(|piece| piece.offset)
            .filter(|&offset| offset > forced_row && offset >= start)
            .min()
    {
        return split_line_from(
            pieces,
            line_height,
            continued,
            available,
            at_top,
            boxes,
            Some(row),
        );
    }
    let forced_push = push_from.is_some_and(|row| forced_row.is_some_and(|forced_row| forced_row < row));
    // Where the line goes on on the next page: at the end of the rows above those moved there,
    // whose boxes that broke go on at its top; the rows moved there start below what those
    // need (see `rows_shift`), one after the other as Chrome lays table rows out.
    let forced_at = forced_push
        .then(|| {
            pieces
                .iter()
                .filter(|piece| piece.grows && forced_row.is_some_and(|row| piece.offset <= row))
                .map(|piece| piece.offset + piece.height)
                .max()
        })
        .flatten();
    if push_from.is_none() {
        let row = placed
            .iter()
            .filter(|(index, _, _, token)| {
                pieces[*index].offset > start
                    && (token
                        .as_ref()
                        .is_some_and(|token| boxes.last_resort(*index, token))
                        || continued.is_none() && avoided(*index))
            })
            .map(|(index, ..)| pieces[*index].offset)
            .min();
        if let Some(row) = row {
            return split_line_from(
                pieces,
                line_height,
                continued,
                available,
                at_top,
                boxes,
                Some(row),
            );
        }
    }
    // A box of the first row that breaks only in its bottom padding and border, where nothing
    // breaks inside its content and no later row starts on the page, does not fit: the row
    // below that waits does not make a break between rows better than the one before the line,
    // as in Chrome (a cell that spans both rows breaks below its content then). Nor does one
    // beside boxes that break inside their content in a table row, whose break is as bad as the
    // worst of its cells' in Chrome.
    let first_row_frame_break = (!content_breaks || boxes.worst_break_counts())
        && placed.iter().all(|(_, _, top, _)| *top <= Au::ZERO)
        && placed.iter().any(|(index, _, top, token)| {
            *top <= Au::ZERO
                && token
                    .as_ref()
                    .is_some_and(|token| boxes.last_resort(*index, token))
        });
    // A flex line breaks where its boxes leave room below them (a class C breakpoint), and
    // before a box that does not fit below the top of the line, which goes on at the top of the
    // next page, as in Chrome; a grid or table row does not.
    let room_below = flex
        && placed
            .iter()
            .map(|(index, fragment, top, _)| {
                let piece = &pieces[*index];
                *top + fragment.rect.height - piece.outset.0 - piece.outset.1 + piece.margin_bottom
            })
            .max()
            .is_none_or(|end| end < line_height - start);
    if continued.is_none()
        && (!at_top
            && (placed.is_empty() && !(flex && (pushed.is_some() || !moved.is_empty()))
                || !(breaks_inside || waiting || pushed.is_some() || !moved.is_empty())
                || first_row_frame_break)
            || at_top && !boxes.flush() && first_row_frame_break)
    {
        let fits = placed.iter().all(|(index, fragment, top, token)| {
            let piece = &pieces[*index];
            token.is_none() && *top + fragment.rect.height - piece.outset.0 - piece.outset.1 <= available
        }) && (line_height <= available || room_below && !placed.is_empty());
        if !fits {
            // The break between the rows above the first box below the first row that does not
            // fit (or breaks only in its bottom padding and border) is a better one, as in
            // Chrome.
            let row = placed
                .iter()
                .filter(|(index, fragment, top, token)| {
                    let piece = &pieces[*index];
                    piece.offset > start
                        && (token.is_some()
                            || *top + fragment.rect.height - piece.outset.0 - piece.outset.1 > available)
                })
                .map(|(index, ..)| pieces[*index].offset)
                .min();
            return match row {
                Some(row) if push_from.is_none() => split_line_from(
                    pieces,
                    line_height,
                    continued,
                    available,
                    at_top,
                    boxes,
                    Some(row),
                ),
                _ => None,
            };
        }
    }
    // Monolithic content that reaches past the end of the page (see `monolithic_overflow`), in a
    // line that no row waits in.
    let overflow = if waiting || pushed.is_some() || !moved.is_empty() {
        None
    } else {
        monolithic_overflow(pieces, start, available, &placed, boxes)
    }
    // The line goes on with its rest (see below) where none of its rows starts lower.
    .filter(|overflow| {
        pieces
            .iter()
            .all(|piece| piece.offset <= (line_height - overflow.rest).clamp_non_negative())
    });
    if let Some(overflow) = &overflow {
        for (position, (index, _, _, token)) in placed.iter_mut().enumerate() {
            if let Some(&(_, rest)) = overflow.below.iter().find(|&&(other, _)| other == position) {
                *token = Some(BreakToken::Rest { rest });
            } else if overflow.done.contains(&position) {
                *token = None;
            } else if let Some(token) = token
                && pieces[*index].stretched
            {
                // A box that goes on with its content beside the overflow has had the height
                // that the line took on the page, as in Chrome (Blink's cell gets the row's
                // height less what the row consumed): only the rest of its height is left.
                token.set_consumed(token.consumed() + overflow.end - available);
            }
        }
    }
    let natural_end = line_height - start;
    // In columns, a line that goes on with none of its boxes left (one of them reached past the
    // end of the column before with monolithic content, which Chrome lets overflow it) ends
    // with the boxes it places, rather than going on over empty columns.
    let all_done = continued.is_some()
        && boxes.in_columns()
        && pieces.iter().enumerate().all(|(index, piece)| {
            match placed.iter().find(|(placed_index, ..)| *placed_index == index) {
                Some((.., token)) => token.is_none(),
                None => piece.offset < start && incoming(index).is_none() && !piece.stretched,
            }
        });
    // The line ends on the page, outlived by boxes that it does not grow for.
    let outlived = pieces.first().is_some_and(|piece| piece.growth != Growth::None)
        && !waiting
        && pushed.is_none()
        && moved.is_empty()
        && overflow.is_none()
        && natural_end <= available
        && placed.iter().any(|(.., token)| token.is_some())
        && placed.iter().all(|(index, .., token)| {
            token.as_ref().is_none_or(|token| {
                matches!(token, BreakToken::Overflow { .. }) || !flex && !pieces[*index].grows
            })
        });
    let broken = !outlived
        && (waiting
            || pushed.is_some()
            || !moved.is_empty()
            || overflow
                .as_ref()
                .map_or(natural_end > available && !all_done, |overflow| {
                    overflow.rest > Au::ZERO
                })
            || placed.iter().any(|(.., token)| token.is_some()));
    let content_end = placed
        .iter()
        // (The boxes that outlive the line do not make it taller.)
        .filter(|(.., token)| !outlived || token.is_none())
        .map(|(index, fragment, top, token)| {
            let piece = &pieces[*index];
            // A box laid out again taller makes its row grow to the end of its border box as it
            // was before, without its bottom margin, as in Chrome.
            let margin = if token.is_none() && measured[*index].is_none() {
                piece.margin_bottom
            } else {
                Au::ZERO
            };
            let height = measured[*index].unwrap_or(fragment.rect.height);
            let end = *top + height - piece.outset.0 - piece.outset.1 + margin;
            if piece.grows {
                end
            } else {
                end.min(piece.offset + piece.height - start)
            }
        })
        .max()
        .unwrap_or(Au::ZERO);
    // Boxes whose height goes on end below the rest of the bottom padding and border of the
    // others, with their own.
    let rest = placed
        .iter()
        .filter(|(index, ..)| frame_rests.contains(index))
        .map(|(index, fragment, ..)| fragment.rect.height - pieces[*index].outset.0 - pieces[*index].outset.1)
        .max();
    let content_end = rest.map_or(content_end, |rest| {
        fillers
            .iter()
            .filter_map(|&index| boxes.frame_below_rest(index).map(|(frame, _)| frame))
            .chain(below_rests.iter().copied())
            .map(|frame| rest + frame)
            .fold(content_end, Au::max)
    });
    // The bottom padding and border left of a box after monolithic overflow that covered the
    // page before (see `Overflow::below`) go below the content that the other boxes go on with.
    let content_end = rest.map_or(content_end, |rest| {
        placed
            .iter()
            .filter(|(index, _, _, _)| {
                !frame_rests.contains(index)
                    && incoming(*index).is_some_and(|token| !matches!(token, BreakToken::Rest { .. }))
            })
            .map(|(index, fragment, top, _)| {
                *top + fragment.rect.height - pieces[*index].outset.0 - pieces[*index].outset.1 + rest
            })
            .fold(content_end, Au::max)
    });
    // Where the monolithic content of each flex item (or cell of a table row in columns) that
    // goes on to the next page reaches past the end of the page: the item grows to hold it, as
    // a block container does, and the line with it (see `monolithic::content_bottom`), as in
    // Chrome.
    let grows = flex || boxes.grows_to_content();
    let reaches: Vec<Option<Au>> = placed
        .iter()
        .map(|(_, fragment, top, token)| {
            let reach = *top + crate::monolithic::item_content_bottom(fragment, -fragment.rect.y)?;
            // In a table in columns, so does the content of a cell that ends there.
            (grows && (token.is_some() || !flex) && reach > available).then_some(reach)
        })
        .collect();
    let height = match &overflow {
        Some(overflow) => overflow.end,
        None if broken => reaches
            .iter()
            .flatten()
            .fold(available, |end, &reach| end.max(reach)),
        None if all_done => content_end,
        // The rows below the items that grew (empty rows and gaps that items span) end lower.
        None => natural_end
            .max(content_end)
            .max(natural_end + rows_shift(line_height, &placed, &measured)),
    };
    // How much further than the end of the page the line goes on it, where flex items grew:
    // the items beside them that go on as well extend that far, which their next parts do not
    // have (Blink's items take the height the line consumed).
    let grown = if overflow.is_none() && broken {
        height - available
    } else {
        Au::ZERO
    };
    if grown > Au::ZERO {
        for (position, (_, _, _, token)) in placed.iter_mut().enumerate() {
            match token {
                _ if flex && reaches[position].is_some() => {}
                // (The rest of the height of a cell that grew is shorter by as much too.)
                Some(BreakToken::Rest { rest }) if *rest > grown => *rest -= grown,
                Some(BreakToken::Rest { .. }) => *token = None,
                _ if reaches[position].is_some() => {}
                Some(token) => token.set_consumed(token.consumed() + grown),
                None => {}
            }
        }
    }
    // The monolithic content of a flex item that ends on the page and reaches past its end,
    // while other items go on: the line's part holds it with the item's bottom margin, which
    // moves the content of the next page below it (see `monolithic::content_bottom`), as in
    // Chrome, where items go on with a height of their own (which keep their place in the line);
    // stretched ones beside them have taken the line's height on the page (Blink's items take
    // the height the line consumed). Where only stretched items go on, they cover the overflow
    // in parallel to it on the next pages instead, which comes to the same.
    let ended_reach = (flex
        && broken
        && overflow.is_none()
        && placed
            .iter()
            .any(|(index, _, _, token)| token.is_some() && !pieces[*index].stretched))
    .then(|| {
        placed
            .iter()
            .filter(|(.., token)| token.is_none())
            .filter_map(|(index, fragment, top, _)| {
                let reach = *top + crate::monolithic::content_bottom(fragment, -fragment.rect.y)?;
                (reach > available).then(|| reach + pieces[*index].margin_bottom)
            })
            .max()
    })
    .flatten()
    .filter(|&reach| reach > height);
    // (Where the line goes on, it does so from the end of the page.)
    let line_end = height;
    let height = match ended_reach {
        Some(reach) => {
            let by = reach - height;
            for (index, _, _, token) in &mut placed {
                if !pieces[*index].stretched {
                    continue;
                }
                match token {
                    Some(BreakToken::Rest { rest }) if *rest > by => *rest -= by,
                    Some(BreakToken::Rest { .. }) => *token = None,
                    Some(token) => token.set_consumed(token.consumed() + by),
                    None => {}
                }
            }
            reach
        }
        None => height,
    };
    // A line in columns that grew to hold the content of its boxes, which all end with it,
    // ends there, its height used up.
    let broken = broken
        && !(grown > Au::ZERO
            && !flex
            && !waiting
            && pushed.is_none()
            && start + height >= line_height
            && placed.iter().all(|(.., token)| token.is_none()));
    let rows_end = (broken && !content_breaks).then(|| {
        placed
            .iter()
            .filter(|(index, ..)| pieces[*index].grows)
            .map(|(index, fragment, top, _)| {
                let piece = &pieces[*index];
                *top + fragment.rect.height - piece.outset.0 - piece.outset.1
            })
            .max()
            .unwrap_or(Au::ZERO)
    });
    // Where a box that follows the line ends in the part, and whether it goes on after it.
    let stretched_end = |piece: &Piece| {
        let end = if piece.offset + piece.height < line_height {
            piece.offset + piece.height - start
        } else if broken {
            // It goes on with the line.
            return (available, true);
        } else {
            height
        };
        if broken && end > available {
            (available, true)
        } else {
            (end, false)
        }
    };
    // Where the stretched items of a flex line that goes on from the page before end lowest:
    // the others end there too, as in Chrome (Blink lays its items out again at the height of
    // the row that the tallest grew).
    let items_end = (flex && continued.is_some())
        .then(|| {
            placed
                .iter()
                .filter(|(index, _, _, token)| {
                    pieces[*index].stretched && pieces[*index].grows && token.is_none()
                })
                .map(|(index, fragment, top, _)| {
                    *top + fragment.rect.height - pieces[*index].outset.0 - pieces[*index].outset.1
                })
                .max()
        })
        .flatten();
    // Where the line breaks only at forced breaks inside its boxes, the boxes beside them whose
    // content is done and that would go on with the line end with its part instead, their
    // bottom padding and border at the end of the page, and have no part on the pages after,
    // as the cells of a table row do in Chrome (Blink lays them out at the height of the row's
    // part, which their content fits in).
    let forced_end = broken
        && overflow.is_none()
        && !waiting
        && (pushed.is_none() || forced_push)
        && moved.is_empty()
        && grown == Au::ZERO
        && placed.iter().any(|(.., token)| token.is_some())
        && placed
            .iter()
            .all(|(index, .., token)| token.is_none() || boxes.broke_forced(*index));
    // (Where the rows below the forced break move to the next page, the boxes of the rows above
    // end with the part.)
    let ends_with_part = |piece: &Piece| {
        forced_end
            && (piece.offset + piece.height >= line_height
                || forced_push && forced_row.is_some_and(|row| piece.offset <= row))
    };
    let mut fragments = Vec::with_capacity(placed.len() + fillers.len());
    let mut tokens = vec![None; pieces.len()];
    for (position, (index, mut fragment, top, token)) in placed.into_iter().enumerate() {
        let piece = &pieces[index];
        let outsets = piece.outset.0 + piece.outset.1;
        // A box whose content reaches past the end of the page ends with that content, as in
        // Chrome; its bottom padding and border go on with the line, if it has room for them.
        if let Some(&(_, end)) = overflow
            .as_ref()
            .and_then(|overflow| overflow.boxes.iter().find(|&&(other, _)| other == position))
        {
            fragment.rect.height = end - top + outsets;
            if broken && !piece.clones {
                set_bottom(&mut fragment, false);
            }
        } else if piece.stretched
            && !frame_rests.contains(&index)
            // A box that ended, whose content overflows it and goes on, keeps its height.
            && !matches!(token, Some(BreakToken::Overflow { .. }))
            && !matches!(incoming(index), Some(BreakToken::Overflow { .. }))
        {
            let (mut end, mut goes_on) =
                if token.is_some() || forced_push && forced_row.is_some_and(|row| piece.offset <= row) {
                    (available + grown, true)
                } else {
                    stretched_end(piece)
                };
            if goes_on && token.is_none() && ends_with_part(piece) {
                goes_on = boxes.ends_open(index);
                ended.push(index);
                // A box that spans rows ends at its own height, not stretched to the end of the
                // part, as a table cell spanning rows does in Chrome.
                if !piece.grows {
                    end = end.min(piece.offset + piece.height - start);
                }
                if incoming(index).is_none() {
                    let by = end - top + outsets - fragment.rect.height;
                    boxes.realign(index, &mut fragment, by);
                }
            }
            let end = match items_end {
                Some(items_end) if !goes_on && piece.grows => end.max(items_end),
                _ => end,
            };
            let cut = token
                .as_ref()
                .map_or(Au::ZERO, |token| boxes.frame_cut(index, token));
            let end = if cut > Au::ZERO {
                end.min(piece.offset + piece.height - start - cut)
            } else {
                end
            };
            let wanted = (end - top + outsets).clamp_non_negative();
            if goes_on || !piece.grows || wanted > fragment.rect.height {
                fragment.rect.height = if goes_on || !piece.grows {
                    wanted
                } else {
                    wanted.max(fragment.rect.height)
                };
            }
            if goes_on && token.is_none() && !piece.clones {
                set_bottom(&mut fragment, false);
            }
        }
        fragment.rect.y = top - piece.outset.0;
        tokens[index] = token;
        fragments.push((index, fragment));
    }
    for index in fillers {
        let piece = &pieces[index];
        let (end, mut goes_on) = stretched_end(piece);
        if goes_on && ends_with_part(piece) {
            goes_on = boxes.ends_open(index);
            ended.push(index);
        }
        let mut fragment = boxes.filler(index);
        fragment.rect.y = -piece.outset.0;
        fragment.rect.height = end.max(Au::ZERO) + piece.outset.0 + piece.outset.1;
        if let FragmentKind::Box { sides, .. } = &mut fragment.kind {
            sides.top = piece.clones;
            sides.bottom = !goes_on || piece.clones;
        }
        fragments.push((index, fragment));
    }
    for &index in &bare {
        let piece = &pieces[index];
        let (end, goes_on) = stretched_end(piece);
        let mut fragment = boxes.bare_filler(index);
        fragment.rect.y = -piece.outset.0;
        fragment.rect.height = end.max(Au::ZERO) + piece.outset.0 + piece.outset.1;
        if let FragmentKind::Box { sides, .. } = &mut fragment.kind {
            sides.top = false;
            sides.bottom = !goes_on;
        }
        if goes_on {
            ended.push(index);
        }
        fragments.push((index, fragment));
    }
    fragments.sort_by_key(|(index, _)| *index);
    for &index in &moved {
        tokens[index] = Some(BreakToken::Before);
    }
    for &index in &ended {
        tokens[index] = Some(BreakToken::Ended);
    }
    let (next, outlived) = if broken {
        let offset = match &overflow {
            // The line goes on with its rest after the overflow: as the end of its height, which
            // the boxes that are done fill.
            Some(overflow) => (line_height - overflow.rest).clamp_non_negative(),
            None => {
                forced_at.unwrap_or_else(|| pushed.map_or(start + line_end, |row| row.min(start + line_end)))
            }
        };
        (Some((tokens, offset)), None)
    } else {
        (None, outlived.then_some(tokens))
    };
    Some(Portion {
        fragments,
        height,
        next,
        rows_end,
        bare,
        outlived,
    })
}

/// How far the block flow of `fragment` reaches down from its top: its border box, or the
/// lines and block-level boxes of its content that overflow it, which break across pages with
/// it as in Chrome (the content of a flex or grid item taller than the item goes on on the next
/// page). Not past the end of a box that clips its overflow, nor of a multi-column container,
/// whose columns hold their content; the content of lines counts with the line.
pub(crate) fn flow_extent(fragment: &Fragment) -> Au {
    if let FragmentKind::Box { style, .. } = &fragment.kind
        && (style.overflow_x != fagus_style::values::Overflow::Visible
            || style.overflow_y != fagus_style::values::Overflow::Visible
            || style.is_multicol())
    {
        return fragment.rect.height;
    }
    fragment
        .children
        .iter()
        .filter_map(|child| match child.kind {
            FragmentKind::Box { .. } | FragmentKind::Anonymous => Some(child.rect.y + flow_extent(child)),
            FragmentKind::Line { .. } | FragmentKind::Image(_) => Some(child.rect.y + child.rect.height),
            _ => None,
        })
        .fold(fragment.rect.height, Au::max)
}

/// Shows or hides the bottom border of a box fragment.
fn set_bottom(fragment: &mut Fragment, visible: bool) {
    if let FragmentKind::Box { sides, .. } = &mut fragment.kind {
        sides.bottom = visible;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::VisibleSides;
    use crate::units::Rect;

    fn px(value: f32) -> Au {
        Au::from_px(value)
    }

    /// Boxes of given content heights that break every `step` pixels.
    struct Columns {
        heights: Vec<Au>,
        step: Au,
        monolithic: Vec<bool>,
    }

    fn boxed(height: Au) -> Fragment {
        Fragment {
            rect: Rect::new(Au::ZERO, Au::ZERO, px(10.0), height),
            kind: FragmentKind::Box {
                style: std::sync::Arc::default(),
                node: None,
                sides: VisibleSides::ALL,
            },
            children: Vec::new().into(),
        }
    }

    impl LineBoxes for Columns {
        fn lay(
            &mut self,
            index: usize,
            token: Option<&BreakToken>,
            remaining: Au,
            at_top: bool,
        ) -> Option<(Fragment, Option<BreakToken>)> {
            let done = match token {
                Some(BreakToken::Rest { rest }) => self.heights[index] - *rest,
                _ => Au::ZERO,
            };
            let left = self.heights[index] - done;
            if left <= remaining {
                return Some((boxed(left), None));
            }
            if self.monolithic[index] {
                return at_top.then(|| (boxed(left), None));
            }
            let fits = (remaining / self.step.0) * self.step.0;
            if fits == Au::ZERO {
                return at_top.then(|| {
                    (
                        boxed(self.step),
                        Some(BreakToken::Rest {
                            rest: left - self.step,
                        }),
                    )
                });
            }
            Some((boxed(fits), Some(BreakToken::Rest { rest: left - fits })))
        }

        fn last_resort(&self, _: usize, _: &BreakToken) -> bool {
            false
        }

        fn filler(&mut self, _: usize) -> Fragment {
            boxed(Au::ZERO)
        }
    }

    fn piece(offset: f32, height: f32, stretched: bool) -> Piece {
        Piece {
            offset: px(offset),
            height: px(height),
            margin_bottom: Au::ZERO,
            stretched,
            clones: false,
            grows: true,
            outset: (Au::ZERO, Au::ZERO),
            growth: Growth::None,
        }
    }

    #[test]
    fn breaks_boxes_and_stretches_the_others() {
        let mut boxes = Columns {
            heights: vec![px(100.0), px(30.0)],
            step: px(20.0),
            monolithic: vec![false, false],
        };
        let pieces = [piece(0.0, 100.0, true), piece(0.0, 100.0, true)];
        let first = split_line(&pieces, px(100.0), None, px(50.0), false, &mut boxes).unwrap();
        assert_eq!(first.height, px(50.0));
        // The short box extends to the end of the page and goes on as an empty one.
        assert_eq!(first.fragments[1].1.rect.height, px(50.0));
        let (tokens, offset) = first.next.unwrap();
        assert_eq!(offset, px(50.0));
        let second = split_line(
            &pieces,
            px(100.0),
            Some((&tokens, offset)),
            px(200.0),
            true,
            &mut boxes,
        )
        .unwrap();
        // 60px of the first box are left, more than the 50px left of the unbroken line.
        assert_eq!(second.height, px(60.0));
        assert_eq!(second.fragments.len(), 2);
        assert_eq!(second.fragments[1].1.rect.height, px(60.0));
        assert!(second.next.is_none());
    }

    #[test]
    fn moves_rows_that_do_not_fit() {
        let mut boxes = Columns {
            heights: vec![px(30.0), px(40.0)],
            step: px(20.0),
            monolithic: vec![false, true],
        };
        let pieces = [piece(0.0, 30.0, false), piece(35.0, 40.0, false)];
        let first = split_line(&pieces, px(75.0), None, px(60.0), false, &mut boxes).unwrap();
        assert_eq!(first.fragments.len(), 1);
        let (tokens, offset) = first.next.unwrap();
        assert_eq!(offset, px(35.0));
        let second = split_line(
            &pieces,
            px(75.0),
            Some((&tokens, offset)),
            px(200.0),
            true,
            &mut boxes,
        )
        .unwrap();
        assert_eq!(second.fragments.len(), 1);
        assert_eq!(second.fragments[0].1.rect.y, Au::ZERO);
        assert_eq!(second.height, px(40.0));
        // The first row does not fit: the line moves on.
        assert!(split_line(&pieces, px(75.0), None, px(30.0), false, &mut boxes).is_some());
        let mut monolithic = Columns {
            heights: vec![px(30.0)],
            step: px(20.0),
            monolithic: vec![true],
        };
        assert!(split_line(&pieces[..1], px(30.0), None, px(20.0), false, &mut monolithic).is_none());
    }

    #[test]
    fn measures_the_block_flow_overflowing_a_box() {
        // A box 10px tall holding a box 50px tall 20px down, which holds a line reaching 30px
        // below it; content that a box clipping its overflow holds does not count.
        let mut inner = boxed(px(50.0));
        inner.rect.y = px(20.0);
        let mut line = boxed(px(80.0));
        line.kind = FragmentKind::Line { baseline: Au::ZERO };
        line.rect.y = px(0.0);
        inner.children = vec![line].into();
        let mut outer = boxed(px(10.0));
        outer.children = vec![inner].into();
        assert_eq!(flow_extent(&outer), px(100.0));
        let clipping = fagus_style::ComputedStyle::from(fagus_style::ComputedValues {
            overflow_y: fagus_style::values::Overflow::Hidden,
            ..fagus_style::ComputedValues::default()
        });
        outer.kind = FragmentKind::Box {
            style: std::sync::Arc::new(clipping),
            node: None,
            sides: VisibleSides::ALL,
        };
        assert_eq!(flow_extent(&outer), px(10.0));
    }
}
