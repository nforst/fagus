//! Content that overflows its page or its box as Chrome prints it: monolithic flex and grid
//! items taller than the rest of the page, which end their container with the overflow as the
//! content of a block container does, or push the container on the next page where they go on,
//! the lines of floats that overflow their box past the end of the page, the columns of
//! multi-column containers of a fixed height, and grid rows that an item spans. The numbers are
//! Chrome's, measured with the same font.

use super::{find, render_document};
use crate::fragment::{Fragment, FragmentKind, Page};

/// Renders `body` after a line on pages 400px × 300px with margins of 20px; `.m` is a box with
/// size containment 700px tall.
fn render(body: &str) -> (fagus_dom::Document, Vec<Page>) {
    render_document(&format!(
        "<!doctype html><style>@page {{ size: 400px 300px; margin: 20px }}
         body {{ margin: 0; font: 16px/20px Inter }} p {{ margin: 0 }}
         .m {{ contain: size; height: 700px; width: 200px }}</style>{body}"
    ))
}

/// The page (from 0) of the element with class `class` and the top of its border box in the
/// page area.
fn top_of(document: &fagus_dom::Document, pages: &[Page], class: &str) -> Option<(usize, f32)> {
    pages
        .iter()
        .enumerate()
        .find_map(|(index, page)| find(page, document, class).map(|(_, y, _)| (index, y - 20.0)))
}

#[test]
fn ends_flex_and_grid_containers_with_monolithic_overflow() {
    // The item moves to the second page and reaches 180px into the fourth, where the
    // paragraph after follows; the container ends with it, which leaves a fifth page.
    for container in [
        "display: flex",
        "display: grid; grid-template-columns: 200px 50px",
        "display: flex; flex-wrap: wrap",
        "display: flex; align-items: start",
    ] {
        let (document, pages) = render(&format!(
            "<p>before</p><div class=c style='{container}'><div class=m></div>
             <div style='height: 100px; width: 50px'></div></div><p class=after>after</p>"
        ));
        assert_eq!(pages.len(), 5, "{container}");
        assert_eq!(
            top_of(&document, &pages, "after"),
            Some((3, 180.0)),
            "{container}"
        );
        // The container is laid out whole on the second page, its overflow shown after it.
        let (_, y, container_fragment) = find(&pages[1], &document, "c").expect("container");
        assert_eq!((y, container_fragment.rect.height.to_px()), (20.0, 700.0));
    }
    // So in a block container, and at the top of the first page.
    let (_, pages) =
        render("<p>before</p><div><div style='display: flex'><div class=m></div></div></div><p>after</p>");
    assert_eq!(pages.len(), 5);
    let (_, pages) = render("<div style='display: flex'><div class=m></div></div><p>after</p>");
    assert_eq!(pages.len(), 4);
}

#[test]
fn adds_no_page_for_overflow_inside_flex_items() {
    // A flex or grid item that ends with the overflow is a parallel flow of the container's
    // line already: the container leaves no page to it.
    for item in [
        "<div style='display: flex'><div class=m></div></div>",
        "<div><div class=m></div></div>",
    ] {
        for container in ["display: flex", "display: grid"] {
            let (document, pages) = render(&format!(
                "<p>before</p><div style='{container}'>{item}</div><p class=after>after</p>"
            ));
            assert_eq!(pages.len(), 4, "{container} {item}");
            assert_eq!(
                top_of(&document, &pages, "after"),
                Some((3, 180.0)),
                "{container} {item}"
            );
        }
    }
    // An item that goes on below the overflow (stretched to it) keeps the line going instead.
    let (document, pages) = render(
        "<p>before</p><div style='display: flex'><div class=m></div><div>beside</div></div>
         <p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 180.0)));
}

