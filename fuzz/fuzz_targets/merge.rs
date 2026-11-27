//! Merges arbitrary PDF data (its cross-reference table or stream, objects, page tree, encryption
//! and content streams): every page of it, then its second page and its first again after it.
#![no_main]

use fagus::{Error, Merge};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // As a PDF, whatever its first bytes are (seeds have the header, and their offsets stay).
    let mut pdf = Vec::with_capacity(data.len() + 9);
    if !data.starts_with(b"%PDF-") {
        pdf.extend_from_slice(b"%PDF-1.7\n");
    }
    pdf.extend_from_slice(data);
    let pdf = std::sync::Arc::new(pdf);
    let merge = Merge::new()
        .add(std::sync::Arc::clone(&pdf))
        .add_pages(pdf, "2,1".parse().expect("pages"))
        .limits(fagus_fuzz::LIMITS);
    // Merging may fail for a reason (a PDF that cannot be read, a page it lacks, a budget that
    // ran out), but never with an internal error, which is a bug.
    if let Err(Error::Internal(message)) = merge.write() {
        panic!("internal error: {message}");
    }
});
