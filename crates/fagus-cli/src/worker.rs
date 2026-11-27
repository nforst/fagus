//! `fagus worker`: renders documents for a caller over standard input and output, in the frames
//! of `docs/protocol.md`. One process serves many documents with its fonts and caches warm.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::request::{self, Answer, Caller, Question};

/// The version of the protocol.
pub(crate) const PROTOCOL: u32 = 1;

/// The largest frame accepted.
const MAX_FRAME: usize = 1 << 30;

const HELLO: u8 = b'H';
const RENDER: u8 = b'R';
const MERGE: u8 = b'M';
const ASK: u8 = b'?';
const RESOURCE: u8 = b'r';
const ASK_MANY: u8 = b'Q';
const RESOURCES: u8 = b'q';
const PDF: u8 = b'P';
const ERROR: u8 = b'E';
const FONT: u8 = b'F';
const DONE: u8 = b'K';
const SIGN: u8 = b'S';
const SIGNED: u8 = b's';
const TIMESTAMP: u8 = b'T';
const TIMESTAMPED: u8 = b't';
const REVOCATION: u8 = b'V';
const REVOKED: u8 = b'v';

/// A message: its type, its JSON header and its data blocks.
#[derive(Debug)]
pub(crate) struct Frame {
    pub kind: u8,
    pub header: Value,
    pub blobs: Vec<Vec<u8>>,
}

/// Both ends of the connection to the caller.
pub(crate) struct Channel<R, W> {
    input: R,
    output: W,
}

impl<R: Read, W: Write> Channel<R, W> {
    pub(crate) fn new(input: R, output: W) -> Self {
        Self { input, output }
    }

    /// Reads a frame; `None` when the input has ended.
    pub(crate) fn read(&mut self) -> io::Result<Option<Frame>> {
        let mut length = [0; 4];
        match self.input.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        let length = u32::from_le_bytes(length) as usize;
        if !(5..=MAX_FRAME).contains(&length) {
            return Err(invalid(&format!("a frame of {length} bytes")));
        }
        // Read as the data comes, not into a buffer of the length the frame claims: a frame cut
        // short would otherwise take up to 1 GiB first.
        let mut body = Vec::new();
        (&mut self.input).take(length as u64).read_to_end(&mut body)?;
        if body.len() < length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "protocol error: a frame of {length} bytes ends after {}",
                    body.len()
                ),
            ));
        }
        let kind = body[0];
        let header_length = u32::from_le_bytes([body[1], body[2], body[3], body[4]]) as usize;
        let header_end = 5usize
            .checked_add(header_length)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| invalid("a header longer than its frame"))?;
        let header: Value = serde_json::from_slice(&body[5..header_end])
            .map_err(|error| invalid(&format!("a header that is not JSON: {error}")))?;
        let mut blobs = Vec::new();
        let mut offset = header_end;
        for length in header
            .get("blobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let length = length
                .as_u64()
                .and_then(|length| usize::try_from(length).ok())
                .ok_or_else(|| invalid("a data block without a length"))?;
            let end = offset
                .checked_add(length)
                .filter(|end| *end <= body.len())
                .ok_or_else(|| invalid("data blocks longer than their frame"))?;
            blobs.push(body[offset..end].to_vec());
            offset = end;
        }
        Ok(Some(Frame { kind, header, blobs }))
    }

    /// Writes a frame; the header gets the lengths of the data blocks.
    pub(crate) fn write(&mut self, kind: u8, mut header: Value, blobs: &[&[u8]]) -> io::Result<()> {
        if !blobs.is_empty()
            && let Value::Object(map) = &mut header
        {
            map.insert(
                "blobs".to_owned(),
                blobs.iter().map(|blob| Value::from(blob.len())).collect(),
            );
        }
        let header = serde_json::to_vec(&header).map_err(io::Error::other)?;
        let length = 1 + 4 + header.len() + blobs.iter().map(|blob| blob.len()).sum::<usize>();
        let length = u32::try_from(length).map_err(|_| invalid("a frame larger than 4 GiB"))?;
        self.output.write_all(&length.to_le_bytes())?;
        self.output.write_all(&[kind])?;
        self.output.write_all(&(header.len() as u32).to_le_bytes())?;
        self.output.write_all(&header)?;
        for blob in blobs {
            self.output.write_all(blob)?;
        }
        self.output.flush()
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("protocol error: {message}"))
}

/// The caller at the other end of the connection, which the engine asks while a document renders.
struct Connection<R, W> {
    channel: Arc<Mutex<Channel<R, W>>>,
}