#[test]
fn moves_monolithic_items_of_column_flex_containers_to_the_next_page() {
    // A box with size containment does not break: it moves to the next page, as an image does,
    // and reaches 40px into the one after, where the next item follows.
    let (document, pages) = render(
        "<p>before</p><div style='display: flex; flex-direction: column'>
         <div class=m style='height: 300px'></div><p class=row2>row2</p></div><p>after</p>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(top_of(&document, &pages, "row2"), Some((2, 40.0)));
}

#[test]
fn pushes_row_flex_containers_below_the_overflow_of_items_that_go_on() {
    // The item goes on after its monolithic content, which reaches 40px into the third page:
    // the container goes on below it there, the item beside it with its rest.
    let (document, pages) = render(
        "<p>before</p><div style='display: flex'><div><div class=m style='height: 300px'></div>
         <p class=x>x</p></div><div>side</div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(top_of(&document, &pages, "x"), Some((2, 40.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 60.0)));
    // Overflow that covers a page leaves it empty.
    let (document, pages) = render(
        "<div style='display: flex'><div><p>y</p><div class=m style='height: 600px'></div>
         <p class=x>x</p></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "x"), Some((3, 80.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 100.0)));
    // An item that clones its decorations has its bottom padding below the overflow.
    let (document, pages) = render(
        "<div style='display: flex'><div style='box-decoration-break: clone; padding: 5px'>
         <p>y</p><div class=m style='height: 258px'></div><p class=x>x</p></div></div>
         <p class=after>after</p>",
    );
    assert_eq!(top_of(&document, &pages, "x"), Some((2, 13.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 38.0)));
}

/// The first page (from 0) with the text `text`, and where its run lies in the page area.
fn place(pages: &[Page], text: &str) -> Option<(usize, f32, f32)> {
    fn walk(fragment: &Fragment, text: &str, origin: (f32, f32)) -> Option<(f32, f32)> {
        let origin = (
            origin.0 + fragment.rect.x.to_px(),
            origin.1 + fragment.rect.y.to_px(),
        );
        if let FragmentKind::Text(run) = &fragment.kind
            && run.text.contains(text)
        {
            return Some(origin);
        }
        fragment
            .children
            .iter()
            .find_map(|child| walk(child, text, origin))
    }
    pages.iter().enumerate().find_map(|(index, page)| {
        let (x, y) = walk(page.fragments.first()?, text, (0.0, 0.0))?;
        Some((index, x - 20.0, y - 20.0))
    })
}

#[test]
fn continues_the_lines_overflowing_a_float_on_the_next_page() {
    // The float's box ends 25px above the end of the page; its fifth line would cross it and
    // goes on at the top of the next page, where the rest of the lines take no room from the
    // paragraph beside them.
    let words = |prefix: char, count: usize| {
        (1..=count)
            .map(|index| format!("{prefix}{index:02}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (_, pages) = render(&format!(
        "<div style='height: 175px'></div><div style='float: left; width: 60px; height: 60px'>{}</div>
         <p>{}</p>",
        words('f', 20),
        words('q', 80)
    ));
    assert_eq!(pages.len(), 2);
    assert_eq!(
        place(&pages, "f07").map(|(page, _, y)| (page, y)),
        Some((0, 235.0))
    );
    assert_eq!(place(&pages, "f09"), Some((1, 0.0, 0.0)));
    assert_eq!(place(&pages, "q39"), Some((1, 0.0, 0.0)));
}

/// Words `w01` to `w{count}`.
fn numbered_words(count: usize) -> String {
    (1..=count)
        .map(|index| format!("w{index:02}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn lays_out_overflow_columns_beside_multicol_containers_of_a_fixed_height() {
    // Three columns 100px tall hold 15 of the words; the rest goes on in columns beyond them,
    // past the container's right edge, rather than on the next page.
    let (document, pages) = render(&format!(
        "<div style='columns: 3; column-fill: auto; column-gap: 10px; height: 100px'><p>{}</p></div>
         <p class=after>after</p>",
        numbered_words(60)
    ));
    assert_eq!(pages.len(), 1);
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
    let (page, x, y) = place(&pages, "w46").expect("w46");
    assert_eq!((page, y), (0, 0.0));
    assert!(x > 360.0, "{x}");
}

#[test]
fn breaks_multicol_containers_of_a_fixed_height_with_the_rest_of_their_height() {
    // The columns get the 110px left on the first page; the content ends there, and the
    // container goes on for the 90px left of its height.
    let (document, pages) = render(&format!(
        "<div style='height: 150px'></div><div style='columns: 3; column-fill: auto; column-gap: 10px;
         height: 200px'><p>{}</p></div><p class=after>after</p>",
        numbered_words(30)
    ));
    assert_eq!(pages.len(), 2);
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 90.0)));
    // Content that does not end there goes on in the 90px on the next page.
    let (_, pages) = render(&format!(
        "<div style='height: 150px'></div><div style='columns: 3; column-fill: auto; column-gap: 10px;
         height: 200px; overflow: hidden'><p>{}</p></div><p>after</p>",
        numbered_words(90)
    ));
    assert_eq!(place(&pages, "w46"), Some((1, 0.0, 0.0)));
    assert_eq!(place(&pages, "w58").map(|(page, _, y)| (page, y)), Some((1, 0.0)));
}

#[test]
fn moves_monolithic_content_to_the_next_page_rather_than_into_a_later_column() {
    // Below other content on the page, a box 200px tall that does not fit in the first column
    // does not go in the second one either: it starts the next page, the second column of this
    // one left empty.
    let (document, pages) = render(
        "<div style='height: 150px'></div><div style='columns: 2; column-gap: 10px'><p>aa bb</p>
         <p>cc dd</p><div class=m style='height: 200px'></div><p class=x>after img</p></div><p>after</p>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(top_of(&document, &pages, "m"), Some((1, 0.0)));
    assert_eq!(place(&pages, "after img"), Some((1, 185.0, 0.0)));
}

#[test]
fn sizes_rows_spanned_by_an_item_up_to_their_growth_limits() {
    // The item across both rows needs 120px; the first row holds an item of 50px, which limits
    // its growth, so the second row takes the other 70px.
    let (document, pages) = render(
        "<div style='display: grid; grid-template-columns: 100px 100px'>
         <div class=a style='min-height: 50px'></div>
         <div style='grid-row: span 2; height: 120px'></div>
         <div class=b style='grid-column: 1; grid-row: 3; height: 10px'></div></div>",
    );
    let (_, _, a) = find(&pages[0], &document, "a").expect("a");
    assert_eq!(a.rect.height.to_px(), 50.0);
    assert_eq!(top_of(&document, &pages, "b").map(|(_, y)| y), Some(120.0));
}

#[test]
fn ends_formatting_contexts_with_their_monolithic_overflow() {
    // A `flow-root` box ending with the overflow ends with it: the bottom margin of its last
    // child and its own bottom padding, border and margin take no room after it, and on the
    // page where the overflow ends (180px down) the empty rest of the box leaves a fifth page.
    for style in [
        "",
        "padding-bottom: 5px",
        "border-bottom: 5px solid; margin-bottom: 8px",
    ] {
        let (document, pages) = render(&format!(
            "<p>before</p><div class=c style='display: flow-root; {style}'>
             <div class=m style='margin-bottom: 10px'></div></div><p class=after>after</p>"
        ));
        assert_eq!(pages.len(), 5, "{style}");
        assert_eq!(top_of(&document, &pages, "after"), Some((3, 180.0)), "{style}");
        let (_, _, container) = find(&pages[1], &document, "c").expect("container");
        assert_eq!(container.rect.height.to_px(), 700.0, "{style}");
    }
    // 140px into the third page, it leaves a fourth one; 125px into the fourth (half the page
    // or less), none.
    let (document, pages) = render(
        "<p>before</p><div style='display: flow-root; padding-bottom: 5px'>
         <div class=m style='height: 400px'></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 140.0)));
    let (document, pages) = render(
        "<p>before</p><div style='display: flow-root; padding-bottom: 5px'>
         <div class=m style='height: 645px'></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 125.0)));
    // A box that repeats its padding below the overflow keeps it there.
    let (document, pages) = render(
        "<p>before</p><div style='display: flow-root; padding-bottom: 5px; box-decoration-break: clone'>
         <div class=m style='height: 400px'></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 145.0)));
    // Content after the overflow keeps the box's padding below it.
    let (document, pages) = render(
        "<p>before</p><div style='display: flow-root; padding-bottom: 5px'>
         <div class=m style='height: 400px'></div><div></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 145.0)));
}

