//! Spaces at the ends of lines and between words as Chrome sets them: a zero width space that
//! ends the spaces that hang, spaces between inline boxes at the end of a right-to-left line,
//! ideographic spaces before preserved ones, word spacing after the marks on a space; and the
//! first line of a block whose content starts with a float, content that `align-content:
//! unsafe` moves past the start of its box; inter-character justification around atomic
//! inlines and ligatures; the space between boxes that do not wrap where `overflow-wrap` breaks.

use super::{find, lines, render, render_document};
use crate::fragment::{Fragment, FragmentKind};

/// The text runs under `fragment`: their left and right edges, font size and text.
fn runs(fragment: &Fragment) -> Vec<(f32, f32, f32, String)> {
    fn walk(fragment: &Fragment, x: f32, out: &mut Vec<(f32, f32, f32, String)>) {
        let x = x + fragment.rect.x.to_px();
        if let FragmentKind::Text(run) = &fragment.kind {
            let width: f32 = run.glyphs.iter().map(|glyph| glyph.advance).sum();
            out.push((x, x + width, run.size, run.text.clone()));
        }
        for child in &fragment.children {
            walk(child, x, out);
        }
    }
    let mut out = Vec::new();
    walk(fragment, 0.0, &mut out);
    out
}

#[test]
fn breaks_before_a_zero_width_space_after_spaces() {
    // The space before the zero width space does not hang: it is not at the end of the line,
    // which breaks before the zero width space instead, as in Chrome.
    let pages = render(
        "<style>body { font: 16px Inter; margin: 0 } div { width: 1px; white-space: pre-wrap }</style>
         <div>a \u{200B} b</div>",
    );
    assert_eq!(lines(&pages[0].fragments[0]), 3);
}

#[test]
fn puts_the_space_between_boxes_at_the_end_of_a_right_to_left_line() {
    // The space between two boxes of left-to-right text ends the line: it takes the paragraph's
    // direction (UAX #9, rule L1) and hangs on the left, the text stands at the right edge.
    let pages = render(
        "<style>body { font: 20px Inter; margin: 0 } div { direction: rtl; width: 100px }</style>
         <div><span>abc</span> <span>def</span></div>",
    );
    let runs = runs(&pages[0].fragments[0]);
    let abc = runs.iter().find(|run| run.3.contains("abc")).expect("abc");
    assert!((abc.1 - 100.0).abs() < 0.5, "{runs:?}");
}

#[test]
fn hangs_ideographic_spaces_before_preserved_spaces_that_overflow() {
    // The preserved spaces at the end of the paragraph hang only as far as they overflow; the
    // ideographic spaces before them hang too where all of them overflow, as in Chrome: the
    // text stands where it would without them.
    let abc = |content: &str| {
        let pages = render(&format!(
            "<style>body {{ font: 20px Inter; margin: 0 }} div {{ width: 60px; text-align: right }}
               span {{ white-space: pre-wrap }}</style><div>{content}</div>"
        ));
        let runs = runs(&pages[0].fragments[0]);
        runs.iter()
            .find(|run| run.3.contains("abc"))
            .map_or(f32::NAN, |run| run.0)
    };
    let alone = abc("abc");
    assert!((abc("abc\u{3000}\u{3000}<span>  </span>") - alone).abs() < 0.01);
}

#[test]
fn widens_a_space_after_the_mark_on_it() {
    // The combining acute on the space stays above it: the word spacing goes after the mark.
    let pages = render(
        "<style>body { font: 20px Inter; margin: 0 } div { word-spacing: 100px; white-space: nowrap }</style>
         <div>A \u{301}</div>",
    );
    let mut glyphs = Vec::new();
    collect_glyphs(&pages[0].fragments[0], &mut glyphs);
    assert_eq!(glyphs.len(), 3, "{glyphs:?}");
    assert!(glyphs[1] < 50.0 && glyphs[2] >= 100.0, "{glyphs:?}");
}

fn collect_glyphs(fragment: &Fragment, out: &mut Vec<f32>) {
    if let FragmentKind::Text(run) = &fragment.kind {
        out.extend(run.glyphs.iter().map(|glyph| glyph.advance));
    }
    for child in &fragment.children {
        collect_glyphs(child, out);
    }
}

