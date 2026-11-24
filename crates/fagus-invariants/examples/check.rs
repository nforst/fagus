//! Checks the invariants on documents: `cargo run -p fagus-invariants --example check FILE...`
//! (`.xhtml` files as XHTML). Prints what was checked and every violation; fails if there is one.

use std::path::Path;
use std::process::ExitCode;

use fagus::{Document, Engine};

/// An engine with the fonts of the tests, as the fuzz targets have.
fn engine() -> Engine {
    let mut engine = Engine::new();
    let fonts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts");
    engine.add_font_dir(fonts).expect("the fonts of the tests");
    engine
}

fn main() -> ExitCode {
    let engine = engine();
    let mut failed = false;
    for file in std::env::args().skip(1) {
        let markup = match std::fs::read(&file) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                eprintln!("{file}: {error}");
                failed = true;
                continue;
            }
        };
        let path = Path::new(&file);
        let document = if path.extension().is_some_and(|extension| extension == "xhtml") {
            Document::xhtml(markup)
        } else {
            Document::html(markup)
        };
        let document = match path.parent() {
            Some(directory) if !directory.as_os_str().is_empty() => document.base_directory(directory),
            _ => document,
        };
        let (stats, violations) = fagus_invariants::examine(&engine, Some(&self::engine()), &document);
        println!("{file}: {stats:?}");
        for violation in &violations {
            println!("{violation}");
        }
        failed |= !violations.is_empty();
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
