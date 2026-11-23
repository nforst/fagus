//! Layout found printing documents with Chrome: the initial containing block, the size text is
//! set at, kerning across words, form controls and line breaks in flex rows, vertical table
//! cells, floats near the end of a page and balanced columns. The numbers are Chrome's,
//! measured with the same font. Without margins of their own, pages start their content at
//! 37px, Chrome's default margin rounded.

use super::{find, lines, render_document};
use crate::{Fragment, FragmentKind, TextRun};

/// The text runs below `fragment`, with their x positions on the page.
fn runs<'f>(fragment: &'f Fragment, x: f32, out: &mut Vec<(f32, &'f TextRun)>) {
    let x = x + fragment.rect.x.to_px();
    if let FragmentKind::Text(run) = &fragment.kind {
        out.push((x, run));
    }
    for child in &fragment.children {
        runs(child, x, out);
    }
}

#[test]
fn hides_generated_content_with_display_none() {
    let (_, pages) = render_document(
        "<style>body { font: 12px Inter } .job::after { content: 'AFTER'; position: absolute }
           .job:last-child::after { display: none }
           .b::after { content: 'B' } .b:last-child::after { display: none }</style>
         <main><div class=job>one</div><div class='job last'>two</div></main>
         <div><p class=b>b1</p><p class='b c'>b2</p></div>",
    );
    let mut found = Vec::new();
    runs(&pages[0].fragments[0], 0.0, &mut found);
    let text: String = found.iter().map(|(_, run)| run.text.as_str()).collect();
    assert_eq!(text.matches("AFTER").count(), 1, "{text}");
    assert_eq!(text.matches('B').count(), 1, "{text}");
}

#[test]
fn places_positioned_boxes_in_a_viewport_of_whole_pixels() {
    // A4 with margins of 12mm leaves 703.02 pixels: the page area is 704 wide, the initial
    // containing block 703, so a box at `right: 0` ends at 703 and one at `bottom: 0` at 1031.
    let (document, pages) = render_document(
        "<style>@page { size: A4; margin: 12mm } body { margin: 0 }
           .abs { position: absolute; right: 0; bottom: 0; width: 100px; height: 20px }
           .flow { height: 10px }</style>
         <div class=flow></div><div class=abs></div>",
    );
    let (x, y, _) = find(&pages[0], &document, "abs").expect("box");
    assert_eq!((x, y), (45.0 + 603.0, 45.0 + 1031.0 - 20.0));
    let (_, _, flow) = find(&pages[0], &document, "flow").expect("flow");
    assert_eq!(flow.rect.width.to_px(), 704.0);
}

#[test]
fn sets_text_at_hundredths_of_a_pixel() {
    let (_, pages) = render_document("<p style='font: 10pt Inter'>Text</p>");
    let mut found = Vec::new();
    runs(&pages[0].fragments[0], 0.0, &mut found);
    let (_, run) = found.first().expect("run");
    assert!((run.size - 13.33).abs() < 1e-4, "{}", run.size);
}

#[test]
fn kerns_across_words_as_one_shaped_line() {
    // Words are shaped one at a time, but their places are those of the whole line shaped at
    // once, kerning between a space and the next word included.
    let text = "AVAVAV To Ty Wa Yo LT AWAY";
    let (_, pages) = render_document(&format!(
        "<style>body {{ margin: 0 }} p {{ margin: 0; font: 20px Inter; white-space: nowrap }}</style><p>{text}</p>"
    ));
    let fonts = super::test_fonts();
    let face = fonts
        .query(&["Inter"], 400.0, fagus_text::FontStyle::Normal)
        .expect("Inter");
    let shaped = fonts.shape(face, text);
    let scale = fonts.face(face).scale(20.0);
    let mut found = Vec::new();
    runs(&pages[0].fragments[0], 0.0, &mut found);
    let end: f32 = found
        .iter()
        .map(|(x, run)| x + run.glyphs.iter().map(|glyph| glyph.advance).sum::<f32>())
        .fold(0.0, f32::max);
    assert!(
        (end - 37.0 - shaped.advance * scale).abs() < 0.01,
        "{end} {}",
        shaped.advance * scale
    );
}