#[test]
fn styles_the_first_line_after_a_float_that_starts_the_block() {
    // The float does not take the block's first line: the text after it is in that style.
    let pages = render(
        "<style>body { font: 16px Inter; margin: 0 } .p { width: 200px } .p::first-line { font-size: 40px }
           .f { float: left; width: 20px; height: 20px }</style>
         <div class=p><div class=f></div>x</div>",
    );
    let runs = runs(&pages[0].fragments[0]);
    let x = runs.iter().find(|run| run.3.contains('x')).expect("x");
    assert!((x.2 - 40.0).abs() < 0.01, "{runs:?}");
}

#[test]
fn moves_content_past_the_start_of_its_box_where_align_content_is_unsafe() {
    for (alignment, top) in [("unsafe end", -30.0), ("end", 0.0)] {
        let (document, pages) = render_document(&format!(
            "<style>body {{ font: 16px/40px Inter; margin: 0 }}
               .b {{ height: 10px; align-content: {alignment}; margin-top: 50px }}</style>
             <div class=b>x</div>"
        ));
        let (_, y, fragment) = find(&pages[0], &document, "b").expect("box");
        let line = fragment.children.first().expect("line");
        assert!(
            (line.rect.y.to_px() - top).abs() < 0.01,
            "{alignment}: {y} {:?}",
            line.rect
        );
    }
}

#[test]
fn justifies_characters_around_atomic_inlines_and_not_inside_ligatures() {
    // Inter-character justification: an inline block is a character with a gap on each side;
    // the gap inside the arrow that Inter makes of `->` (one glyph) takes its share of the room
    // but adds nothing, so the line ends short of the end of its box, as in Chrome.
    let justified = |content: &str| {
        let (document, pages) = render_document(&format!(
            "<style>body {{ font: 20px Inter; margin: 0 }}
               p {{ width: 200px; margin: 0; text-align-last: justify; text-justify: inter-character }}
               .ib {{ display: inline-block; width: 20px; height: 10px }}</style><p>{content}</p>"
        ));
        let runs = runs(&pages[0].fragments[0]);
        let block = find(&pages[0], &document, "ib").map(|(x, _, _)| x);
        (runs, block)
    };
    let (runs, block) = justified("o<span class=ib></span>c");
    // The gap before the block widens the `o`, the one after it the block.
    let (o, c) = (&runs[0], &runs[runs.len() - 1]);
    let after = c.0 - (block.unwrap_or(f32::NAN) + 20.0);
    let o_and_gap = o.1 - o.0;
    assert!(
        after > 50.0 && o_and_gap > after && o_and_gap < after + 15.0,
        "{runs:?} {block:?}"
    );
    let (runs, _) = justified("a->a");
    let end = runs.iter().map(|run| run.1).fold(0.0, f32::max);
    assert!(end < 190.0, "{runs:?}");
}

#[test]
fn rounds_each_part_of_generated_content_as_an_item() {
    // Blink makes a text object of each string and counter of `content` and rounds the width
    // of each up to layout units: `"(" counter(c) ") "` takes 1/64 pixel more than `"(1) "`
    // before the text after it (Inter at 16px: 5.836 + 6.508 + 10.336 pixels against 22.680).
    let start = |content: &str| {
        let (_, pages) = render_document(&format!(
            "<style>body {{ font: 16px Inter; margin: 0 }} p {{ margin: 0; counter-reset: c 1 }}
               p::before {{ content: {content} }}</style><p>Zweck</p>"
        ));
        runs(&pages[0].fragments[0])
            .iter()
            .find(|run| run.3.starts_with('Z'))
            .map_or(f32::NAN, |run| run.0)
    };
    let whole = start("'(1) '");
    let parts = start("'(' counter(c) ') '");
    assert!((parts - whole - 1.0 / 64.0).abs() < 1e-3, "{whole} {parts}");
}

#[test]
fn ends_a_line_at_a_hyphen_as_wide_as_the_hyphen_alone() {
    // Inter kerns `-V`: where the line breaks after the hyphen, Chrome shapes the text before
    // the break again, so the line ends with the hyphen's own advance and justification spreads
    // the same room as before `-a`, which Inter does not kern.
    let words = |next: &str| {
        let (_, pages) = render_document(&format!(
            "<style>body {{ font: 10pt/1.55 Inter; margin: 0 }} p {{ width: 357px; margin: 0; text-align: justify }}</style>
             <p>die elektronische Rechnung im inländischen B2B-{next}erkehr zur Regel gemacht.</p>"
        ));
        runs(&pages[0].fragments[0])
    };
    let (kerned, plain) = (words("V"), words("a"));
    let end = |runs: &[(f32, f32, f32, String)]| {
        runs.iter()
            .find(|run| run.3.contains("B2B-"))
            .map_or(f32::NAN, |run| run.1)
    };
    assert!((end(&kerned) - end(&plain)).abs() < 0.01, "{kerned:?} {plain:?}");
}