impl<R: Read + Send, W: Write + Send> Caller for Connection<R, W> {
    fn resource(&self, id: u64, url: &str) -> Option<Vec<u8>> {
        let mut channel = self.channel.lock().ok()?;
        channel.write(ASK, json!({ "id": id, "url": url }), &[]).ok()?;
        let frame = channel.read().ok()??;
        if frame.kind != RESOURCE || frame.header.get("found") != Some(&Value::Bool(true)) {
            return None;
        }
        frame.blobs.into_iter().next()
    }

    /// Asks with `Q`: the answer says which of `urls` are found, and holds their data in that
    /// order. What an answer leaves out is not found.
    fn resources(&self, id: u64, urls: &[&str]) -> Vec<Option<Vec<u8>>> {
        let answer = || {
            let mut channel = self.channel.lock().ok()?;
            channel
                .write(ASK_MANY, json!({ "id": id, "urls": urls }), &[])
                .ok()?;
            channel.read().ok()?.filter(|frame| frame.kind == RESOURCES)
        };
        let Some(frame) = answer() else {
            return vec![None; urls.len()];
        };
        let mut found = frame
            .header
            .get("found")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        let mut blobs = frame.blobs.into_iter();
        urls.iter()
            .map(|_| {
                if found.next() == Some(&Value::Bool(true)) {
                    blobs.next()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Asks with `S`, `T` or `V`, and returns the data blocks of the answer.
    fn ask(&self, question: Question, header: Value, data: &[&[u8]]) -> Result<Vec<Vec<u8>>, String> {
        let (kind, answer) = match question {
            Question::Sign => (SIGN, SIGNED),
            Question::Timestamp => (TIMESTAMP, TIMESTAMPED),
            Question::Revocation => (REVOCATION, REVOKED),
        };
        let mut channel = self.channel.lock().map_err(|_| "poisoned lock".to_owned())?;
        channel
            .write(kind, header, data)
            .map_err(|error| error.to_string())?;
        let frame = channel
            .read()
            .map_err(|error| error.to_string())?
            .ok_or("the caller closed the connection")?;
        if frame.kind != answer {
            return Err(format!(
                "the caller answered `{}` instead of `{}`",
                char::from(frame.kind),
                char::from(answer)
            ));
        }
        if let Some(error) = frame.header.get("error").and_then(Value::as_str) {
            return Err(error.to_owned());
        }
        Ok(frame.blobs)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> io::Result<std::sync::MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| io::Error::other("poisoned lock"))
}

/// Serves render requests until the input ends.
pub(crate) fn serve<R, W>(mut engine: fagus::Engine, input: R, output: W) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let channel = Arc::new(Mutex::new(Channel::new(input, output)));
    let caller: Arc<dyn Caller> = Arc::new(Connection {
        channel: Arc::clone(&channel),
    });

    lock(&channel)?.write(
        HELLO,
        json!({
            "protocol": PROTOCOL,
            "version": env!("CARGO_PKG_VERSION"),
            "sandbox": crate::sandbox::status().name(),
        }),
        &[],
    )?;
    let trims = crate::memory::trims();
    loop {
        let Some(frame) = lock(&channel)?.read()? else {
            return Ok(());
        };
        match frame.kind {
            RENDER => {
                let answer = request::render(&engine, frame.header, frame.blobs, Some(&caller));
                let mut channel = lock(&channel)?;
                match answer {
                    Answer::Pdf { header, pdf } => channel.write(PDF, with_memory(header), &[&pdf])?,
                    Answer::Error { header } => channel.write(ERROR, with_memory(header), &[])?,
                }
                // Once the answer is out, so that the caller does not wait for it.
                drop(channel);
                if trims {
                    crate::memory::trim();
                }
            }
            MERGE => {
                let answer = crate::merge::answer(frame.header, frame.blobs, engine.limits());
                let mut channel = lock(&channel)?;
                match answer {
                    Answer::Pdf { header, pdf } => channel.write(PDF, with_memory(header), &[&pdf])?,
                    Answer::Error { header } => channel.write(ERROR, with_memory(header), &[])?,
                }
                drop(channel);
                if trims {
                    crate::memory::trim();
                }
            }
            FONT => {
                let reply = match frame.blobs.into_iter().next() {
                    Some(data) => engine.add_font_data(data).map_err(|error| error.to_string()),
                    None => Err("the font has no data".to_owned()),
                };
                let mut channel = lock(&channel)?;
                match reply {
                    Ok(faces) => channel.write(DONE, json!({ "faces": faces }), &[])?,
                    Err(message) => {
                        channel.write(ERROR, json!({ "message": message, "diagnostics": [] }), &[])?;
                    }
                }
            }
            other => {
                lock(&channel)?.write(
                    ERROR,
                    json!({ "message": format!("unknown message type `{}`", char::from(other)), "diagnostics": [] }),
                    &[],
                )?;
            }
        }
    }
}

/// The header of an answer to a render request, with the resident memory of the worker after
/// the document: `rss` now and `maxRss` at its peak, in bytes, where the system tells them.
fn with_memory(mut header: Value) -> Value {
    let resident = crate::memory::resident();
    if let Some(fields) = header.as_object_mut() {
        for (name, bytes) in [("rss", resident.now), ("maxRss", resident.peak)] {
            if let Some(bytes) = bytes {
                fields.insert(name.to_owned(), bytes.into());
            }
        }
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes frames as the caller would.
    fn frame(kind: u8, header: &Value, blobs: &[&[u8]]) -> Vec<u8> {
        let mut channel = Channel::new(io::empty(), Vec::new());
        channel.write(kind, header.clone(), blobs).expect("written");
        channel.output
    }

    /// Splits the worker's output into frames.
    fn frames(output: &[u8]) -> Vec<Frame> {
        let mut channel = Channel::new(output, io::sink());
        std::iter::from_fn(|| channel.read().expect("a frame")).collect()
    }

    fn engine() -> fagus::Engine {
        let mut engine = fagus::Engine::new();
        engine
            .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
            .expect("fonts");
        engine
    }

    /// A writer the test can read after the worker has finished.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn renders_and_asks_for_resources() {
        let html = br"<style>@font-face { font-family: Doc; src: url(font.ttf); } body { font-family: Doc }</style><p>Hallo</p>";
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fonts/Inter-Regular.ttf"
        ))
        .expect("font");
        // The request, then the answer to the question the worker will ask.
        let mut input = frame(
            RENDER,
            &json!({ "id": 7, "html": 0, "ask": true, "created": "2026-11-02T12:00:00Z" }),
            &[html],
        );
        input.extend(frame(RESOURCE, &json!({ "found": true }), &[&font]));
        input.extend(frame(FONT, &json!({}), &[b"no font"]));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "H?PE");
        assert_eq!(frames[0].header["protocol"], PROTOCOL);
        assert!(frames[0].header["sandbox"].is_string());
        assert_eq!(frames[1].header, json!({ "id": 7, "url": "font.ttf" }));
        assert_eq!(frames[2].header["id"], 7);
        assert_eq!(frames[2].header["pages"], 1);
        assert!(frames[2].blobs[0].starts_with(b"%PDF-"));
        // The worker's memory, where the system tells it; FreeBSD knows it only once the
        // process has run for a clock tick (memory.rs).
        let max_rss = frames[2].header.get("maxRss");
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(max_rss.is_some());
        }
        if let Some(bytes) = max_rss {
            assert!(bytes.as_u64().is_some_and(|bytes| bytes > 1 << 20), "{bytes}");
        }
        assert_eq!(frames[2].header.get("rss").is_some(), cfg!(target_os = "linux"));
        assert!(
            frames[3].header["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
    }

    /// The frames the worker writes for a document with two style sheets, one of them with a
    /// font, which the caller answers with `answers`.
    fn asked_for_many(ask_many: bool, answers: &[Vec<u8>]) -> Vec<Frame> {
        let html = b"<link rel=stylesheet href=a.css><link rel=stylesheet href=b.css><p>Hallo</p>";
        let mut header = json!({ "id": 7, "html": 0, "ask": true, "created": "2026-11-02T12:00:00Z" });
        if ask_many {
            header["askMany"] = Value::Bool(true);
        }
        let mut input = frame(RENDER, &header, &[html]);
        for answer in answers {
            input.extend(answer);
        }
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        frames(&output)
    }

    const CSS: &[u8] = b"@font-face { font-family: Doc; src: url(font.ttf) } body { font-family: Doc }";

    /// The codes of the diagnostics of an answer.
    fn codes(frame: &Frame) -> Vec<&str> {
        frame.header["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .filter_map(|diagnostic| diagnostic["code"].as_str())
            .collect()
    }

    fn kinds(frames: &[Frame]) -> String {
        frames.iter().map(|frame| char::from(frame.kind)).collect()
    }

    #[test]
    fn asks_for_many_resources_at_once() {
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fonts/Inter-Regular.ttf"
        ))
        .expect("font");
        // The style sheets together, then the font of the one that was found.
        let frames = asked_for_many(
            true,
            &[
                frame(RESOURCES, &json!({ "found": [true, false] }), &[CSS]),
                frame(RESOURCES, &json!({ "found": [true] }), &[&font]),
            ],
        );
        assert_eq!(kinds(&frames), "HQQP");
        assert_eq!(frames[1].header, json!({ "id": 7, "urls": ["a.css", "b.css"] }));
        assert_eq!(frames[2].header, json!({ "id": 7, "urls": ["font.ttf"] }));
        assert_eq!(codes(&frames[3]), ["resource.not-loaded"]);
        let together = frames[3].blobs[0].clone();

        // A caller that does not say `askMany` is asked for one after the other, and gets the
        // same PDF.
        let frames = asked_for_many(
            false,
            &[
                frame(RESOURCE, &json!({ "found": true }), &[CSS]),
                frame(RESOURCE, &json!({ "found": false }), &[]),
                frame(RESOURCE, &json!({ "found": true }), &[&font]),
            ],
        );
        assert_eq!(kinds(&frames), "H???P");
        assert_eq!(frames[1].header, json!({ "id": 7, "url": "a.css" }));
        assert_eq!(frames[2].header, json!({ "id": 7, "url": "b.css" }));
        assert_eq!(frames[3].header, json!({ "id": 7, "url": "font.ttf" }));
        assert_eq!(codes(&frames[4]), ["resource.not-loaded"]);
        assert_eq!(frames[4].blobs[0], together);
    }

