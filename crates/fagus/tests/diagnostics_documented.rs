//! Every diagnostics code the crates emit is listed in `docs/troubleshooting.md`, and every code
//! listed there is one the crates emit: the codes are part of the public API (ADR 5), so a new one
//! needs its line in the documentation and a removed one must not stay in it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Endings of file names the sources mention, such as `"logo.png"`, which have the shape of a
/// code but are none.
const FILE_ENDINGS: &[&str] = &[
    "avif", "bmp", "css", "csv", "der", "gif", "html", "jpeg", "jpg", "js", "json", "md", "otf", "p12",
    "pdf", "pem", "png", "svg", "tif", "tiff", "ttf", "txt", "webp", "woff", "woff2", "xml",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "tests" {
                sources(&path, files);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
        {
            files.push(path);
        }
    }
}

/// Whether `text` has the shape of a code: `category.name`, lowercase, the name with hyphens.
fn is_code(text: &str) -> bool {
    let Some((category, name)) = text.split_once('.') else {
        return false;
    };
    !category.is_empty()
        && category.bytes().all(|b| b.is_ascii_lowercase())
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !FILE_ENDINGS.contains(&name)
}

/// The string literals with the shape of a code in the sources of the crates that emit
/// diagnostics, outside their tests.
fn emitted() -> BTreeSet<String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root().join("crates")).unwrap() {
        let krate = entry.unwrap().path();
        // The test kit reads and writes files of the test suites and emits no diagnostics.
        if krate.file_name().unwrap() != "fagus-testkit" {
            sources(&krate.join("src"), &mut files);
        }
    }
    let mut codes = BTreeSet::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let text = text.split("#[cfg(test)]").next().unwrap();
        for (start, _) in text.match_indices('"') {
            let rest = &text[start + 1..];
            if let Some(end) = rest.find('"') {
                let literal = &rest[..end];
                if is_code(literal) {
                    codes.insert(literal.to_owned());
                }
            }
        }
    }
    codes
}

/// The codes in the table of the section *Read diagnostics*.
fn documented() -> BTreeSet<String> {
    let text = fs::read_to_string(root().join("docs/troubleshooting.md")).unwrap();
    let section = text
        .split("\n## Read diagnostics\n")
        .nth(1)
        .expect("docs/troubleshooting.md has the section Read diagnostics");
    let section = section.split("\n## ").next().unwrap();
    section
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|line| line.split_once('`'))
        .map(|(code, _)| code.to_owned())
        .filter(|code| is_code(code))
        .collect()
}

#[test]
fn every_diagnostics_code_is_documented_and_every_documented_code_emitted() {
    let emitted = emitted();
    let documented = documented();
    assert!(emitted.contains("css.invalid-value"), "found {emitted:?}");
    let missing: Vec<_> = emitted.difference(&documented).collect();
    let stale: Vec<_> = documented.difference(&emitted).collect();
    assert!(
        missing.is_empty(),
        "codes missing in docs/troubleshooting.md#read-diagnostics: {missing:?}"
    );
    assert!(
        stale.is_empty(),
        "codes in docs/troubleshooting.md#read-diagnostics that no crate emits: {stale:?}"
    );
}

#[test]
fn file_names_are_not_codes() {
    assert!(is_code("pdf.layer-shown"));
    assert!(is_code("limit.memory"));
    assert!(!is_code("logo.png"));
    assert!(!is_code("rechnung.pdf"));
    assert!(!is_code("Customer.name"));
    assert!(!is_code("a.b.c"));
}
