//! Where Chrome breaks in a few more places: content after a box that ends with monolithic
//! content taller than a page, a flex item moved out of a container of fixed height, the first
//! column below a spanner whose best break is a poor one, and rows that a cell spans. The
//! numbers are Chrome's, measured with the same font.

use super::render_document;
use crate::fragment::{Fragment, FragmentKind, Page};

/// Renders `body` on pages 400px × 300px without margins.
fn render(css: &str, body: &str) -> Vec<Page> {
    render_document(&format!(
        "<style>@page {{ size: 400px 300px; margin: 0 }} body {{ margin: 0; font: 16px/20px Inter }}
         p {{ margin: 0 }} {css}</style>{body}"
    ))
    .1
}

/// The first page (from 0) with the text `text`, and where the line with it lies on that page.
fn place(pages: &[Page], text: &str) -> Option<(usize, f32, f32)> {
    fn walk(fragment: &Fragment, text: &str, origin: (f32, f32)) -> Option<(f32, f32)> {
        if let FragmentKind::Text(run) = &fragment.kind
            && run.text.contains(text)
        {
            return Some((
                origin.0 + fragment.rect.x.to_px(),
                origin.1 + fragment.rect.y.to_px(),
            ));
        }
        let origin = (
            origin.0 + fragment.rect.x.to_px(),
            origin.1 + fragment.rect.y.to_px(),
        );
        fragment
            .children
            .iter()
            .find_map(|child| walk(child, text, origin))
    }
    pages.iter().enumerate().find_map(|(index, page)| {
        let (x, y) = walk(page.fragments.first()?, text, (0.0, 0.0))?;
        Some((index, x, y))
    })
}

/// Paragraphs `after 1` to `after {count}`.
fn after(count: usize) -> String {
    (1..=count)
        .map(|line| format!("<p>after {line}</p>"))
        .collect::<Vec<_>>()
        .concat()
}

#[test]
fn moves_the_content_on_below_a_box_that_ends_with_overflow() {
    // The box 470px tall moves to the second page and ends 170px down the third, where the
    // paragraphs go on. Its parent records the rest of it in a parallel flow and moves it down
    // by those 170px once more on the third page: 40px past its end, where the fourth page
    // starts.
    let wrapped = render(
        "",
        &format!(
            "<p>before</p><div><div style='contain: size; height: 470px'></div></div>{}",
            after(12)
        ),
    );
    assert_eq!(place(&wrapped, "after 1"), Some((2, 0.0, 170.0)));
    assert_eq!(place(&wrapped, "after 7"), Some((3, 0.0, 40.0)));
    // Without the box around it, or with content after it in that box, nothing moves.
    let bare = render(
        "",
        &format!(
            "<p>before</p><div style='contain: size; height: 470px'></div>{}",
            after(12)
        ),
    );
    assert_eq!(place(&bare, "after 7"), Some((3, 0.0, 0.0)));
}

#[test]
fn adds_a_page_for_the_rest_of_a_box_that_ends_with_overflow() {
    let pages = render(
        "",
        "<p>before</p><div><div style='contain: size; height: 470px'></div></div><p>after 1</p>",
    );
    assert_eq!(place(&pages, "after 1"), Some((2, 0.0, 170.0)));
    assert_eq!(pages.len(), 4);
}

#[test]
fn keeps_a_flex_container_of_fixed_height_at_its_height_after_a_break() {
    // The item moves to the second page, where the container has 10px left: the paragraph
    // after it follows there, the item overflowing the container.
    let pages = render(
        "",
        "<div style='height: 210px'></div><div style='display: flex; height: 100px; \
         align-items: flex-end'><p>end</p></div><p>after</p>",
    );
    assert_eq!(place(&pages, "end"), Some((1, 0.0, 0.0)));
    assert_eq!(place(&pages, "after"), Some((1, 0.0, 10.0)));
}

/// Columns continued from the first page, a spanner `spanner` px tall below them on the second,
/// and a heading of three lines that avoids a break after it, with a paragraph.
fn below_spanner(spanner: u32) -> Vec<Page> {
    let lines = (1..=52)
        .map(|line| format!("w{line}<br>"))
        .collect::<Vec<_>>()
        .concat();
    let words: Vec<String> = (1..=30).map(|word| format!("x{word}")).collect();
    render(
        &format!(
            ".cols {{ columns: 2; column-gap: 20px }} h2 {{ column-span: all; margin: 0; \
             font: 16px/20px Inter; height: {spanner}px }} h3 {{ font: 16px/20px Inter; \
             margin: 10px 0 0; break-after: avoid }}"
        ),
        &format!(
            "<div class=cols><p>{lines}</p><h2>Spanner</h2><h3>Head one two three four five \
             six</h3><p>{}</p></div>",
            words.join(" ")
        ),
    )
}