#[test]
fn ends_flex_lines_below_the_margins_of_overflowing_items() {
    // The line holds the item with its bottom margin: the paragraph follows 10px below the
    // overflow, while the page added after it depends on the overflow alone.
    for (item, pages_count, after) in [
        ("height: 700px; margin-bottom: 10px", 5, 190.0),
        ("height: 645px; margin-bottom: 10px", 4, 135.0),
        ("height: 655px", 5, 135.0),
        ("height: 400px; margin-bottom: 10px", 4, 150.0),
    ] {
        let (document, pages) = render(&format!(
            "<p>before</p><div class=c style='display: flex'><div class=m style='{item}'></div></div>
             <p class=after>after</p>"
        ));
        let page = if item.contains("400px") { 2 } else { 3 };
        assert_eq!(pages.len(), pages_count, "{item}");
        assert_eq!(top_of(&document, &pages, "after"), Some((page, after)), "{item}");
        let (_, _, container) = find(&pages[1], &document, "c").expect("container");
        assert!(container.rect.height.to_px() > 260.0, "{item}");
    }
    // An item whose top margin the break before it truncates: the container started on the
    // page before goes on with the line, which it ends with on the next one.
    let (document, pages) = render(
        "<p>before</p><div class=c style='display: flex'><div class=m style='margin-top: 10px'></div></div>
         <p class=after>after</p>",
    );
    assert_eq!(pages.len(), 5);
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 180.0)));
}

