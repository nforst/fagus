//! Documents that once broke an invariant, found by the fuzz target `invariants` or by hand:
//! they hold them. Where the output or an error shows the bug as well, its test is in
//! `crates/fagus/tests/robustness.rs`.

use fagus::{Document, Engine};

fn holds(html: &str) {
    if let Err(violation) = fagus_invariants::check(&Engine::new(), &Document::html(html)) {
        panic!("{html}\n{violation}");
    }
}

#[test]
fn a_document_without_a_root_box_has_a_page() {
    // It had no pages.
    holds("<html style='display: none'>x");
    holds("<style>@page { @top-center { content: counter(page) } }</style><html style='display: none'>");
}

#[test]
fn collapsed_borders_in_columns_narrower_than_their_joints_have_no_negative_length() {
    // The border over a column stops short of the vertical borders that win its joints; in a
    // column narrower than those, its box was laid out with a negative width (and drew nothing,
    // as it draws nothing now).
    holds(
        "<style>table { width: 10px; border-collapse: collapse; table-layout: fixed } \
         td { border: 4px solid; padding: 0 }</style><table><tr><td>a<td colspan=2>b<td>c</table>",
    );
    holds(
        "<style>table { width: 150px; border-collapse: collapse; table-layout: fixed } \
         td { border: 1px solid; padding: 0 }</style>\
         <table><tr><td style='width: 148px'>a<td colspan=2>b<td>c<td>d<td>e<td>f<td>g<td>h<td>i<td>j</table>",
    );
}

#[test]
fn text_at_sizes_and_spacings_past_any_number_has_finite_glyphs() {
    // `zoom` times `font-size` was infinite, and the offsets of the glyphs no numbers.
    holds("<div style='zoom: 1e30'><p style='font-size: 1e30px'>z</p></div>");
    // The advances were infinite or no numbers, the text of negative width.
    holds("<p style='letter-spacing: 3e38px'>abc</p>");
    holds("<p style='letter-spacing: calc(infinity * 1px)'>abc</p>");
    holds("<div style='zoom: 1e30; letter-spacing: 1e30px'>y z</div>");
}

#[test]
fn a_grid_item_that_spans_rows_and_overflows_across_pages_has_no_negative_height() {
    // Found by the fuzz target. Chrome ends the box of the item after its 100 pixels and lets
    // the content overflow; Fagus drew it down four pages and gave it -700 pixels on the fifth.
    holds(
        "<style>@page { size: 300px 200px; margin: 0 } body { margin: 0 } \
         .grid { display: grid; grid-template-rows: 50px 50px } .item { grid-row: 1 / 3 }</style>\
         <div class=grid><div class=item><div style='height: 900px'></div></div></div>",
    );
}