#[test]
fn breaks_inside_a_heading_rather_than_after_it_below_a_spanner() {
    // 60px are left below the spanner: the heading and a line of the paragraph do not fit in
    // the first column, which breaks between the heading's lines (fewer than `orphans`) rather
    // than after it (which `break-after` avoids), as Chrome ranks those breaks.
    let pages = below_spanner(20);
    assert_eq!(place(&pages, "Head"), Some((1, 0.0, 250.0)));
    assert_eq!(place(&pages, "five"), Some((1, 210.0, 240.0)));
    assert_eq!(place(&pages, "x1 "), Some((1, 210.0, 260.0)));
}

#[test]
fn moves_the_columns_below_a_spanner_whose_last_column_breaks_poorly() {
    // With 50px left, the second column would hold a single line of the paragraph after the
    // rest of the heading: the row of columns moves to the next page, as in Chrome.
    let pages = below_spanner(30);
    assert_eq!(
        place(&pages, "Head").map(|(page, _, y)| (page, y)),
        Some((2, 10.0))
    );
}

/// A table of rows 25px tall, `offset` px down the page, with a cell that spans two rows at the
/// end of the first page.
fn spanned_rows(offset: f32) -> Vec<Page> {
    let rows = (1..=10)
        .map(|row| format!("<tr><td>g{row}</td><td>row {row}</td></tr>"))
        .collect::<Vec<_>>()
        .concat();
    render(
        "table { border-collapse: collapse; width: 100% } td { border: 1px solid #777; \
         padding: 2px; vertical-align: top } td[rowspan] { width: 60px }",
        &format!(
            "<div style='height: {offset}px'></div><table>{rows}<tr><td rowspan=2>Group<br>32</td>\
             <td>first</td></tr><tr><td>second</td></tr><tr><td>g</td><td>after</td></tr></table>"
        ),
    )
}

#[test]
fn breaks_rows_that_a_cell_spans_where_its_content_and_frame_fit() {
    // The spanning cell's content, bottom padding and border fit on the first page: its first
    // row stays there, even where the cell breaks in its bottom padding.
    for offset in [0.0, 1.0, 4.5] {
        let pages = spanned_rows(offset);
        assert_eq!(place(&pages, "first").map(|(page, ..)| page), Some(0), "{offset}");
        assert_eq!(
            place(&pages, "second").map(|(page, ..)| page),
            Some(1),
            "{offset}"
        );
    }
    // Where they do not (by half a pixel), the rows move to the next page together.
    for offset in [4.6, 5.0, 7.0] {
        let pages = spanned_rows(offset);
        assert_eq!(place(&pages, "first").map(|(page, ..)| page), Some(1), "{offset}");
    }
}

