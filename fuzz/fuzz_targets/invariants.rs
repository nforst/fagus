//! Renders arbitrary markup as `render` does, twice, and checks what holds for the layout of
//! every document (crates/fagus-invariants): finite numbers, no negative sizes, no placeholders
//! left, a consistent page count and reported layout, and the same output both times. A target
//! of its own, as rendering twice halves the speed of `render`.
#![no_main]

use fagus::Document;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let markup = fagus_fuzz::text(data).into_owned();
    let document = if markup.starts_with("<?xml") {
        Document::xhtml(markup)
    } else {
        Document::html(markup)
    };
    fagus_fuzz::check(&document);
});
