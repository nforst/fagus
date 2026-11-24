//! Every document of `tests/corpus` holds the invariants, rendered by a warm engine and by a
//! fresh one.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use fagus::{Document, Engine};

fn documents(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("a directory of the corpus") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            documents(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "html") {
            found.push(path);
        }
    }
}

fn engine(tests: &Path) -> Engine {
    let mut engine = Engine::new();
    engine
        .add_font_dir(tests.join("fonts"))
        .expect("the fonts of the tests");
    engine
}

#[test]
fn the_corpus_holds_the_invariants() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let mut found = Vec::new();
    documents(&tests.join("corpus"), &mut found);
    assert!(found.len() > 200, "{} documents", found.len());
    // The largest first: the longest documents take most of the time and start together.
    found.sort_by_cached_key(|path| {
        let size = std::fs::metadata(path).expect("a file").len();
        (std::cmp::Reverse(size), path.clone())
    });
    let warm = engine(&tests);
    let next = AtomicUsize::new(0);
    let fragments = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    // Side by side: each document is rendered three times, which takes minutes one by one in a
    // build without optimizations.
    let threads = std::thread::available_parallelism().map_or(2, |threads| threads.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(path) = found.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let html = std::fs::read_to_string(path).expect("a document");
                    let document = Document::html(html)
                        .base_directory(path.parent().expect("a directory"))
                        .root(&tests);
                    let name = path.strip_prefix(&tests).expect("in tests").display();
                    let (stats, violations) =
                        fagus_invariants::examine(&warm, Some(&engine(&tests)), &document);
                    fragments.fetch_add(stats.fragments, Ordering::Relaxed);
                    let mut failures = failures.lock().expect("the failures");
                    if !stats.rendered || stats.over_budget {
                        failures.push(format!("{name}: not rendered ({stats:?})"));
                    }
                    failures.extend(violations.iter().map(|violation| format!("{name}: {violation}")));
                }
            });
        }
    });
    let mut failures = failures.into_inner().expect("the failures");
    failures.sort();
    assert!(fragments.into_inner() > 100_000, "fragments");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
