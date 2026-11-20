//! Feeds `fagus worker` (the protocol of `docs/protocol.md`) a stream of frames from the caller.
//!
//! An input that starts with a zero byte is the stream itself, byte for byte, which finds what
//! the frame parser does with lengths and headers that do not fit. Any other input describes the
//! messages (`arbitrary`), which are written as well-formed frames with headers that may still
//! be wrong in every field: that reaches the requests themselves and the rendering behind them.
#![no_main]
#![allow(dead_code)]

use std::io;

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value, json};

// The protocol code of the command line program, as it is, with the modules it uses.
#[path = "../../crates/fagus-cli/src/date.rs"]
mod date;
#[path = "../../crates/fagus-cli/src/layout_json.rs"]
mod layout_json;
#[path = "../../crates/fagus-cli/src/memory.rs"]
mod memory;
#[path = "../../crates/fagus-cli/src/request.rs"]
mod request;
#[path = "../../crates/fagus-cli/src/worker.rs"]
mod worker;

/// The worker runs outside the sandbox here, as `fagus worker --no-sandbox` does: entering it
/// would also shut libFuzzer out of the files it writes. The worker only asks for its status.
mod sandbox {
    #[derive(Clone, Copy)]
    pub(crate) enum Status {
        Off,
    }

    impl Status {
        pub(crate) fn name(self) -> &'static str {
            match self {
                Self::Off => "off",
            }
        }
    }

    pub(crate) fn status() -> Status {
        Status::Off
    }
}

/// A message from the caller.
#[derive(Arbitrary, Debug)]
enum Message {
    Render(Render),
    Font(Vec<u8>),
    Resource {
        found: Option<bool>,
        data: Option<Vec<u8>>,
    },
    /// Any type, with any header.
    Other {
        kind: u8,
        header: String,
        blobs: Vec<Vec<u8>>,
    },
}

/// A word the protocol knows, or any other.
#[derive(Arbitrary, Debug)]
enum Word {
    Known(u8),
    Any(String),
}

impl Word {
    fn of(&self, known: &[&str]) -> Value {
        match self {
            Self::Known(index) => Value::from(known[usize::from(*index) % known.len()]),
            Self::Any(text) => Value::from(text.as_str()),
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Render {
    id: u64,
    html: u8,
    xhtml: Option<bool>,
    /// Whether files are read from the images of the tests.
    files: bool,
    stylesheets: Vec<(String, u8)>,
    resources: Vec<(Word, u8)>,
    ask: bool,
    conformance: Option<Word>,
    tagged: Option<bool>,
    accessible: bool,
    attachments: Vec<Attachment>,
    factur_x: Option<FacturX>,
    created: Option<Word>,
    /// A memory budget in KiB and a time limit in milliseconds, each at least 1 (the engine's
    /// limits otherwise, see `fagus_fuzz::LIMITS`).
    limits: Option<(u16, u16)>,
    blobs: Vec<Vec<u8>>,
    /// Other fields, of any value.
    extra: Vec<(String, Scalar)>,
}

#[derive(Arbitrary, Debug)]
struct Attachment {
    name: String,
    mime: Option<String>,
    relationship: Option<Word>,
    description: Option<Option<String>>,
    blob: u8,
}

#[derive(Arbitrary, Debug)]
struct FacturX {
    blob: u8,
    profile: Option<Word>,
    version: Option<Option<String>>,
    description: Option<Option<String>>,
}

#[derive(Arbitrary, Debug)]
enum Scalar {
    Null,
    Bool(bool),
    Number(i64),
    Float(f64),
    Text(String),
}

impl Scalar {
    fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::from(*value),
            Self::Number(value) => Value::from(*value),
            Self::Float(value) => Value::from(*value),
            Self::Text(value) => Value::from(value.as_str()),
        }
    }
}

const LEVELS: &[&str] = &["2b", "2u", "2a", "3b", "3u", "3a"];
const RELATIONSHIPS: &[&str] = &["source", "data", "alternative", "supplement", "unspecified"];
const PROFILES: &[&str] = &["minimum", "basicwl", "basic", "en16931", "extended", "xrechnung"];
const DATES: &[&str] = &[
    "2026-11-02T12:00:00Z",
    "2026-11-02T13:00:00+01:00",
    "2026-11-02T12:00:00",
];
const URLS: &[&str] = &["logo.png", "style.css", "font.ttf", "a.svg", "sub/page.html"];

fn optional(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value);
    }
}

fn nullable(value: Option<&Option<String>>) -> Option<Value> {
    value.map(|value| value.as_deref().map_or(Value::Null, Value::from))
}