    #[test]
    fn takes_what_an_answer_for_many_resources_leaves_out_as_not_found() {
        // An answer of another type: nothing is found, and nothing asked for again.
        let frames = asked_for_many(true, &[frame(RESOURCE, &json!({ "found": true }), &[CSS])]);
        assert_eq!(kinds(&frames), "HQP");
        assert_eq!(codes(&frames[2]), ["resource.not-loaded", "resource.not-loaded"]);
        // Fewer answers than URLs, and no list at all.
        for found in [json!({ "found": [false] }), json!({}), json!({ "found": true })] {
            let frames = asked_for_many(true, &[frame(RESOURCES, &found, &[])]);
            assert_eq!(kinds(&frames), "HQP", "{found}");
            assert_eq!(
                codes(&frames[2]),
                ["resource.not-loaded", "resource.not-loaded"],
                "{found}"
            );
        }
        // Found, but without its data.
        let frames = asked_for_many(true, &[frame(RESOURCES, &json!({ "found": [true, true] }), &[])]);
        assert_eq!(kinds(&frames), "HQP");
        assert_eq!(codes(&frames[2]), ["resource.not-loaded", "resource.not-loaded"]);
        // The second of two, with the data of one: the font it names is asked for next.
        let frames = asked_for_many(
            true,
            &[
                frame(RESOURCES, &json!({ "found": [false, true] }), &[CSS]),
                frame(RESOURCES, &json!({ "found": [] }), &[]),
            ],
        );
        assert_eq!(kinds(&frames), "HQQP");
        assert_eq!(frames[2].header, json!({ "id": 7, "urls": ["font.ttf"] }));
        assert_eq!(codes(&frames[3]), ["resource.not-loaded", "resource.not-loaded"]);
    }