#[test]
fn adds_the_page_after_an_overflowing_inline_flex_box() {
    // The line with the inline-level box (and its strut below the box) reaches 184px into the
    // fourth page, as the box would in a block of its own.
    let (document, pages) = render(
        "<p>before</p><div style='display: inline-flex; width: 300px'><div class=m></div></div>
         <p class=after>after</p>",
    );
    assert_eq!(pages.len(), 5);
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 184.0)));
}

#[test]
fn moves_the_rest_of_a_grid_row_below_the_overflow_of_its_items() {
    // The row ends 10px below the item (its margin), which goes on below the overflow; the
    // grid's part on the second page holds the item whole.
    let (document, pages) = render(
        "<p>before</p><div class=c style='display: grid'><div class=m style='margin-bottom: 10px'></div></div>
         <p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 190.0)));
    let (_, _, container) = find(&pages[1], &document, "c").expect("container");
    assert_eq!(container.rect.height.to_px(), 700.0);
    assert!(find(&pages[2], &document, "c").is_none());
}

#[test]
fn grows_grids_on_the_page_after_the_overflow_of_items_that_go_on() {
    // The item's box of 300px reaches 40px past the end of the second page; the grid goes on
    // at the top of the third beside that overflow, and is 40px taller there than its content.
    let item = "<p>y</p><div class=m style='height: 300px'></div><p class=x>x</p>";
    let (document, pages) = render(&format!(
        "<div style='display: grid'><div>{item}</div></div><p class=after>after</p>"
    ));
    assert_eq!(pages.len(), 3);
    assert_eq!(top_of(&document, &pages, "x"), Some((2, 0.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 60.0)));
    // A row below takes 20px of the overflow on the second page, which moves the grid down
    // by as much on the third.
    let (document, pages) = render(&format!(
        "<div style='display: grid'><div>{item}</div><div class=row>row</div></div>
         <p class=after>after</p>"
    ));
    assert_eq!(top_of(&document, &pages, "x"), Some((2, 20.0)));
    assert_eq!(top_of(&document, &pages, "row"), Some((2, 40.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 80.0)));
    // Content that covers the third page as well: the grid goes on 120px down it, and its rest
    // of 240px reaches 100px into the fourth.
    let (document, pages) = render(
        "<div style='display: grid'><div><p>y</p><div class=m style='height: 600px'></div>
         <p class=x>x</p></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "x"), Some((2, 120.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 100.0)));
}

#[test]
fn adds_a_page_for_a_fixed_box_below_a_single_page() {
    // A fixed box wholly below the page area of a document of one page shows on a page added
    // for it, 10px down; one that starts in the page area, or below the last of two pages, does
    // not add one.
    let fixed = |top: u32| {
        format!(
            "<div class=f style='position: fixed; top: {top}px; left: 0; height: 20px; width: 100px'>f</div>"
        )
    };
    let (_, pages) = render(&format!("<p>one</p>{}", fixed(270)));
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[1].carried.len(), 1);
    let (_, pages) = render(&format!("<p>one</p>{}", fixed(250)));
    assert_eq!(pages.len(), 1);
    let (_, pages) = render(&format!(
        "<p>one</p><div style='height: 300px'></div>{}",
        fixed(270)
    ));
    assert_eq!(pages.len(), 2);
}