#[test]
fn kerns_across_the_edges_of_inline_boxes() {
    // Chrome shapes the text of adjacent boxes in the same font at once: the quotes of `<q>`
    // kern with what follows them, as "AV" does across a span, unless padding (or a margin, a
    // border, a vertical alignment) separates them.
    let end = |html: &str| {
        let (_, pages) = render_document(&format!(
            "<style>body {{ margin: 0 }} p {{ margin: 0; font: 30px Inter; white-space: nowrap }}</style><p>{html}</p>"
        ));
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], 0.0, &mut found);
        found
            .iter()
            .map(|(x, run)| x + run.glyphs.iter().map(|glyph| glyph.advance).sum::<f32>())
            .fold(0.0, f32::max)
    };
    let plain = end("AV");
    assert!((end("A<span>V</span>") - plain).abs() < 0.01);
    assert!(end("A<span style='padding-left: 0.01px'>V</span>") > plain + 1.0);
    // Each quote is an item of its own, its width rounded up to layout units: Chrome ends
    // `<q>x</q>.` 2/64 pixel after `“x”.`.
    assert!((end("<q>x</q>.") - end("\u{201c}x\u{201d}.")).abs() < 0.05);
}

#[test]
fn kerns_across_inline_boxes_with_the_same_font_description() {
    // Blink shapes adjacent items at once where their whole font description is equal: the
    // same letter spacing (not only none) kerns "AV" across a span, while another weight,
    // stretch or spacing does not, even where it gets the same face.
    let end = |p: &str, html: &str| {
        let (_, pages) = render_document(&format!(
            "<style>body {{ margin: 0 }} p {{ margin: 0; font: 30px Inter; white-space: nowrap; {p} }}</style><p>{html}</p>"
        ));
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], 0.0, &mut found);
        found
            .iter()
            .map(|(x, run)| x + run.glyphs.iter().map(|glyph| glyph.advance).sum::<f32>())
            .fold(0.0, f32::max)
    };
    let spaced = "letter-spacing: 2px";
    assert!((end(spaced, "A<span>V</span>") - end(spaced, "AV")).abs() < 0.01);
    let apart = end("", "AV") + 1.0;
    assert!(end(spaced, "A<span style='letter-spacing: 3px'>V</span>") > end(spaced, "AV") + 2.0);
    assert!(end("", "A<span style='font-weight: 500'>V</span>") > apart);
    assert!(end("", "A<span style='font-stretch: 110%'>V</span>") > apart);
}

#[test]
fn shrinks_checkboxes_and_text_fields_in_flex_rows_to_their_own_size() {
    let long = "wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww";
    let (document, pages) = render_document(&format!(
        "<style>body {{ margin: 0; font: 14px Inter; width: 400px }} .f {{ display: flex }}
           input {{ margin: 0 }}</style>
         <div class=f><input class=check type=checkbox style='width: 30px; height: 30px'><span>{long}</span></div>
         <div class=f><input class=field type=text style='width: 300px'><span>{long}</span></div>"
    ));
    // A checkbox goes down to 13px, as wide as without a width; a text field to its width
    // for 20 characters.
    let (_, _, check) = find(&pages[0], &document, "check").expect("checkbox");
    assert_eq!(check.rect.width.to_px(), 13.0);
    let (_, _, field) = find(&pages[0], &document, "field").expect("field");
    assert!(field.rect.width.to_px() > 100.0, "{}", field.rect.width.to_px());
}

#[test]
fn keeps_line_breaks_with_the_text_of_flex_containers() {
    let (document, pages) = render_document(
        "<style>body { font: 14px Inter } .f { display: flex; flex-direction: column }</style>
         <div class=f>PAID<br>05.11.2026</div>",
    );
    let (_, _, flex) = find(&pages[0], &document, "f").expect("container");
    assert_eq!(flex.children.len(), 1, "one item");
    assert_eq!(lines(flex), 2);
}

#[test]
fn widens_columns_by_the_padding_along_vertical_cells() {
    // A vertical cell 30px wide with padding of 10px above and below and 2px at the sides takes
    // a column 30 + 20 + 1 wide in Chrome, as if its padding along its lines lay across.
    let (document, pages) = render_document(
        "<style>body { margin: 0; font: 10px Inter } table { width: 600px; border-collapse: collapse; table-layout: fixed }
           th, td { border: 1px solid; padding: 10px 2px }
           .v { writing-mode: vertical-rl; width: 30px }</style>
         <table><tr><th style='width: 100px'>A</th><th class=v>S</th><th class=b>B</th><th>C</th></tr></table>",
    );
    let (x, _, _) = find(&pages[0], &document, "b").expect("cell");
    assert_eq!(x, 37.0 + 156.0);
}