    #[test]
    fn reports_the_layout_only_when_asked() {
        let html: &[u8] = b"<p id=a>x</p>";
        let mut input = frame(RENDER, &json!({ "id": 1, "html": 0 }), &[html]);
        input.extend(frame(
            RENDER,
            &json!({ "id": 2, "html": 0, "layout": true }),
            &[html],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        assert_eq!(frames[1].header.get("layout"), None);
        let layout = &frames[2].header["layout"];
        assert_eq!(layout["pages"].as_array().map(Vec::len), Some(1));
        assert_eq!(layout["elements"][0]["id"], "a");
        assert_eq!(layout["elements"][0]["tag"], "p");
        assert_eq!(layout["elements"][0]["boxes"][0]["page"], 1);
    }

    #[test]
    fn ends_documents_past_their_limits_and_goes_on() {
        let html: &[u8] = b"<p>Hallo</p>";
        let long = "<p>Hallo Welt</p>".repeat(20_000);
        // A budget of a kilobyte, a time limit of a millisecond, then the same without limits.
        let mut input = frame(
            RENDER,
            &json!({ "id": 1, "html": 0, "memoryBudget": 1024 }),
            &[html],
        );
        input.extend(frame(
            RENDER,
            &json!({ "id": 2, "html": 0, "timeLimit": 1 }),
            &[long.as_bytes()],
        ));
        input.extend(frame(
            RENDER,
            &json!({ "id": 3, "html": 0, "memoryBudget": 0, "timeLimit": 0 }),
            &[html],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HEEP");
        assert_eq!(frames[1].header["id"], 1);
        assert_eq!(frames[1].header["diagnostics"][0]["code"], "limit.memory");
        assert_eq!(frames[2].header["id"], 2);
        assert_eq!(frames[2].header["diagnostics"][0]["code"], "limit.time");
        assert_eq!(frames[3].header["id"], 3);
    }

    #[test]
    fn merges_pdfs_and_says_what_it_cannot_merge() {
        // Two PDFs to merge, rendered first by the same worker.
        let mut input = frame(
            RENDER,
            &json!({ "id": 1, "html": 0, "created": "2026-11-02T12:00:00Z" }),
            &[b"<p style='break-after: page'>Eins</p><p>Zwei</p>"],
        );
        input.extend(frame(
            RENDER,
            &json!({ "id": 2, "html": 0, "created": "2026-11-02T12:00:00Z" }),
            &[b"<p>Drei</p>"],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let rendered = frames(&output.0.lock().expect("lock"));
        let (first, second) = (&rendered[1].blobs[0], &rendered[2].blobs[0]);

        let mut input = frame(
            MERGE,
            &json!({
                "id": 3,
                "inputs": [{ "blob": 1, "pages": "1" }, { "blob": 0, "pages": null }, { "blob": 1, "pages": "2-" }],
                "title": "Sammeldruck",
                "authors": ["Buchhaltung"],
                "language": "de-DE",
                "created": "2026-11-27T12:00:00Z",
            }),
            &[second, first],
        );
        // The same without a date, twice: the same bytes.
        for id in [4, 5] {
            input.extend(frame(
                MERGE,
                &json!({ "id": id, "inputs": [{ "blob": 0 }, { "blob": 1 }] }),
                &[first, second],
            ));
        }
        for header in [
            json!({ "id": 6, "inputs": [] }),
            json!({ "id": 7, "inputs": [{ "blob": 0 }, { "blob": 1 }] }),
            json!({ "id": 8, "inputs": [{ "blob": 0, "pages": "3" }] }),
            json!({ "id": 9, "inputs": [{ "blob": 0, "pages": "x" }] }),
            json!({ "id": 10, "inputs": [{ "blob": 2 }] }),
            json!({ "id": 11, "inputs": [{ "blob": 0 }], "created": "gestern" }),
            json!({ "id": 12, "inputs": [{ "blob": 0 }], "memoryBudget": 1024 }),
            json!({ "id": 13 }),
        ] {
            input.extend(frame(MERGE, &header, &[first, b"kein PDF"]));
        }
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let frames = frames(&output.0.lock().expect("lock"));
        assert_eq!(kinds(&frames), "HPPPEEEEEEEE");
        let merged = &frames[1];
        assert_eq!(merged.header["id"], 3);
        assert_eq!(merged.header["pages"], 3);
        assert_eq!(merged.header["diagnostics"], json!([]));
        assert_eq!(merged.header["encrypted"], false);
        assert!(merged.header["timings"]["pdf"].as_f64().is_some());
        assert!(merged.blobs[0].starts_with(b"%PDF-1.7"));
        assert_eq!(frames[2].header["pages"], 3);
        assert_eq!(frames[2].blobs[0], frames[3].blobs[0]);
        let messages: Vec<&str> = frames[4..]
            .iter()
            .map(|frame| frame.header["message"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            messages[..6],
            [
                "there are no PDF files to merge",
                "the PDF at index 1 cannot be read: it is not a PDF file (it has no `%PDF-` header)",
                "the PDF at index 0 has 2 pages, not page 3",
                "invalid pages `x`: expected page numbers and ranges like `1-3,5,8-`",
                "data block 2 is missing",
                "invalid date `gestern`",
            ]
        );
        assert!(
            messages[6].starts_with("the document needs more memory than its budget"),
            "{}",
            messages[6]
        );
        assert_eq!(codes(&frames[10]), ["limit.memory"]);
        assert!(
            messages[7].starts_with("invalid request: missing field `inputs`"),
            "{}",
            messages[7]
        );
        for (frame, id) in frames[4..].iter().zip(6..) {
            assert_eq!(frame.header["id"], id);
        }
    }

    #[test]
    fn ends_at_a_frame_cut_short() {
        // A frame that claims a gigabyte and ends after a few bytes.
        let mut input = ((1u32 << 30) - 1).to_le_bytes().to_vec();
        input.extend_from_slice(b"R\x02\x00\x00\x00{}");
        let mut channel = Channel::new(io::Cursor::new(input), io::sink());
        let error = channel.read().expect_err("cut short");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn reports_bad_requests() {
        let input = frame(RENDER, &json!({ "id": 1, "html": 3 }), &[b"<p>x</p>"]);
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        assert_eq!(frames[1].kind, ERROR);
        assert_eq!(frames[1].header["message"], "data block 3 is missing");
    }

    #[test]
    fn describes_attachments_by_their_names() {
        // PDF/A-3 and PDF/UA refuse attachments without a description; the name stands in.
        let attachment = |description: Value| json!({ "name": "daten.csv", "mime": "text/csv", "description": description, "blob": 1 });
        let mut input = Vec::new();
        for (id, description) in [(1, Value::Null), (2, json!("")), (3, json!("Die Daten"))] {
            input.extend(frame(
                RENDER,
                &json!({
                    "id": id,
                    "html": 0,
                    "conformance": "3b",
                    "attachments": [attachment(description)],
                    "created": "2026-11-02T12:00:00Z",
                }),
                &[b"<p>Anhang</p>", b"a;b"],
            ));
        }
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HPPP", "{:?}", frames.last().map(|frame| &frame.header));
    }

    #[test]
    fn sets_the_version_and_description_of_invoices() {
        let xml: &[u8] = b"<rsm:CrossIndustryInvoice><ram:GuidelineSpecifiedDocumentContextParameter>\
            <ram:ID>urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0</ram:ID>\
            </ram:GuidelineSpecifiedDocumentContextParameter></rsm:CrossIndustryInvoice>";
        let mut input = Vec::new();
        for (id, version, description) in [
            (1, json!("3.0"), json!("XRechnung Invoice")),
            (2, Value::Null, Value::Null),
            (3, json!(""), Value::Null),
        ] {
            input.extend(frame(
                RENDER,
                &json!({
                    "id": id,
                    "html": 0,
                    "facturX": { "blob": 1, "version": version, "description": description },
                    "created": "2026-11-02T12:00:00Z",
                }),
                &[b"<p>Rechnung</p>", xml],
            ));
        }
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HPPE");
        let has = |pdf: &[u8], text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
        assert!(has(&frames[1].blobs[0], "<fx:Version>3.0</fx:Version>"));
        assert!(has(&frames[2].blobs[0], "<fx:Version>1.0</fx:Version>"));
        assert_eq!(
            frames[3].header["message"],
            "the document does not meet Factur-X: the invoice version is empty"
        );
    }

    #[test]
    fn writes_the_initial_view() {
        let mut input = Vec::new();
        for (id, view) in [
            (
                1,
                json!({ "pageMode": "outlines", "pageLayout": "one-column", "zoom": "fit-width", "page": 2,
                        "fitWindow": true, "centerWindow": true }),
            ),
            (2, json!({ "zoom": 150 })),
            (3, Value::Null),
            (4, json!({})),
            (5, json!({ "page": 3, "zoom": 62.5 })),
            (6, json!({ "pageMode": "bookmarks" })),
            (7, json!({ "pageLayout": "two" })),
            (8, json!({ "zoom": 0 })),
            (9, json!({ "zoom": "wide" })),
        ] {
            input.extend(frame(
                RENDER,
                // PDF/A-1b has no object streams: the catalog is there to read.
                &json!({ "id": id, "html": 0, "view": view, "conformance": "1b", "created": "2026-11-25T12:00:00Z" }),
                &[b"<title>Handbuch</title><p>Eins<p style='break-before: page'>Zwei"],
            ));
        }
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HPPPPPEEEE");
        let has = |pdf: &[u8], text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
        let first = &frames[1].blobs[0];
        for entry in [
            "/ViewerPreferences<</DisplayDocTitle true/FitWindow true/CenterWindow true>>",
            "/PageMode/UseOutlines",
            "/PageLayout/OneColumn",
            "/FitH 841.92]",
        ] {
            assert!(has(first, entry), "{entry}");
        }
        assert!(has(&frames[2].blobs[0], "/XYZ null 841.92 1.5]"));
        // No view and an empty one are the same file.
        assert!(!has(&frames[3].blobs[0], "/OpenAction"));
        assert_eq!(frames[3].blobs[0], frames[4].blobs[0]);
        // A page the document does not have is left out, and said.
        assert!(has(&frames[5].blobs[0], "/XYZ null 841.92 0.625]"));
        assert_eq!(frames[5].header["diagnostics"][0]["code"], "pdf.view-ignored");
        assert_eq!(frames[6].header["message"], "unknown page mode `bookmarks`");
        assert_eq!(frames[7].header["message"], "unknown page layout `two`");
        assert_eq!(frames[8].header["message"], "invalid zoom `0`");
        assert_eq!(frames[9].header["message"], "invalid zoom `wide`");
    }

    #[test]
    fn encrypts_with_passwords_and_permissions() {
        let mut input = Vec::new();
        for (id, encryption) in [
            (
                1,
                json!({ "userPassword": "user", "ownerPassword": "owner", "permissions": ["print", "copy"] }),
            ),
            (2, json!({ "ownerPassword": "owner" })),
            (3, Value::Null),
            (
                4,
                json!({ "userPassword": "user", "permissions": ["everything"] }),
            ),
            (5, json!({})),
        ] {
            input.extend(frame(
                RENDER,
                &json!({ "id": id, "html": 0, "encryption": encryption }),
                &[b"<p>Gehalt</p>"],
            ));
        }
        input.extend(frame(
            RENDER,
            &json!({ "id": 6, "html": 0, "conformance": "2b", "encryption": { "userPassword": "user" } }),
            &[b"<p>Archiv</p>"],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HPPPEEE");
        let has = |pdf: &[u8], text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
        assert!(has(&frames[1].blobs[0], "/Filter/Standard/V 5/R 6"));
        assert!(has(&frames[1].blobs[0], "/P -1324/"), "print and copy");
        assert_eq!(frames[1].header["encrypted"], true);
        assert!(has(&frames[2].blobs[0], "/P -4/"), "everything");
        assert_eq!(frames[3].header["encrypted"], false);
        assert_eq!(frames[3].header["signed"], false);
        assert!(!has(&frames[3].blobs[0], "/Encrypt"));
        assert_eq!(frames[4].header["message"], "unknown permission `everything`");
        assert_eq!(
            frames[5].header["message"],
            "the PDF could not be written: encryption needs a user password or an owner password"
        );
        assert_eq!(
            frames[6].header["message"],
            "the document does not meet PDF/A-2b: the PDF cannot be encrypted (PDF/A forbids encryption)"
        );
    }

    #[test]
    fn sends_the_diagnostics_of_failures() {
        // PDF/UA refuses a document without a title; the CSS warning found before comes along.
        let input = frame(
            RENDER,
            &json!({ "id": 4, "html": 0, "accessible": true }),
            &[b"<style>p { colr: red }</style><p>Brief</p>"],
        );
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        assert_eq!(frames[1].kind, ERROR);
        assert_eq!(frames[1].header["id"], 4);
        assert_eq!(frames[1].header["diagnostics"][0]["code"], "css.unknown-property");
        assert_eq!(frames[1].header["diagnostics"][0]["severity"], "warning");
        // The worker's memory comes with failures too, where the system tells it (on FreeBSD
        // once the process has run for a clock tick).
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(frames[1].header.get("maxRss").is_some());
        } else if !cfg!(target_os = "freebsd") {
            assert!(frames[1].header.get("maxRss").is_none());
        }
    }

    #[test]
    fn makes_form_fields_when_asked() {
        let html: &[u8] = b"<input name=city value=Berlin>";
        let mut input = frame(RENDER, &json!({ "id": 1, "html": 0 }), &[html]);
        input.extend(frame(
            RENDER,
            &json!({ "id": 2, "html": 0, "forms": true }),
            &[html],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        // The field's appearance is a stream of its own beside the page's.
        assert_eq!(frames[1].header["pages"], 1);
        assert_eq!(frames[2].header["pages"], 1);
        assert_eq!(frames[1].header["forms"], false);
        assert_eq!(frames[2].header["forms"], true);
        let streams = |pdf: &[u8]| pdf.windows(9).filter(|window| window == b"endstream").count();
        assert_eq!(streams(&frames[2].blobs[0]), streams(&frames[1].blobs[0]) + 1);
    }

    #[test]
    fn writes_images_as_jpegs_of_a_quality() {
        let html: &[u8] = b"<img src=photo.webp width=200>";
        let photo: &[u8] = include_bytes!("../../../tests/corpus/images/photo.webp");
        let request = |id: u32, quality: Value| {
            let mut header = json!({
                "id": id,
                "html": 0,
                "resources": [{ "url": "photo.webp", "blob": 1 }],
                "created": "2026-11-25T12:00:00Z",
            });
            if !quality.is_null() {
                header["imageQuality"] = quality;
            }
            frame(RENDER, &header, &[html, photo])
        };
        let mut input = Vec::new();
        for (id, quality) in [
            Value::Null,
            json!(80),
            json!(30),
            json!(0),
            json!(101),
            json!(-1),
            json!(80.5),
            json!("80"),
        ]
        .into_iter()
        .enumerate()
        {
            input.extend(request(id as u32 + 1, quality));
        }
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HPPPEEEEE");
        let jpeg = |pdf: &[u8]| pdf.windows(10).any(|window| window == b"/DCTDecode");
        let (lossless, high, low) = (&frames[1].blobs[0], &frames[2].blobs[0], &frames[3].blobs[0]);
        assert!(!jpeg(lossless) && jpeg(high) && jpeg(low));
        assert!(low.len() < high.len() && high.len() < lossless.len());
        // A quality that is none is an error, not a guess.
        for (frame, quality) in frames[4..7].iter().zip(["0", "101", "-1"]) {
            assert_eq!(
                frame.header["message"],
                format!("the image quality must be between 1 and 100, not {quality}")
            );
        }
        for frame in &frames[7..] {
            let message = frame.header["message"].as_str().expect("a message");
            assert!(message.starts_with("invalid request"), "{message}");
        }
    }

    #[test]
    fn parses_xhtml_as_xml() {
        // An empty-element tag closes the `div` in XHTML only; in HTML it holds the paragraph.
        let html: &[u8] = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><div style="break-after: page"/><p>x</p></body></html>"#;
        let mut input = frame(RENDER, &json!({ "id": 1, "html": 0 }), &[html]);
        input.extend(frame(
            RENDER,
            &json!({ "id": 2, "html": 0, "xhtml": true }),
            &[html],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        assert_eq!(frames[1].header["pages"], 1);
        assert_eq!(frames[2].header["pages"], 2);
    }

    #[test]
    fn signs_and_asks_the_caller_to_sign() {
        let fixture = |name: &str| {
            std::fs::read(format!(
                "{}/../../tests/signing/{name}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("fixture")
        };
        let mut input = frame(
            RENDER,
            &json!({
                "id": 1,
                "html": 0,
                "created": "2026-11-19T09:30:00Z",
                "signature": {
                    "pkcs12": 1,
                    "password": "fagus-test-only",
                    "reason": "Rechnung",
                    "certify": "fill-forms",
                },
            }),
            &[b"<p>Rechnung</p>", &fixture("ecdsa.p12")],
        );
        input.extend(frame(
            RENDER,
            &json!({
                "id": 2,
                "html": 0,
                "signature": {
                    "certificates": [1, 2],
                    "location": "Berlin",
                    "area": { "page": 1, "x": 50, "y": 60, "width": 200, "height": 40 },
                },
            }),
            &[b"<p>Rechnung</p>", &fixture("rsa.pem"), &fixture("ca.pem")],
        ));
        // The caller cannot sign.
        input.extend(frame(SIGNED, &json!({ "error": "no key" }), &[]));
        input.extend(frame(
            RENDER,
            &json!({ "id": 3, "html": 0, "signature": { "pkcs12": 1, "password": "falsch" } }),
            &[b"<p>Rechnung</p>", &fixture("ecdsa.p12")],
        ));
        input.extend(frame(
            RENDER,
            &json!({
                "id": 4,
                "html": 0,
                "signature": { "pkcs12": 1, "password": "fagus-test-only", "element": "signature" },
            }),
            &[b"<p id=signature>Unterschrieben</p>", &fixture("ecdsa.p12")],
        ));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HPSEEP");
        let has = |pdf: &[u8], text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
        assert!(has(&frames[1].blobs[0], "/Reason(Rechnung)"));
        assert_eq!(frames[1].header["signed"], true);
        assert!(has(&frames[1].blobs[0], "/M(D:20261119093000Z)"));
        assert!(has(
            &frames[1].blobs[0],
            "/TransformParams<</Type/TransformParams/P 2/V/1.2>>"
        ));
        assert!(has(&frames[1].blobs[0], "/Perms<</DocMDP "));
        assert_eq!(frames[2].header["id"], 2);
        assert_eq!(frames[2].header["algorithm"], "RSA-SHA256");
        assert_eq!(frames[2].blobs[0].len(), 32);
        assert_eq!(frames[3].header["id"], 2);
        assert_eq!(
            frames[3].header["message"],
            "the PDF could not be signed: the signer failed: no key"
        );
        assert!(
            frames[4].header["message"]
                .as_str()
                .is_some_and(|message| message.contains("the PKCS#12 file cannot be read")),
            "{}",
            frames[4].header["message"]
        );
        // A visible signature: in the field made for it, not in one of no size.
        assert_eq!(frames[5].header["signed"], true);
        assert!(!has(&frames[5].blobs[0], "/Rect[0 0 0 0]"));
    }

    #[test]
    fn asks_the_caller_for_revocation_data() {
        let p12 = std::fs::read(format!(
            "{}/../../tests/signing/ecdsa.p12",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("fixture");
        let request = |id: u32| {
            frame(
                RENDER,
                &json!({
                    "id": id,
                    "html": 0,
                    "signature": { "pkcs12": 1, "password": "fagus-test-only", "revocation": true },
                }),
                &[b"<p>Rechnung</p>", &p12],
            )
        };
        let mut input = request(1);
        // Nothing known of the signer's certificate: the certificates alone.
        input.extend(frame(REVOKED, &json!({}), &[]));
        input.extend(request(2));
        input.extend(frame(REVOKED, &json!({ "error": "offline" }), &[]));
        let output = Shared::default();
        serve(engine(), io::Cursor::new(input), output.clone()).expect("served");
        let output = output.0.lock().expect("lock").clone();
        let frames = frames(&output);
        let kinds: String = frames.iter().map(|frame| char::from(frame.kind)).collect();
        assert_eq!(kinds, "HVPVE");
        assert_eq!(frames[1].header["id"], 1);
        assert_eq!(frames[1].header["ocspUrls"], json!([]));
        assert_eq!(frames[1].header["crlUrls"], json!([]));
        // The certificate, its issuer's and the OCSP request.
        assert_eq!(frames[1].blobs.len(), 3);
        assert_eq!(frames[1].blobs[2][0], 0x30);
        let has = |pdf: &[u8], text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
        assert!(has(&frames[2].blobs[0], "/DSS "));
        assert_eq!(
            frames[4].header["message"],
            "the PDF could not be signed: getting the revocation status failed: offline"
        );
    }
}