#[test]
fn starts_the_next_flex_line_beside_the_rest_of_items_grown_past_their_line() {
    // The first item's monolithic box moves to the second page and reaches 40px into the third:
    // the item goes on below it there, while the second line starts at the top of the page
    // beside it, as in Chrome (Blink expands the first line to the end of the second page).
    let (document, pages) = render(
        "<div style='display: flex; flex-wrap: wrap'><div style='width: 100%'><p>y</p>
         <div class=m style='height: 300px'></div><p class=x>x</p></div><div class=second>second line</div></div>
         <p class=after>after</p>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(top_of(&document, &pages, "x"), Some((2, 40.0)));
    assert_eq!(top_of(&document, &pages, "second"), Some((2, 0.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 60.0)));
}

#[test]
fn pushes_row_flex_containers_below_the_overflow_of_items_that_end_with_it() {
    // The first item ends on the second page, 385px past its end (and 10px of bottom margin
    // more); the second item goes on below that on the fourth page, the third left to the
    // overflow, as in Chrome.
    let (document, pages) = render(
        "<p>before</p><div style='display: flex; align-items: start'>
         <div class=m style='height: 645px; margin-bottom: 10px'></div>
         <div class=g style='height: 670px; width: 50px'></div></div><p class=after>after</p>",
    );
    assert_eq!(pages.len(), 6);
    let continued =
        |page: usize| find(&pages[page], &document, "g").map(|(_, y, g)| (y - 20.0, g.rect.height.to_px()));
    assert_eq!(continued(2), None);
    assert_eq!(continued(3), Some((135.0, 125.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((5, 25.0)));
}

/// The top and bottom of the fragment of the element with class `class` on page `page`, in the
/// page area.
fn span_of(document: &fagus_dom::Document, pages: &[Page], page: usize, class: &str) -> Option<(f32, f32)> {
    find(&pages[page], document, class)
        .map(|(_, y, fragment)| (y - 20.0, y - 20.0 + fragment.rect.height.to_px()))
}

#[test]
fn moves_grid_rows_below_items_that_grow_past_their_area_after_a_break() {
    let lines = |name: &str, count: usize| {
        (1..=count)
            .map(|line| format!("{name}{line}"))
            .collect::<Vec<_>>()
            .join("<br>")
    };
    // The first item breaks after four lines and needs more room on the second page than its
    // area has left: its row grows, and the next row starts below it after the gap, as in
    // Chrome, with the item that spans both rows reaching down to the end of the second.
    let (document, pages) = render(&format!(
        "<div style='height: 150px'></div><div style='display: grid; grid-template-columns: 1fr 1fr; gap: 12px'>
         <div class=s style='padding: 4px; border: 1px solid'>{}</div>
         <div class=k style='grid-row: span 2; padding: 4px; border: 1px solid'>{}</div>
         <div class=t style='padding: 4px; border: 1px solid'>t1</div></div><p>after</p>",
        lines("s", 6),
        lines("c", 8)
    ));
    assert_eq!(pages.len(), 2);
    let (_, s_end) = span_of(&document, &pages, 1, "s").expect("s");
    let (t_top, t_end) = span_of(&document, &pages, 1, "t").expect("t");
    assert_eq!((s_end, t_top), (45.0, 57.0));
    assert_eq!(
        span_of(&document, &pages, 1, "k").map(|(_, end)| end),
        Some(t_end)
    );
    // An item that spans an empty row below the one that grew reaches past it by the gap.
    let (document, pages) = render(&format!(
        "<div style='display: grid; grid-template-columns: 1fr 1fr; gap: 12px'>
         <div style='grid-row: span 2; height: 100px'></div><div style='height: 50px'></div>
         <div style='height: 45px'></div><div class=r style='border: 1px solid'>{}</div>
         <div class=k style='grid-row: span 2; border: 1px solid'>k</div></div><p>after</p>",
        lines("r", 7)
    ));
    assert_eq!(pages.len(), 2);
    let (_, r_end) = span_of(&document, &pages, 1, "r").expect("r");
    let (_, k_end) = span_of(&document, &pages, 1, "k").expect("k");
    assert_eq!(k_end - r_end, 12.0);
}

#[test]
fn breaks_stretched_grid_items_in_their_bottom_padding_above_it() {
    // The item spanning an empty row reaches 4px past the end of the first page, into its
    // bottom padding and border: as it is taller than its content, it breaks there rather than
    // moving its row to the next page, and its part ends above them, as in Chrome.
    let (document, pages) = render(
        "<div style='height: 12px'></div><div style='display: grid; grid-template-columns: 1fr 1fr; gap: 12px'>
         <div><div style='height: 240px'></div></div>
         <div class=k style='grid-row: span 2; padding-bottom: 10px; border: 1px solid'><div style='height: 20px'></div></div></div>
         <div class=after style='height: 10px'></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(span_of(&document, &pages, 0, "k"), Some((12.0, 253.0)));
    assert_eq!(span_of(&document, &pages, 1, "k"), Some((0.0, 4.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 4.0)));
}

#[test]
fn breaks_tables_after_rows_that_end_with_monolithic_overflow() {
    // The row's monolithic content ends the table 182px into the third page: the table goes on
    // there, its header repeated at the top over the overflow, and ends twice the spacing below
    // the overflow, where the paragraph after follows. The table box starts below the overflow,
    // the header above it; the cell without such content ends with the second page.
    let table = |height: u32, header: &str| {
        format!(
            "<p>before</p><table class=t style='border-spacing: 10px'>{header}<tr>
             <td style='padding: 0'><div class=m style='height: {height}px'></div></td>
             <td class=c style='padding: 0'>cell</td></tr></table><p class=after>after</p>"
        )
    };
    let header = "<thead><tr><td class=h>head</td></tr></thead>";
    let (document, pages) = render(&table(400, header));
    assert_eq!(pages.len(), 3);
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 202.0)));
    let (_, y, _) = find(&pages[2], &document, "h").expect("repeated header");
    assert_eq!(y, 20.0);
    let (_, y, part) = find(&pages[2], &document, "t").expect("table");
    assert_eq!((y - 20.0, part.rect.height.to_px()), (182.0, 20.0));
    let (_, y, cell) = find(&pages[1], &document, "c").expect("cell");
    assert_eq!(y - 20.0 + cell.rect.height.to_px(), 260.0);
    // Where the overflow covers the third page, that page holds the header, and the end of the
    // table takes the spacing at the top of the fourth, where the paragraph follows.
    let (document, pages) = render(&table(700, header));
    assert_eq!(pages.len(), 4);
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 10.0)));
    assert!(find(&pages[2], &document, "h").is_some());
    // Without a header, the row breaks before its monolithic content, which reaches 140px into
    // the fourth page, and the table ends the spacing below it.
    let (document, pages) = render(&table(400, ""));
    assert_eq!(top_of(&document, &pages, "after"), Some((3, 150.0)));
}

