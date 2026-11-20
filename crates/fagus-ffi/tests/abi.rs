//! The C interface, called as C calls it: through raw pointers, from Rust and from a C program
//! built against `include/fagus.h`.
#![allow(unsafe_code, reason = "calls the C interface")]

use std::ffi::{CStr, c_char};
use std::path::{Path, PathBuf};
use std::ptr;

use fagus_ffi::abi::{
    FagusEngine, FagusResult, fagus_abi, fagus_engine_free, fagus_engine_new, fagus_protocol, fagus_render,
    fagus_result_free, fagus_result_header, fagus_result_ok, fagus_result_pdf, fagus_string_free,
    fagus_version,
};
use serde_json::{Value, json};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// An engine with `options`, or the message of its failure.
fn engine(options: &str) -> Result<*mut FagusEngine, String> {
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: the options are `options.len()` bytes; `error` may be written.
    let engine = unsafe { fagus_engine_new(options.as_ptr().cast(), options.len(), &raw mut error) };
    if engine.is_null() {
        assert!(!error.is_null());
        // SAFETY: a message of the library, given back once.
        let message = unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned();
        unsafe { fagus_string_free(error) };
        return Err(message);
    }
    assert!(error.is_null());
    Ok(engine)
}

/// What the library answers to a request: whether it is a PDF, its header and the PDF.
fn render(engine: *const FagusEngine, header: &Value, blobs: &[&[u8]]) -> (bool, Value, Vec<u8>) {
    let mut header = header.clone();
    header["blobs"] = blobs.iter().map(|blob| blob.len()).collect();
    let request = header.to_string();
    let data = blobs.concat();
    // SAFETY: the request and the data are as long as their lengths say.
    let result = unsafe {
        fagus_render(
            engine,
            request.as_ptr().cast(),
            request.len(),
            data.as_ptr().cast(),
            data.len(),
        )
    };
    answer(result)
}

/// Reads an answer and gives it back.
fn answer(result: *mut FagusResult) -> (bool, Value, Vec<u8>) {
    assert!(!result.is_null());
    let (mut header_len, mut pdf_len) = (0, 0);
    // SAFETY: a live answer of `fagus_render`, read before it is given back.
    unsafe {
        let ok = fagus_result_ok(result) == 1;
        let header = fagus_result_header(result, &raw mut header_len);
        let text = CStr::from_ptr(header).to_str().expect("UTF-8");
        assert_eq!(text.len(), header_len);
        let header: Value = serde_json::from_str(text).expect("JSON");
        let pdf = fagus_result_pdf(result, &raw mut pdf_len);
        let pdf = if pdf.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(pdf, pdf_len).to_vec()
        };
        assert_eq!(pdf.is_empty(), !ok);
        fagus_result_free(result);
        (ok, header, pdf)
    }
}

#[test]
fn says_its_versions() {
    // SAFETY: a string of the library that lives as long as it.
    let version = unsafe { CStr::from_ptr(fagus_version()) };
    assert_eq!(version.to_str(), Ok(env!("CARGO_PKG_VERSION")));
    assert_eq!(fagus_abi(), 0);
    assert_eq!(fagus_protocol(), 1);
}

#[test]
fn renders_requests_of_the_worker_protocol() {
    let fonts = repository().join("tests/fonts");
    let engine = engine(&json!({ "fontDirs": [fonts] }).to_string()).expect("engine");
    let (ok, header, pdf) = render(
        engine,
        &json!({
            "id": 7,
            "html": 0,
            "stylesheets": [{ "name": "app.css", "blob": 1 }],
            "created": "2026-11-02T12:00:00Z",
            "layout": true,
        }),
        &[b"<h1 id=title>Rechnung</h1>", b"h1 { colr: red }"],
    );
    assert!(ok, "{header}");
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(header["id"], 7);
    assert_eq!(header["pages"], 1);
    assert_eq!(header["encrypted"], false);
    assert_eq!(header["diagnostics"][0]["code"], "css.unknown-property");
    assert_eq!(header["diagnostics"][0]["source"], "app.css");
    assert_eq!(header["layout"]["elements"][0]["id"], "title");
    assert!(header["timings"]["layout"].is_number());
    // The same bytes as the engine itself makes of the document.
    let mut direct = fagus::Engine::new();
    direct.add_font_dir(&fonts).expect("fonts");
    let expected = direct
        .render(
            &fagus::Document::html("<h1 id=title>Rechnung</h1>")
                .stylesheet("app.css", "h1 { colr: red }")
                .created(fagus::Date::new(2026, 11, 2, 12, 0, 0))
                .report_layout(true),
        )
        .expect("rendered");
    assert_eq!(pdf, expected.pdf);
    // SAFETY: the engine of `fagus_engine_new`, freed once.
    unsafe { fagus_engine_free(engine) };
}