#[test]
fn moves_a_float_whose_bottom_margin_does_not_fit_to_the_next_page() {
    let (document, pages) = render_document(
        "<style>@page { size: 400px 400px; margin: 0 } body { margin: 0; font: 16px/20px Inter }
           .f { float: right; width: 100px; margin: 0 0 50px 10px }</style>
         <div style='height: 300px'></div>
         <div class=f>Float one two three four</div><p style='margin: 0'>Text beside it</p>",
    );
    assert_eq!(pages.len(), 2);
    assert!(find(&pages[0], &document, "f").is_none());
    let (_, y, _) = find(&pages[1], &document, "f").expect("float");
    assert_eq!(y, 0.0);
}

#[test]
fn balances_columns_without_the_bottom_margin_they_end_with() {
    // 400px of content with margins in three columns: 133.33 tall, the margin after the last
    // paragraph cut off at the end of the column, as in Chrome.
    let (document, pages) = render_document(
        "<style>body { margin: 0; font: 16px/20px Inter } .cols { column-count: 3; column-gap: 20px }
           article { margin-bottom: 50px } p { margin: 0 } .span { column-span: all }</style>
         <div class=cols>
           <article><p>One</p><p>Two</p><p>Three</p></article><article><p>Four</p><p>Five</p></article>
           <article><p>Six</p><p>Seven</p><p>Eight</p><p>Nine</p></article><article><p>Ten</p></article>
           <div class=span>Span</div></div>",
    );
    let (_, y, _) = find(&pages[0], &document, "span").expect("spanner");
    assert!((y - 37.0 - 133.33).abs() < 0.02, "{y}");
}

#[test]
fn balances_columns_to_the_end_of_the_content_of_the_tallest() {
    // Five lines of 20px in two columns do not fit at an even share (50px, with `orphans` and
    // `widows` of 2): Chrome stretches the columns by just as much as the third line needs, to
    // 60px, though the paragraph broken in the first column reaches down to its end.
    let (document, pages) = render_document(
        "<style>body { margin: 0; font: 16px/20px Inter } .cols { columns: 2; column-gap: 20px }
           p { margin: 0 } .span { column-span: all }</style>
         <div class=cols><p>a<br>b<br>c<br>d<br>e</p><div class=span>Span</div></div>",
    );
    let (_, y, _) = find(&pages[0], &document, "span").expect("spanner");
    assert_eq!(y, 37.0 + 60.0);
}

#[test]
fn keeps_running_elements_in_the_flow_where_a_later_declaration_wins() {
    // Where a later declaration wins, the element stays in the flow, as in Chrome, which knows
    // no `running()`. `@supports` answers for Fagus, which has running elements
    // (nforst/fagus#4): there the element goes into the page margin.
    let in_flow = |css: &str| {
        let (_, pages) = render_document(&format!(
            "<style>@page {{ @bottom-center {{ content: element(foot) }} }} body {{ font: 12px Inter }}
               {css}</style><div class=foot>FOOT</div><p>text</p>"
        ));
        let mut found = Vec::new();
        runs(&pages[0].fragments[0], 0.0, &mut found);
        found.iter().any(|(_, run)| run.text.contains("FOOT"))
    };
    assert!(!in_flow(
        "@supports (position: running(x)) { .foot { position: running(foot) } }"
    ));
    assert!(in_flow(
        "@supports not (position: running(x)) { .foot { position: relative } }
         .foot { position: running(foot); position: relative }"
    ));
    // Fagus' own support takes it out of the flow, into the page margin.
    assert!(!in_flow(".foot { position: running(foot) }"));
}

#[test]
fn keeps_the_widows_of_the_rest_of_a_paragraph_in_balanced_columns() {
    // The two lines left for the second page do not break between the columns: one line on
    // either side of the break is fewer than `orphans` and `widows` ask for, so both go in the
    // first column, as in Chrome.
    let lines: Vec<String> = (1..=32).map(|line| format!("x{line:02}")).collect();
    let (_, pages) = render_document(&format!(
        "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
           p {{ margin: 0 }} .cols {{ columns: 2; column-gap: 20px }}</style>
         <div class=cols><p>{}</p></div>",
        lines.join("<br>")
    ));
    assert_eq!(pages.len(), 2);
    let mut found = Vec::new();
    runs(&pages[1].fragments[0], 0.0, &mut found);
    let x = |text: &str| {
        found
            .iter()
            .find(|(_, run)| run.text.contains(text))
            .map(|(x, _)| *x)
    };
    assert_eq!((x("x31"), x("x32")), (Some(0.0), Some(0.0)));
}