/// A document with `after` 10px tall after `body`.
fn before_after(body: &str) -> (fagus_dom::Document, Vec<Page>) {
    render(&format!("{body}<div class=after style='height: 10px'></div>"))
}

/// Four boxes 180px tall that avoid breaks inside them: one fits on a page, so every break
/// takes 80px of the height of the box around them.
const BOXES: &str = "<div class=b style='height: 180px; break-inside: avoid'></div>\
                     <div class=b style='height: 180px; break-inside: avoid'></div>\
                     <div class=b style='height: 180px; break-inside: avoid'></div>\
                     <div class=b style='height: 180px; break-inside: avoid'></div>";

#[test]
fn keeps_the_content_of_stretched_flex_items_past_their_second_page() {
    // The item is as tall as its container, 700px, and ends 180px down the third page; its
    // content of 900px goes on there and overflows it onto the fourth, as in Chrome. (The
    // content was lost from the third page on.)
    let (document, pages) = before_after(
        "<div style='display: flex; height: 700px'><div class=i style='width: 100%'>
         <div class=c style='height: 900px'></div></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 1, "i"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 2, "i"), Some((0.0, 180.0)));
    assert_eq!(span_of(&document, &pages, 2, "c"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 3, "c"), Some((0.0, 120.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 180.0)));
    // Each break takes 80px of the item's 720px: the fourth box overflows it.
    let (document, pages) = before_after(&format!(
        "<div style='display: flex; height: 720px'><div class=i style='width: 100%'>{BOXES}</div></div>"
    ));
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 2, "i"), Some((0.0, 200.0)));
    assert_eq!(span_of(&document, &pages, 2, "b"), Some((0.0, 180.0)));
    assert_eq!(span_of(&document, &pages, 3, "b"), Some((0.0, 180.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((2, 200.0)));
}

#[test]
fn ends_grid_items_in_rows_of_fixed_sizes_above_their_overflow() {
    // The item over two rows of 50px is 100px tall, and the grid with it: its content of 900px
    // overflows both down four pages, beside what follows the grid, as in Chrome. (The item
    // went on down the pages and ended with a negative height.)
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 50px 50px'><div class=i style='grid-row: 1 / 3'>
         <div class=c style='height: 900px'></div></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 1, "i"), Some((0.0, 0.0)));
    assert_eq!(span_of(&document, &pages, 3, "i"), Some((0.0, 0.0)));
    assert_eq!(span_of(&document, &pages, 3, "c"), Some((0.0, 120.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
    // So does an item in one row.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 50px 50px'><div class=i>
         <div class=c style='height: 900px'></div></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 50.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
    // And one over flexible rows that share a fixed height.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 1fr 1fr 1fr; height: 150px'>
         <div class=i style='grid-row: 1 / 4'><div class=c style='height: 900px'></div></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 150.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 150.0)));
    // Rows taller than the page break, and the item ends with them on the second page.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 200px 200px'><div class=i style='grid-row: 1 / 3'>
         <div class=c style='height: 900px'></div></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 1, "i"), Some((0.0, 140.0)));
    assert_eq!(span_of(&document, &pages, 2, "i"), Some((0.0, 0.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 140.0)));
    // An item that avoids breaks inside it moves to the next page with its rows and keeps
    // their height there.
    let (document, pages) = before_after(
        "<div style='height: 200px'></div><div style='display: grid; grid-template-rows: 50px 50px'>
         <div class=i style='grid-row: 1 / 3; break-inside: avoid'><div class=c style='height: 500px'></div>
         </div></div>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(span_of(&document, &pages, 1, "i"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 2, "c"), Some((0.0, 240.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 100.0)));
    // A grid whose rows fit on the page, but not its bottom padding below them, stays there
    // with that content, and its padding goes on alone.
    let (document, pages) = before_after(
        "<div style='height: 140px'></div>
         <div class=g style='display: grid; grid-template-rows: 50px 50px; padding-bottom: 30px'>
         <div class=i style='grid-row: 1 / 3'><div class=c style='height: 500px'></div></div></div>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((140.0, 240.0)));
    assert_eq!(span_of(&document, &pages, 1, "g"), Some((0.0, 30.0)));
    assert_eq!(span_of(&document, &pages, 2, "c"), Some((0.0, 120.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 30.0)));
}

