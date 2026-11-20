//! The C library of Fagus, experimental: renders documents in the caller's process instead of a
//! `fagus worker` process. A document is the render request of the worker protocol (the JSON
//! header of `R` and its data blocks, `docs/protocol.md`), so that the options are the same and
//! live in one place; the answer is the header of `P` or `E` and the PDF. The functions are in
//! [`abi`] and declared in `include/fagus.h`; `docs/ffi.md` describes them, and ADR 8 why the
//! library is experimental and what it leaves out.
//!
//! Besides `memory.rs` of the command line tool, this is the only crate with unsafe code, all of
//! it in [`abi`]: the functions that C calls, which read the caller's memory and hand out memory
//! that the caller gives back. Everything else is here, safe.
#![deny(unsafe_code)]

use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

// The protocol code of the command line tool, as it is (the fuzz target includes it the same
// way), so that a request means the same in the worker and here.
#[path = "../../fagus-cli/src/date.rs"]
mod date;
#[path = "../../fagus-cli/src/layout_json.rs"]
mod layout_json;
#[path = "../../fagus-cli/src/request.rs"]
mod request;

pub mod abi;

/// The version of the C interface: 0 while it is experimental, which may change in any release.
pub const ABI: u32 = 0;

/// The options of an engine, as JSON: those of `fagus worker` (docs/cli.md), with the limits in
/// the units of the protocol.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Options {
    /// Directories with font files.
    #[serde(default)]
    font_dirs: Vec<PathBuf>,
    /// Whether the fonts installed on the system are used too.
    #[serde(default)]
    system_fonts: bool,
    /// The language of the texts of form controls, such as `de-DE`.
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    sans_serif: Option<String>,
    #[serde(default)]
    serif: Option<String>,
    #[serde(default)]
    monospace: Option<String>,
    /// The memory budget of a document in bytes, 0 for none (2 GiB if left out).
    #[serde(default)]
    memory_budget: Option<u64>,
    /// The time limit of a document in milliseconds, 0 for none (none if left out).
    #[serde(default)]
    time_limit: Option<f64>,
}

/// An engine with the options of `options` (JSON; empty for the defaults).
fn engine(options: &[u8]) -> Result<fagus::Engine, String> {
    let options: Options = if options.is_empty() {
        Options::default()
    } else {
        serde_json::from_slice(options).map_err(|error| format!("invalid options: {error}"))?
    };
    let mut engine = fagus::Engine::new();
    let mut limits = fagus::Limits::new();
    if let Some(bytes) = options.memory_budget {
        limits = limits.memory((bytes > 0).then_some(bytes));
    }
    if let Some(milliseconds) = options.time_limit {
        let limit = std::time::Duration::try_from_secs_f64(milliseconds / 1000.0)
            .map_err(|_| format!("invalid time limit `{milliseconds}`"))?;
        limits = limits.time((!limit.is_zero()).then_some(limit));
    }
    engine.set_limits(limits);
    for directory in &options.font_dirs {
        engine
            .add_font_dir(directory)
            .map_err(|error| format!("reading fonts in {}: {error}", directory.display()))?;
    }
    if options.system_fonts {
        engine.add_system_fonts();
    }
    engine.set_locale(options.locale);
    if options.sans_serif.is_some() || options.serif.is_some() || options.monospace.is_some() {
        engine.set_generic_families(options.sans_serif, options.serif, options.monospace);
    }
    Ok(engine)
}

/// How many threads compress the PDF while it is written, unless `RAYON_NUM_THREADS` says
/// otherwise: two, as in the command line tool. More gain nothing, and every process that loads
/// the library (each PHP-FPM worker) would start one per processor.
#[cfg(feature = "parallel")]
const PDF_THREADS: usize = 2;

/// Starts the threads that write PDFs, once, with the first document rather than with the first
/// engine: a process that makes an engine and then forks (PHP-FPM preloading) would leave its
/// children with a pool whose threads they do not have.
fn start_threads() {
    #[cfg(feature = "parallel")]
    {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            if std::env::var_os("RAYON_NUM_THREADS").is_none() {
                // Fails only where the pool exists already, which then serves as it is.
                let _ = rayon::ThreadPoolBuilder::new()
                    .num_threads(PDF_THREADS)
                    .build_global();
            }
        });
    }
}