#[test]
fn reads_files_from_the_base_directory() {
    let engine = engine("").expect("engine");
    let base = repository().join("tests/fonts");
    let html: &[u8] = b"<style>@font-face { font-family: Doc; src: url(Inter-Bold.ttf) } p { font-family: Doc }</style><p>Fett</p>";
    let (ok, header, _) = render(engine, &json!({ "id": 1, "html": 0, "base": base }), &[html]);
    assert!(ok, "{header}");
    assert_eq!(header["diagnostics"], json!([]));
    // Without a directory, the font is not found.
    let (ok, header, _) = render(engine, &json!({ "id": 2, "html": 0 }), &[html]);
    assert!(ok, "{header}");
    assert_ne!(header["diagnostics"], json!([]));
    // SAFETY: the engine of `fagus_engine_new`, freed once.
    unsafe { fagus_engine_free(engine) };
}

#[test]
fn answers_what_it_cannot_render_with_errors() {
    let engine = engine("").expect("engine");
    let message = |header: &Value, blobs: &[&[u8]]| {
        let (ok, header, _) = render(engine, header, blobs);
        assert!(!ok);
        header["message"].as_str().expect("message").to_owned()
    };
    assert_eq!(
        message(&json!({ "id": 1, "html": 0, "ask": true }), &[b"<p>x</p>"]),
        "`ask` needs a caller that answers questions, and there is none"
    );
    assert_eq!(
        message(&json!({ "id": 2, "html": 3 }), &[b"<p>x</p>"]),
        "data block 3 is missing"
    );
    assert_eq!(
        message(
            &json!({ "id": 3, "html": 0, "conformance": "9z" }),
            &[b"<p>x</p>"]
        ),
        "unknown PDF/A level `9z`"
    );
    // PDF/UA refuses a document without a title; the diagnostics found before come along.
    let (ok, header, _) = render(
        engine,
        &json!({ "id": 4, "html": 0, "accessible": true }),
        &[b"<style>p { colr: red }</style><p>Brief</p>"],
    );
    assert!(!ok);
    assert_eq!(header["id"], 4);
    assert_eq!(header["diagnostics"][0]["code"], "css.unknown-property");
    // A request that is not JSON, data that does not fit the blocks, null pointers.
    let request = b"{ not json";
    // SAFETY: pointers with their lengths, and null ones.
    let (ok, header, _) =
        answer(unsafe { fagus_render(engine, request.as_ptr().cast(), request.len(), ptr::null(), 0) });
    assert!(!ok);
    assert!(
        header["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("invalid request"))
    );
    let request = br#"{ "id": 5, "html": 0, "blobs": [8] }"#;
    let (ok, header, _) =
        answer(unsafe { fagus_render(engine, request.as_ptr().cast(), request.len(), c"<p>".as_ptr(), 3) });
    assert!(!ok);
    assert_eq!(
        header["message"],
        "invalid request: the data blocks are longer than the data"
    );
    let (ok, header, _) = answer(unsafe {
        fagus_render(
            ptr::null(),
            request.as_ptr().cast(),
            request.len(),
            ptr::null(),
            0,
        )
    });
    assert!(!ok);
    assert_eq!(header["message"], "the engine is a null pointer");
    let (ok, header, _) = answer(unsafe { fagus_render(engine, ptr::null(), 10, ptr::null(), 0) });
    assert!(!ok);
    assert_eq!(header["message"], "the request or its data is a null pointer");
    // Answers and engines that are null are ignored.
    unsafe {
        assert_eq!(fagus_result_ok(ptr::null()), 0);
        let mut len = 1;
        assert!(fagus_result_pdf(ptr::null(), &raw mut len).is_null());
        assert_eq!(len, 0);
        assert!(fagus_result_header(ptr::null(), ptr::null_mut()).is_null());
        fagus_result_free(ptr::null_mut());
        fagus_engine_free(ptr::null_mut());
        fagus_string_free(ptr::null_mut());
        fagus_engine_free(engine);
    }
}

#[test]
fn refuses_options_it_does_not_know() {
    assert!(
        engine(r#"{ "fontDir": "fonts" }"#).is_err_and(|message| message.contains("unknown field `fontDir`"))
    );
    assert!(engine("[").is_err_and(|message| message.starts_with("invalid options")));
    // Without a place for the message, only null.
    let options = "{ \"systemFont\": true }";
    // SAFETY: the options with their length; no place for a message.
    let engine = unsafe { fagus_engine_new(options.as_ptr().cast(), options.len(), ptr::null_mut()) };
    assert!(engine.is_null());
}

#[test]
fn renders_on_several_threads_with_one_engine() {
    /// The engine, shared with the threads as C would share it.
    #[derive(Clone, Copy)]
    struct Shared(*const FagusEngine);
    // SAFETY: an engine may render on several threads at once (fagus.h).
    unsafe impl Send for Shared {}

    let engine = Shared(engine("").expect("engine"));
    let threads: Vec<_> = (0..4)
        .map(|index| {
            std::thread::spawn(move || {
                let engine = engine;
                let html = format!("<p>Dokument {index}</p>");
                render(
                    engine.0,
                    &json!({ "id": index, "html": 0, "created": "2026-11-02T12:00:00Z" }),
                    &[html.as_bytes()],
                )
            })
        })
        .collect();
    for (index, thread) in threads.into_iter().enumerate() {
        let (ok, header, pdf) = thread.join().expect("rendered");
        assert!(ok, "{header}");
        assert_eq!(header["id"], index);
        assert!(pdf.starts_with(b"%PDF-"));
    }
    // SAFETY: the engine of `fagus_engine_new`, freed once all threads are done.
    unsafe { fagus_engine_free(engine.0.cast_mut()) };
}

/// The functions that `text` declares or defines: the names after `fagus_` up to `(`, outside
/// comments.
fn functions(text: &str, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with(['*', '/']))
        .filter_map(|line| line.split_once(prefix))
        .filter_map(|(_, rest)| rest.split_once('(').map(|(name, _)| format!("fagus_{name}")))
        .filter(|name| {
            name.chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn the_header_declares_every_function() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let header = std::fs::read_to_string(crate_dir.join("include/fagus.h")).expect("header");
    let abi = std::fs::read_to_string(crate_dir.join("src/abi.rs")).expect("abi.rs");
    let declared = functions(&header, " *fagus_")
        .into_iter()
        .chain(functions(&header, " fagus_"))
        .collect::<std::collections::BTreeSet<_>>();
    let defined = functions(&abi, "extern \"C\" fn fagus_")
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(declared, defined);
    assert_eq!(defined.len(), 11);
}

/// A C program built against the header and linked with the shared library, where a C compiler
/// is at hand (not on Windows, where the library is a DLL with an import library).
#[test]
fn works_from_c() {
    if cfg!(windows)
        || std::process::Command::new("cc")
            .arg("--version")
            .output()
            .is_err()
    {
        eprintln!("no C compiler: skipped");
        return;
    }
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Cargo builds the shared library next to the test, in target/<profile>/deps.
    let libraries = std::env::current_exe()
        .expect("test")
        .parent()
        .expect("deps directory")
        .to_path_buf();
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fagus-ffi-c");
    std::fs::create_dir_all(&directory).expect("directory");
    let program = directory.join("render");
    let built = std::process::Command::new("cc")
        .args(["-std=c99", "-Wall", "-Werror", "-o"])
        .arg(&program)
        .arg(crate_dir.join("tests/render.c"))
        .arg("-I")
        .arg(crate_dir.join("include"))
        .arg("-L")
        .arg(&libraries)
        .arg(format!("-Wl,-rpath,{}", libraries.display()))
        .arg("-lfagus_ffi")
        .status()
        .expect("cc");
    assert!(built.success());
    let output = std::process::Command::new(&program)
        .arg(directory.join("out.pdf"))
        .output()
        .expect("ran");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pdf = std::fs::read(directory.join("out.pdf")).expect("PDF");
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1 page");
}