#[test]
fn grows_grid_rows_only_where_their_sizes_follow_their_content() {
    // The end of the item on its last page, and where the box after the grid is.
    let ends = |grid: &str, item: &str| {
        let (document, pages) = before_after(&format!(
            "<div style='display: grid; {grid}'><div class=i style='{item}'>{BOXES}</div></div>"
        ));
        assert_eq!(pages.len(), 4);
        let last = if span_of(&document, &pages, 3, "i") == Some((0.0, 0.0)) {
            2
        } else {
            3
        };
        (
            last,
            span_of(&document, &pages, last, "i").map(|(_, end)| end),
            top_of(&document, &pages, "after"),
        )
    };
    // The breaks take 240px of the item's 720px. In a row of an automatic size, the item
    // grows by them and ends below its fourth box, as in Chrome
    // (`MinBlockSizeShouldEncompassIntrinsicSize`).
    assert_eq!(ends("", ""), (3, Some(180.0), Some((3, 180.0))));
    assert_eq!(
        ends("grid-template-rows: minmax(100px, auto)", ""),
        (3, Some(180.0), Some((3, 180.0)))
    );
    // So it does in a flexible row, past the end of the grid of a fixed height.
    let (last, end, _) = ends("grid-template-rows: 1fr; height: 720px", "");
    assert_eq!((last, end), (3, Some(180.0)));
    // In a row of a fixed size it ends 720px down, on the third page, and the fourth box
    // overflows it, as it does in a row of a fixed minimum in a grid of a fixed height, and
    // over several rows, whatever their sizes.
    assert_eq!(
        ends("grid-template-rows: 720px", ""),
        (2, Some(200.0), Some((2, 200.0)))
    );
    assert_eq!(
        ends("grid-template-rows: minmax(100px, auto); height: 720px", ""),
        (2, Some(200.0), Some((2, 200.0)))
    );
    assert_eq!(
        ends("grid-template-rows: auto auto", "grid-row: 1 / 3"),
        (2, Some(200.0), Some((2, 200.0)))
    );
}