/// The answer to a render request: whether it is a PDF, the JSON header of `P` or `E`, and the
/// PDF.
pub(crate) struct Rendered {
    ok: bool,
    header: CString,
    pdf: Vec<u8>,
}

impl Rendered {
    fn error(message: &str) -> Self {
        Self::answer(
            false,
            &json!({ "id": null, "message": message, "diagnostics": [] }),
            Vec::new(),
        )
    }

    fn answer(ok: bool, header: &Value, pdf: Vec<u8>) -> Self {
        // JSON escapes the control characters of strings, so it holds no NUL.
        let header = CString::new(header.to_string()).unwrap_or_default();
        Self { ok, header, pdf }
    }
}

/// Renders the request of `header` (the JSON header of `R`) with the data blocks that `data`
/// holds one after another, as long as the header's `blobs` say.
fn render(engine: &fagus::Engine, header: &[u8], data: &[u8]) -> Rendered {
    start_threads();
    let header: Value = match serde_json::from_slice(header) {
        Ok(header) => header,
        Err(error) => return Rendered::error(&format!("invalid request: {error}")),
    };
    let blobs = match split(&header, data) {
        Ok(blobs) => blobs,
        Err(message) => return Rendered::error(&message),
    };
    match request::render(engine, header, blobs, None) {
        request::Answer::Pdf { header, pdf } => Rendered::answer(true, &header, pdf),
        request::Answer::Error { header } => Rendered::answer(false, &header, Vec::new()),
    }
}

/// The data blocks of a request: `data` cut into the lengths of the header's `blobs`, which
/// must add up to all of it.
fn split(header: &Value, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut blobs = Vec::new();
    let mut offset = 0usize;
    for length in header
        .get("blobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let length = length
            .as_u64()
            .and_then(|length| usize::try_from(length).ok())
            .ok_or("invalid request: a data block without a length")?;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or("invalid request: the data blocks are longer than the data")?;
        blobs.push(data[offset..end].to_vec());
        offset = end;
    }
    if offset != data.len() {
        return Err(format!(
            "invalid request: {} bytes of data after the data blocks",
            data.len() - offset
        ));
    }
    Ok(blobs)
}

/// Runs `work`, and `fallback` with the message of a panic should it panic: a panic must not
/// unwind into C, where it would end the process.
fn guard<T>(work: impl FnOnce() -> T, fallback: impl FnOnce(&str) -> T) -> T {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic".to_owned());
        fallback(&format!("internal error: {message}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_the_data_into_blocks() {
        let header = json!({ "blobs": [2, 0, 3] });
        assert_eq!(
            split(&header, b"abcde"),
            Ok(vec![b"ab".to_vec(), Vec::new(), b"cde".to_vec()])
        );
        assert_eq!(split(&json!({}), b""), Ok(Vec::new()));
        assert!(split(&header, b"abcd").is_err());
        assert!(split(&header, b"abcdef").is_err());
        assert!(split(&json!({ "blobs": [-1] }), b"").is_err());
    }

    #[test]
    fn takes_the_options_of_the_worker() {
        assert!(engine(b"").is_ok());
        assert!(engine(br#"{ "locale": "de-DE", "memoryBudget": 0, "timeLimit": 30000 }"#).is_ok());
        assert!(
            engine(br#"{ "fontDir": "fonts" }"#)
                .is_err_and(|message| message.contains("unknown field `fontDir`"))
        );
        assert!(
            engine(br#"{ "fontDirs": ["/no/such/directory"] }"#)
                .is_err_and(|message| message.starts_with("reading fonts in /no/such/directory"))
        );
        assert!(engine(br#"{ "timeLimit": -1 }"#).is_err());
    }

    #[test]
    fn turns_panics_into_errors() {
        let answer = guard(
            || -> Result<(), String> { panic!("a bug") },
            |message| Err(message.to_owned()),
        );
        assert_eq!(answer, Err("internal error: a bug".to_owned()));
    }
}