impl Render {
    fn header(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::from(self.id));
        map.insert("html".to_owned(), Value::from(self.html));
        optional(&mut map, "xhtml", self.xhtml.map(Value::from));
        if self.files {
            let images = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/corpus/images");
            map.insert("base".to_owned(), Value::from(images));
            map.insert("root".to_owned(), Value::from(images));
        }
        map.insert(
            "stylesheets".to_owned(),
            self.stylesheets
                .iter()
                .map(|(name, blob)| json!({ "name": name, "blob": blob }))
                .collect(),
        );
        map.insert(
            "resources".to_owned(),
            self.resources
                .iter()
                .map(|(url, blob)| json!({ "url": url.of(URLS), "blob": blob }))
                .collect(),
        );
        map.insert("ask".to_owned(), Value::from(self.ask));
        optional(
            &mut map,
            "conformance",
            self.conformance.as_ref().map(|level| level.of(LEVELS)),
        );
        optional(&mut map, "tagged", self.tagged.map(Value::from));
        map.insert("accessible".to_owned(), Value::from(self.accessible));
        map.insert(
            "attachments".to_owned(),
            self.attachments
                .iter()
                .map(|attachment| {
                    let mut map = Map::new();
                    map.insert("name".to_owned(), Value::from(attachment.name.as_str()));
                    optional(&mut map, "mime", attachment.mime.as_deref().map(Value::from));
                    optional(
                        &mut map,
                        "relationship",
                        attachment
                            .relationship
                            .as_ref()
                            .map(|word| word.of(RELATIONSHIPS)),
                    );
                    optional(&mut map, "description", nullable(attachment.description.as_ref()));
                    map.insert("blob".to_owned(), Value::from(attachment.blob));
                    Value::Object(map)
                })
                .collect(),
        );
        if let Some(invoice) = &self.factur_x {
            let mut facturx = Map::new();
            facturx.insert("blob".to_owned(), Value::from(invoice.blob));
            optional(
                &mut facturx,
                "profile",
                invoice.profile.as_ref().map(|word| word.of(PROFILES)),
            );
            optional(&mut facturx, "version", nullable(invoice.version.as_ref()));
            optional(
                &mut facturx,
                "description",
                nullable(invoice.description.as_ref()),
            );
            map.insert("facturX".to_owned(), Value::Object(facturx));
        }
        optional(
            &mut map,
            "created",
            self.created.as_ref().map(|date| date.of(DATES)),
        );
        if let Some((kib, milliseconds)) = self.limits {
            map.insert("memoryBudget".to_owned(), Value::from((u64::from(kib) + 1) << 10));
            map.insert("timeLimit".to_owned(), Value::from(u64::from(milliseconds) + 1));
        }
        for (key, value) in &self.extra {
            map.entry(key.clone()).or_insert_with(|| value.value());
        }
        Value::Object(map)
    }
}

/// Writes a frame as a caller does (the worker's own writer).
fn frame(stream: &mut Vec<u8>, kind: u8, header: Value, blobs: &[Vec<u8>]) {
    let blobs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let mut channel = worker::Channel::new(io::empty(), stream);
    let _ = channel.write(kind, header, &blobs);
}

/// A frame with a header that need not be JSON.
fn raw_frame(stream: &mut Vec<u8>, kind: u8, header: &[u8], blobs: &[Vec<u8>]) {
    let length = 1 + 4 + header.len() + blobs.iter().map(Vec::len).sum::<usize>();
    stream.extend_from_slice(&(length as u32).to_le_bytes());
    stream.push(kind);
    stream.extend_from_slice(&(header.len() as u32).to_le_bytes());
    stream.extend_from_slice(header);
    for blob in blobs {
        stream.extend_from_slice(blob);
    }
}

fn stream(messages: &[Message]) -> Vec<u8> {
    let mut stream = Vec::new();
    for message in messages {
        match message {
            Message::Render(render) => frame(&mut stream, b'R', render.header(), &render.blobs),
            Message::Font(data) => frame(&mut stream, b'F', json!({}), std::slice::from_ref(data)),
            Message::Resource { found, data } => {
                let mut header = Map::new();
                optional(&mut header, "found", found.map(Value::from));
                let blobs: &[Vec<u8>] = data.as_ref().map_or(&[], std::slice::from_ref);
                frame(&mut stream, b'r', Value::Object(header), blobs);
            }
            Message::Other { kind, header, blobs } => raw_frame(&mut stream, *kind, header.as_bytes(), blobs),
        }
    }
    stream
}

/// Messages before the worker is done with them; a render request takes long.
const MAX_MESSAGES: usize = 8;

fuzz_target!(|data: &[u8]| {
    let input = match data.split_first() {
        Some((0, raw)) => raw.to_vec(),
        _ => {
            let mut unstructured = Unstructured::new(data);
            let Ok(messages) = unstructured.arbitrary_iter::<Message>() else {
                return;
            };
            let messages: Vec<Message> = messages.take(MAX_MESSAGES).map_while(Result::ok).collect();
            stream(&messages)
        }
    };
    let mut engine = fagus::Engine::new();
    engine.set_limits(fagus_fuzz::LIMITS);
    // The worker ends with an error on a frame it cannot read; that is how it should end.
    let _ = worker::serve(engine, io::Cursor::new(input), io::sink());
});