#[test]
fn goes_on_with_the_overflow_of_grid_items_beside_the_rows_after_them() {
    // Both rows are on the first page, with the item over them, the one in the first and the
    // one in the second; the content of the first two overflows them on the pages after, and
    // moves nothing, as in Chrome.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 50px 50px; grid-template-columns: 1fr 1fr'>
         <div class=i style='grid-row: 1 / 3'><div class=c style='height: 900px'></div></div>
         <div class=j><div class=d style='height: 400px'></div></div><div class=k></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 0, "j"), Some((0.0, 50.0)));
    assert_eq!(span_of(&document, &pages, 0, "k"), Some((50.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 0, "d"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 1, "d"), Some((0.0, 140.0)));
    assert_eq!(span_of(&document, &pages, 3, "c"), Some((0.0, 120.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
    // The first row of 300px ends 40px down the second page, where the second row follows it
    // beside the content that overflows the first.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 300px 40px'>
         <div class=i><div class=c style='height: 700px'></div></div><div class=j></div></div>",
    );
    assert_eq!(pages.len(), 3);
    assert_eq!(span_of(&document, &pages, 1, "i"), Some((0.0, 40.0)));
    assert_eq!(span_of(&document, &pages, 1, "j"), Some((40.0, 80.0)));
    assert_eq!(span_of(&document, &pages, 1, "c"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 2, "c"), Some((0.0, 180.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 80.0)));
    // The second row breaks, and goes on at the top of the second page beside that content.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 50px 300px; gap: 10px'>
         <div class=i><div class=c style='height: 900px'></div></div>
         <div class=j><div class=d style='height: 280px'></div></div></div>",
    );
    assert_eq!(pages.len(), 4);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 50.0)));
    assert_eq!(span_of(&document, &pages, 0, "j"), Some((60.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 1, "j"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 1, "d"), Some((0.0, 80.0)));
    assert_eq!(span_of(&document, &pages, 1, "c"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 3, "c"), Some((0.0, 120.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((1, 100.0)));
    // An item as tall as its content, which its row of 50px does not grow for, goes on past
    // the grid too.
    let (document, pages) = before_after(
        "<div style='display: grid; grid-template-rows: 50px 50px'><div class=i style='align-self: start'>
         <div class=c style='height: 500px'></div></div><div class=j></div></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 260.0)));
    assert_eq!(span_of(&document, &pages, 0, "j"), Some((50.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 1, "i"), Some((0.0, 240.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
}

#[test]
fn ends_flex_lines_above_the_overflow_of_items_that_end_on_the_page() {
    // The item of 100px ends on the first page, and the line with it: the item beside it is
    // stretched to 100px, and the content that overflows the first goes on beside what follows
    // the container, as in Chrome. (The line went on down the pages with that content.)
    let (document, pages) = before_after(
        "<div style='display: flex'><div class=i style='height: 100px; width: 50%'>
         <div class=c style='height: 500px'></div></div><div class=j style='width: 50%'></div></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 0, "j"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 1, "c"), Some((0.0, 240.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
    // The next line follows it on the page.
    let (document, pages) = before_after(
        "<div style='display: flex; flex-wrap: wrap'><div class=i style='height: 100px; width: 100%'>
         <div class=c style='height: 500px'></div></div><div class=j style='width: 50%; height: 30px'></div></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(span_of(&document, &pages, 0, "j"), Some((100.0, 130.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 130.0)));
    // So does a line end with a grid whose rows are of fixed sizes.
    let (document, pages) = before_after(
        "<div style='display: flex'><div style='display: grid; grid-template-rows: 50px 50px; width: 50%'>
         <div class=i style='grid-row: 1 / 3'><div class=c style='height: 500px'></div></div></div>
         <div class=j style='width: 50%'></div></div>",
    );
    assert_eq!(pages.len(), 2);
    assert_eq!(span_of(&document, &pages, 0, "i"), Some((0.0, 100.0)));
    assert_eq!(span_of(&document, &pages, 0, "j"), Some((0.0, 100.0)));
    assert_eq!(top_of(&document, &pages, "after"), Some((0, 100.0)));
}