/// The lines of `content` in a box 60px wide with `style`, in standards mode (where a line of
/// nothing but a space is as tall as any other): the top of each in the box and its text.
fn nowrap_lines(style: &str, content: &str) -> Vec<(f32, String)> {
    let (document, pages) = render_document(&format!(
        "<!DOCTYPE html><style>@page {{ margin: 0 }} body {{ font: 12px/16px Inter; margin: 0 }}
           .n {{ white-space: nowrap }}</style><div class=box style='width: 60px; {style}'>{content}</div>"
    ));
    let (_, _, block) = find(&pages[0], &document, "box").expect("box");
    block
        .children
        .iter()
        .map(|line| {
            let text: String = runs(line).into_iter().map(|run| run.3).collect();
            (line.rect.y.to_px(), text.trim().to_owned())
        })
        .collect()
}

#[test]
fn hangs_the_space_between_boxes_that_do_not_wrap_where_overflow_wrap_breaks() {
    // Issue #11: the first box is wider than the line, the space after it hangs at its end and
    // the second box starts the next line, as in Chrome, also where `overflow-wrap` lets lines
    // break anywhere: not before the space, which made a line of its own.
    let two = "<span class=n>Oct 21, 2026,</span> <span class=n>02:05:00 PM</span>";
    let expected = [
        (0.0, "Oct 21, 2026,".to_owned()),
        (16.0, "02:05:00 PM".to_owned()),
    ];
    for style in [
        "",
        "overflow-wrap: break-word",
        "overflow-wrap: anywhere",
        "word-break: break-word",
        "word-break: break-all",
        "overflow-wrap: break-word; white-space: pre-wrap",
    ] {
        assert_eq!(nowrap_lines(style, two), expected, "{style}");
    }
    // Two spaces, a space in a box of its own and a preserved one.
    for content in [
        "<span class=n>Oct 21, 2026,</span>  <span class=n>02:05:00 PM</span>",
        "<span class=n>Oct 21, 2026,</span><span> </span><span class=n>02:05:00 PM</span>",
        "<span class=n>Oct 21, 2026,</span><span style='white-space: pre-wrap'> </span><span class=n>02:05:00 PM</span>",
    ] {
        assert_eq!(
            nowrap_lines("overflow-wrap: anywhere", content),
            expected,
            "{content}"
        );
    }
    // Three boxes, one line each.
    assert_eq!(
        nowrap_lines(
            "overflow-wrap: break-word",
            &format!("{two} <span class=n>Nov 3</span>")
        ),
        [
            (0.0, "Oct 21, 2026,".to_owned()),
            (16.0, "02:05:00 PM".to_owned()),
            (32.0, "Nov 3".to_owned())
        ]
    );
    // A space of `break-spaces` takes room: the line breaks before it, and after it, as in Chrome.
    assert_eq!(
        nowrap_lines("overflow-wrap: break-word; white-space: break-spaces", two),
        [
            (0.0, "Oct 21, 2026,".to_owned()),
            (16.0, String::new()),
            (32.0, "02:05:00 PM".to_owned())
        ]
    );
}

#[test]
fn keeps_rows_of_fixed_tables_as_tall_as_their_lines_where_overflow_wrap_breaks() {
    // Issue #11: each row of two lines is 32px tall, as in Chrome, not 48px with a line for the
    // space between the boxes that do not wrap.
    let (document, pages) = render_document(
        "<!DOCTYPE html><style>@page { margin: 0 } body { font: 12px/16px Inter; margin: 0 }
           table { table-layout: fixed; width: 120px; border-collapse: collapse }
           td { width: 60px; padding: 0; border: 1px solid; overflow-wrap: break-word }
           .n { white-space: nowrap }</style>
         <table class=t><tr><td><span class=n>Oct 21, 2026,</span> <span class=n>02:05:00 PM</span><td>x
           <tr><td><span class=n>Oct 21, 2026,</span> <span class=n>02:05:00 PM</span><td>y</table>",
    );
    let (_, _, table) = find(&pages[0], &document, "t").expect("table");
    assert_eq!(table.rect.height.to_px(), 67.0);
}