#[test]
fn breaks_against_orphans_where_every_break_before_is_avoided() {
    // The box and the heading avoid the breaks after them: a break after the first line of the
    // paragraph, which leaves fewer lines than `orphans` asks for, is the better one.
    let pages = render(
        ".k { break-after: avoid }",
        "<div class=k style='height: 260px'></div><p class=k>Head</p><p>l1<br>l2<br>l3<br>l4</p>",
    );
    assert_eq!(
        place(&pages, "l1").map(|(page, _, y)| (page, y)),
        Some((0, 280.0))
    );
    assert_eq!(place(&pages, "l2").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
}

#[test]
fn breaks_between_grid_rows_that_an_item_spans_where_a_break_is_forced() {
    // The item across both rows stays whole on the first page; the second row goes on to the
    // next one.
    let pages = render(
        "",
        "<div style='height: 100px'></div><div style='display: grid; grid-template-columns: 1fr 1fr'>
         <div style='grid-row: span 2'>a1<br>a2<br>a3<br>a4</div><div style='break-after: page'>b1</div>
         <div>c1</div></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(
        place(&pages, "a4").map(|(page, _, y)| (page, y)),
        Some((0, 160.0))
    );
    assert_eq!(place(&pages, "c1"), Some((1, 200.0, 0.0)));
    // A forced break before an item of the second row is no break before the grid.
    let pages = render(
        "",
        "<div style='height: 100px'></div><div style='display: grid; grid-template-columns: 1fr 1fr 1fr'>
         <div>s1</div><div>b1</div><div style='grid-row: span 2'>v1<br>v2<br>v3<br>v4<br>v5</div>
         <div style='break-before: page'>and1</div><div>c1</div></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(
        place(&pages, "s1").map(|(page, _, y)| (page, y)),
        Some((0, 100.0))
    );
    assert_eq!(place(&pages, "and1"), Some((1, 0.0, 0.0)));
}

#[test]
fn breaks_inside_a_grid_item_that_avoids_it_where_no_break_before_is_better() {
    // Every break before the second row is avoided: it breaks inside the item that avoids that.
    let body = "<div style='display: grid'><div style='break-after: avoid'>x1</div>
         <div style='break-inside: avoid'>y1<br>y2<br>y3<br>y4<br>y5</div></div>";
    let pages = render(
        "",
        &format!("<div style='height: 200px; break-after: avoid'></div>{body}"),
    );
    assert_eq!(
        place(&pages, "y3").map(|(page, _, y)| (page, y)),
        Some((0, 260.0))
    );
    assert_eq!(place(&pages, "y4").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
    // A break before the grid is the better one where it is not avoided.
    let pages = render("", &format!("<div style='height: 200px'></div>{body}"));
    assert_eq!(place(&pages, "x1").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
}

#[test]
fn truncates_the_last_margin_of_a_box_that_clones_its_decorations_in_balanced_columns() {
    // The columns are as tall as the content of the second one with the box's padding and
    // border below it, without the bottom margin of its last paragraph.
    let pages = render(
        ".c { box-decoration-break: clone; border: 2px solid; padding: 6px } p { margin: 0 0 8px }",
        "<div style='columns: 2; column-gap: 20px'><div class=c><p>p1<br>p2<br>p3</p>
         <p>p4<br>p5<br>p6</p><p>p7<br>p8</p></div></div><p>after</p>",
    );
    assert_eq!(place(&pages, "p6"), Some((0, 218.0, 8.0)));
    assert_eq!(place(&pages, "after"), Some((0, 0.0, 124.0)));
}

#[test]
fn balances_columns_by_the_least_room_their_content_lacks() {
    // From the even share (125px), the second column needs 24.9px more for its next box, less
    // than the first: the columns grow to 150px with the second box in the second column,
    // although 0.1px more would hold it in the first one.
    let pages = render(
        "div > div { break-inside: avoid }",
        "<div style='columns: 2; column-gap: 0'><div style='height: 100.1px'>A</div>
         <div style='height: 50px'>B</div><div style='height: 100px'>C</div></div><p>after</p>",
    );
    assert_eq!(place(&pages, "B"), Some((0, 200.0, 0.0)));
    assert_eq!(place(&pages, "C"), Some((0, 200.0, 50.0)));
    assert_eq!(place(&pages, "after"), Some((0, 0.0, 150.0)));
}

#[test]
fn moves_a_box_that_avoids_breaks_where_its_overflowing_content_would_break() {
    // The box of 200px fits below the line, but its content of 300px would break across the
    // end of the page: the box moves to the next page, as in Chrome.
    let pages = render(
        "",
        "<p>before</p><div style='height: 200px; break-inside: avoid'>
         <div style='height: 280px'></div><p>inside</p></div>",
    );
    assert_eq!(place(&pages, "inside").map(|(page, ..)| page), Some(1));
    // Without the overflow it stays.
    let pages = render(
        "",
        "<p>before</p><div style='height: 200px; break-inside: avoid'>
         <div style='height: 100px'></div><p>inside</p></div>",
    );
    assert_eq!(place(&pages, "inside").map(|(page, ..)| page), Some(0));
}

#[test]
fn breaks_inside_the_list_before_a_box_that_avoids_the_break_before_it() {
    // The subtotal does not fit below the list and avoids the break before it: the list breaks
    // before its last item, which goes on with the subtotal, rather than the group moving
    // whole with its heading, as in Chrome.
    let items = "<li style='height: 40px; break-inside: avoid'>item 1</li>
                 <li style='height: 40px; break-inside: avoid'>item 2</li>
                 <li style='height: 40px; break-inside: avoid'>item 3</li>
                 <li style='height: 40px; break-inside: avoid'>item 4</li>
                 <li style='height: 40px; break-inside: avoid'>item 5</li>
                 <li style='height: 40px; break-inside: avoid'>item 6</li>";
    let pages = render(
        "ol { margin: 0 }",
        &format!(
            "<p style='height: 30px'>before</p><div><h3 style='margin: 0; break-after: avoid'>heading</h3>
             <ol>{items}</ol><p style='break-before: avoid'>subtotal</p></div>"
        ),
    );
    assert_eq!(place(&pages, "heading").map(|(page, ..)| page), Some(0));
    assert_eq!(place(&pages, "item 6").map(|(page, ..)| page), Some(1));
    assert_eq!(place(&pages, "item 5").map(|(page, ..)| page), Some(0));
    assert_eq!(place(&pages, "subtotal").map(|(page, ..)| page), Some(1));
}

#[test]
fn breaks_inside_a_box_that_avoids_it_after_a_heading_at_the_top_of_the_page() {
    // The heading avoids the break after it and starts the page: breaking inside the box that
    // avoids breaks inside is as poor as breaking after the heading, and later, so the box
    // breaks with the heading above it, as in Chrome (nforst/fagus#5).
    let lines = |prefix: &str, count: usize| {
        (1..=count)
            .map(|line| format!("{prefix}{line}<br>"))
            .collect::<Vec<_>>()
            .concat()
    };
    let head = "<p style='break-after: avoid'>Head</p>";
    let pages = render(
        "",
        &format!("{head}<div style='break-inside: avoid'>{}</div>", lines("l", 20)),
    );
    assert_eq!(
        place(&pages, "Head").map(|(page, _, y)| (page, y)),
        Some((0, 0.0))
    );
    assert_eq!(
        place(&pages, "l14").map(|(page, _, y)| (page, y)),
        Some((0, 280.0))
    );
    assert_eq!(place(&pages, "l15").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
    // Also where the box alone would fit on the next page (with `widows` at 2).
    let pages = render(
        "",
        &format!("{head}<div style='break-inside: avoid'>{}</div>", lines("l", 15)),
    );
    assert_eq!(
        place(&pages, "l13").map(|(page, _, y)| (page, y)),
        Some((0, 260.0))
    );
    assert_eq!(place(&pages, "l14").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
    // And inside a row group of a table that avoids breaks inside.
    let rows = ["a", "b", "c", "d"]
        .map(|row| format!("<tr><td>{}</td></tr>", lines(row, 4)))
        .concat();
    let pages = render(
        "td { padding: 0 } table { border-spacing: 0 }",
        &format!("{head}<table><tbody style='break-inside: avoid'>{rows}</tbody></table>"),
    );
    assert_eq!(
        place(&pages, "d2").map(|(page, _, y)| (page, y)),
        Some((0, 280.0))
    );
    assert_eq!(place(&pages, "d3").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
    // Where content before the heading leaves a break that nothing avoids, the heading moves
    // on with the box, which breaks on the next page.
    let pages = render(
        "",
        &format!(
            "<p>before</p>{head}<div style='break-inside: avoid'>{}</div>",
            lines("l", 20)
        ),
    );
    assert_eq!(
        place(&pages, "Head").map(|(page, _, y)| (page, y)),
        Some((1, 0.0))
    );
    assert_eq!(place(&pages, "l15").map(|(page, _, y)| (page, y)), Some((2, 0.0)));
}

#[test]
fn breaks_between_boxes_before_rather_than_inside_one_that_avoids_it() {
    // The break before the last box is avoided, but the box before it holds a break that
    // nothing avoids (between its second and third box, deeper in): that one is taken rather
    // than a break inside the last box, which avoids breaks inside, as in Chrome
    // (css/css-break/break-between-avoid-003.html).
    let square = |text: &str| format!("<div style='height: 90px; break-inside: avoid'>{text}</div>");
    let pages = render(
        "",
        &format!(
            "<div>{}<div><div>{}{}</div></div></div><div style='break-before: avoid; height: \
             90px; break-inside: avoid'>s4</div>",
            square("s1"),
            square("s2"),
            square("s3")
        ),
    );
    assert_eq!(place(&pages, "s2").map(|(page, _, y)| (page, y)), Some((0, 90.0)));
    assert_eq!(place(&pages, "s3").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
    assert_eq!(place(&pages, "s4").map(|(page, _, y)| (page, y)), Some((1, 90.0)));
}
