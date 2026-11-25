//! What the fuzz targets share: an engine with the fonts of the tests, a check that rendering
//! did not end in an internal error, and one that the layout holds its invariants.
//!
//! libFuzzer's panic hook aborts the process on a panic, even one the engine would catch, so a
//! panic anywhere in the engine is reported as a crash. Only the panics of the CSS parser that
//! `fagus_css::guarded` expects are not.

use std::sync::OnceLock;

use std::time::Duration;

use fagus::{Date, Document, Engine, Error, Limits};

/// The memory budget and time limit of each document, below the limits `run.sh` gives libFuzzer
/// (`-rss_limit_mb=4096`, `-timeout=60`): an input that needs more ends as the engine ends such
/// documents, with `Error::MemoryBudget` or `Error::TimeLimit`. What still takes too long or too
/// much is where the engine does not look at its budget.
pub const LIMITS: Limits = Limits::new()
    .memory(Some(1 << 30))
    .time(Some(Duration::from_secs(20)));

/// The engine the targets render with, made once: the fonts of `tests/fonts` and those built
/// into Fagus, never the system's, so that runs do not depend on the machine.
pub fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut engine = Engine::new();
        engine.set_limits(LIMITS);
        engine
            .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fonts"))
            .expect("the fonts of the tests");
        engine
    })
}

/// A fixed creation date, so that the same input writes the same PDF.
pub fn date() -> Date {
    Date::from_unix(1_793_620_800)
}

/// Renders `document`. Rendering may fail for a reason (a font or PDF error, a budget that ran
/// out), but never with an internal error, which is a bug.
pub fn render(document: &Document) {
    match engine().render(&document.clone().created(date())) {
        Err(Error::Internal(message)) => panic!("internal error: {message}"),
        Ok(_) | Err(_) => {}
    }
}

/// Renders `document` twice and checks what holds for the layout of every document
/// (`fagus_invariants`): the first invariant that does not hold is a panic with where it broke.
pub fn check(document: &Document) {
    if let Err(violation) = fagus_invariants::check(engine(), document) {
        panic!("{violation}");
    }
}

/// The text of fuzz input, with invalid UTF-8 replaced.
pub fn text(data: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(data)
}
