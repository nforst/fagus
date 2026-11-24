# Rust library

The crate `fagus` is the engine the PHP package and the command line tool are built on. Use it to
render PDFs inside a Rust program, without a separate process.

```rust
let mut engine = fagus::Engine::new();
engine.add_font_dir("fonts")?;
let output = engine.render(&fagus::Document::html(html).base_directory("assets"))?;
std::fs::write("invoice.pdf", output.pdf)?;
```

## Add the crate

The crate is not published on crates.io ([why](stability.md#the-rust-crate)). Depend on it from the Git
repository, pinned to a release tag (releases are tagged `v` and their version) or to a commit
with `rev`:

```toml
[dependencies]
fagus = { git = "https://github.com/nforst/fagus", tag = "v0.2.0" }
```

A checkout of the repository works as a path dependency:
`fagus = { path = "../fagus/crates/fagus" }`.

The crate needs Rust 1.97 or newer (edition 2024); CI builds and tests it with 1.97.0 as well as
with the current release. Its code has no `unsafe`.

### Features

| Feature | Default | Effect |
|---|---|---|
| `parallel` | on | Compresses the page contents on rayon's global thread pool while later pages are drawn |

Without it (`default-features = false`), each document is written on one thread. With it,
`RAYON_NUM_THREADS` or your own `rayon::ThreadPoolBuilder::build_global()` sets the size of the
pool; the `fagus` binary uses two threads.

## The engine

An `Engine` holds the fonts, the parsed default style sheet and caches: faces loaded through
`@font-face` (up to 256 MiB), decoded images and parsed style sheets are kept for the next
documents. Create one and render many documents with it.

| Method | Description |
|---|---|
| `Engine::new()`, `Engine::default()` | An engine with the built-in fonts (Inter regular and bold), which stand in where no other font fits |
| `add_font_dir(&mut self, dir) -> Result<usize, Error>` | Adds the fonts (TTF, OTF, TTC, OTC, WOFF, WOFF2) in a directory and its subdirectories; returns the number of faces |
| `add_font_data(&mut self, Vec<u8>) -> Result<usize, Error>` | Adds a font file's data (TTF, OTF, TTC, OTC, WOFF, WOFF2); returns the number of faces |
| `add_system_fonts(&mut self) -> usize` | Adds the fonts installed on the machine, found as Chrome finds them; the output then depends on the machine |
| `set_locale(&mut self, Option<String>)` | The language of the texts form controls show where the document gives no `lang` (`"de-DE"`); also the shaping language of text without `lang` and the PDF's language when `<html>` has none |
| `set_generic_families(&mut self, sans_serif, serif, monospace)` | The families (`Option<String>` each) used for `sans-serif`, `serif` and `monospace` |
| `set_limits(&mut self, Limits)`, `limits(&self) -> Limits` | The memory budget and time limit of each document, see below |
| `render(&self, &Document) -> Result<Output, Error>` | Renders a document to PDF |
| `render_with_diagnostics(&self, &Document) -> Result<Output, Failure>` | The same; a `Failure` has the `error` and the `diagnostics` found before it |

Fonts, locale and generic families are described in [fonts.md](fonts.md) and
[php.md](php.md); the Rust methods behave as the PHP options of the same name.

`Limits` bounds one document as a whole ([cli.md](cli.md#limits) says what it counts):
`Limits::new()` (also `Default`) has a memory budget of `Limits::DEFAULT_MEMORY` (2 GiB) and no
time limit, `Limits::unlimited()` neither; `memory(Option<u64>)` sets the budget in bytes and
`time(Option<Duration>)` the time limit, `memory_budget()` and `time_limit()` read them. A
document that needs more fails with `Error::MemoryBudget` or `Error::TimeLimit`; the time counts
from the call to `render`. They never change the output of a document that stays within them.

```rust
use std::time::Duration;

engine.set_limits(fagus::Limits::new().memory(Some(1 << 30)).time(Some(Duration::from_secs(20))));
```

## Documents

A `Document` is built with methods that take and return it. It is `Clone`, so one prepared
document can be rendered with variations.

| Method | Description |
|---|---|
| `Document::html(impl Into<String>)` | An HTML document |
| `Document::xhtml(impl Into<String>)` | An XHTML document, parsed as XML |
| `base_directory(impl Into<PathBuf>)` | The directory relative URLs resolve against; files are only read inside it (or `root`). Without it, no files are read |
| `root(impl Into<PathBuf>)` | A larger directory files may be read from, which contains the base directory; URLs starting with `/` resolve against it. It takes effect only with a base directory |
| `stylesheet(name, css)` | A style sheet after the document's own; `name` appears in diagnostics |
| `resource(url, Vec<u8>)` | The data of a resource, by its URL as written in the document |
| `loader(Arc<dyn ResourceLoader>)` | A loader for resources that are not handed over; asked before a file is read, for [many at once](#load-many-resources-at-once) where it can fetch them concurrently |
| `conformance(Conformance)` | PDF/A |
| `tagged(bool)` | Whether the PDF is tagged (default `true`) |
| `accessible()` | PDF/UA-1 |
| `forms(bool)` | Whether form controls become the fields of an interactive form (default `false`) |
| `image_quality(u8)` | Writes opaque images as JPEGs of this quality, 1 to 100 (default: without loss); another value fails the render with `PdfError::ImageQuality` ([pdf.md](pdf.md#make-photos-smaller)) |
| `attach(Attachment)` | Attaches a file |
| `factur_x(Vec<u8>)` | A ZUGFeRD / Factur-X invoice with this XML, its profile read from the XML |
| `factur_x_profile(Vec<u8>, FacturXProfile)` | The same with the profile given |
| `factur_x_version(impl Into<String>)` | The `Version` of the invoice's XMP metadata (default `1.0`), before or after `factur_x` |
| `factur_x_description(impl Into<String>)` | The description of the attached invoice XML (default "Factur-X/ZUGFeRD invoice"), before or after `factur_x` |
| `created(Date)` | The creation date (default: now), for reproducible output |
| `report_layout(bool)` | Whether `Output::layout` reports where the elements with an `id` ended up (default `false`) |
| `encrypt(Encryption)` | Protects the PDF with passwords (AES-256) |
| `sign(Signature)` | Signs the PDF digitally (PAdES), see below |
| `limits(Limits)` | The memory budget and time limit of this document, in place of the engine's |

What these options do to the PDF is described in [pdf.md](pdf.md), how resources are found in
[resources.md](resources.md).

The types they take:

- `Conformance`: `PdfA1b`, `PdfA2b`, `PdfA2u`, `PdfA2a`, `PdfA3b`, `PdfA3u`, `PdfA3a`, `PdfA4`,
  `PdfA4e`, `PdfA4f`. `Conformance::from_level("3b")` parses a name (`"4"` for PDF/A-4),
  `name()` gives `"PDF/A-3b"`, `part()` the part of ISO 19005 (1 to 4), `is_tagged()` says
  whether the level requires tags.
- `Attachment::new(name, mime_type, data)`, where `data` is a `Vec<u8>` or an `Arc<Vec<u8>>`,
  with `description(…)` and `relationship(…)`, a `Relationship` (`Source`, `Data`,
  `Alternative`, `Supplement`, or `Unspecified` if not given). PDF/A-3 and PDF/UA require a
  `description`. The fields `name`, `mime_type`, `description`, `relationship` and `data` are
  public to read.
- `FacturXProfile`: `Minimum`, `BasicWl`, `Basic`, `En16931`, `Extended`, `XRechnung`.
  `FacturXProfile::detect(&xml)` reads the profile an XML declares.
- `Encryption::new()` with `user_password(…)`, `owner_password(…)` and
  `permissions([Permission::Print, …])` (everything unless given), and at least one of the
  passwords; `Permission` is `Print`, `PrintLowQuality`, `Modify`, `Copy`, `Annotate`,
  `FillForms` or `Assemble`, with `from_name("fill-forms")` and `name()`. Its `Debug` hides the
  passwords. The keys come from the operating system; `seed([u8; 32])` takes them from a
  generator with that seed instead, so that tests can compare bytes (never in production: the
  seed decrypts the file).
- `Date` in UTC, with the public fields `year`, `month`, `day`, `hour`, `minute` and `second`.
  `Date::new(2026, 11, 1, 8, 0, 0)` (a `const fn`), `Date::now()` and
  `Date::from_unix(seconds)` make one; convert a local time to UTC before.

- `Signature`: a digital signature ([pdf.md](pdf.md#sign-the-pdf)).
  `Signature::pkcs12(&data, password)` reads the key and certificates of a PKCS#12 file;
  `Signature::with_signer(certificates, signer)` signs with a key outside, where `certificates`
  are DER or PEM (the signer's first) and `signer` implements the trait `Signer`, which closures
  `Fn(&[u8], Algorithm) -> Result<Vec<u8>, String>` do: it gets the SHA-256 digest to sign and
  `Algorithm::RsaSha256` or `Algorithm::EcdsaP256Sha256`. `name`, `reason`, `location`,
  `contact_info` and `time(Date)` set what the signature says (the time defaults to the creation
  date); `timestamp(timestamper)` timestamps it (PAdES B-T) through the trait `Timestamper`
  (closures `Fn(&[u8]) -> Result<Vec<u8>, String>`), which gets the SHA-256 digest of the
  signature and returns the RFC 3161 answer of an authority. `fagus::timestamp_request(&digest)`
  makes the request to send. `certify(Certification::FillForms)` makes it a certification
  signature (`NoChanges`, `FillForms`, `Annotate`). `visible_on(id)` shows it in the box of an
  element, `visible_at(page, x, y, width, height)` in an area of a page (from 1, in points from
  its top left corner); `placement()` returns the `Placement`. `revocation(source)` adds the
  validation data (PAdES B-LT) through the trait `Revocation` (closures
  `Fn(&RevocationRequest) -> Result<Vec<Vec<u8>>, String>`), which gets the certificate, its
  issuer, `ocsp_urls`, `crl_urls` and the `ocsp_request`, and returns OCSP responses and CRLs
  (DER); `archive_timestamp()` adds a document timestamp after it (B-LTA). An encrypted document
  is signed with its file key. A signature that fails makes rendering fail with
  `PdfError::Sign`.

A document's metadata comes from its HTML (`<title>`, `<html lang>`, `<meta>`), see
[pdf.md](pdf.md); there is no method to set it.

## Load resources

A resource is looked for as a `data:` URL, among the resources handed over with `resource()`,
through the loader, and last as a file inside the base directory or root. Fagus itself never goes
to the network. Implement `ResourceLoader` to serve resources from a database, a cache or the
network, under your own rules:

```rust
use std::sync::Arc;

use fagus::{Document, ResourceLoader};

/// Answers with the files of the application's storage.
struct Storage;

impl ResourceLoader for Storage {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        let key = url.strip_prefix("storage://")?;
        std::fs::read(std::path::Path::new("assets").join(key)).ok()
    }
}

let document = Document::html(r#"<img src="storage://logo.svg" alt="Logo">"#)
    .loader(Arc::new(Storage));
```

`load` gets the URL exactly as written in the document and returns the data or `None`; for a URL
with a fragment (`icons.svg#check`) it gets one more call without the fragment if it returns `None`. It is
called once for each URL in a render, whatever it returns. The
loader must be `Send + Sync`: it is called from the engine's rendering thread. The documents of
frames ask for their resources by their URLs resolved against the frame's URL.

### Load many resources at once

A render knows most of its resources before it uses them, and asks for them together with
`load_many`: the linked style sheets of the document, then the first font file of each
`@font-face` rule, then the images, at most `fagus::BATCH` (64) URLs at a time. The default
implementation calls `load` for one after the other. A loader that fetches from a slow place
implements it to fetch them concurrently:

```rust
impl ResourceLoader for Storage {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        self.fetch(url)
    }

    fn load_many(&self, urls: &[&str]) -> Vec<Option<Vec<u8>>> {
        std::thread::scope(|scope| {
            let fetches: Vec<_> = urls
                .iter()
                .map(|url| scope.spawn(move || self.fetch(url)))
                .collect();
            fetches
                .into_iter()
                .map(|fetch| fetch.join().ok().flatten())
                .collect()
        })
    }
}
```

The answers are in the order of `urls`; an answer shorter than `urls` leaves the rest not found.
`load_many` is asked only for URLs that `load` would be asked for anyway, in the same order, so
the PDF does not depend on which of the two a loader implements. Resources that only show while
others are read still come to `load`: the style sheets of `@import` rules, a later source of a
font whose first could not be loaded, the sprites and images of inline SVG, and the documents of
`<iframe>`s. The time limit of a render is looked at after each batch. `load_many` is new in
1.2.0.

## The output

`render` returns an `Output`:

| Field | Type | Content |
|---|---|---|
| `pdf` | `Vec<u8>` | The PDF file |
| `pages` | `usize` | The number of pages |
| `diagnostics` | `Vec<Diagnostic>` | CSS that was ignored, resources that are missing, warnings about the PDF |
| `timings` | `Timings` | `parse`, `style`, `layout`, `paint` and `pdf`, each a `Duration` |
| `layout` | `Option<Layout>` | Where the elements with an `id` ended up, with `report_layout(true)` |

A `Diagnostic` has a `severity` (`Severity::Info`, `Warning` or `Error`, ordered so that
`severity >= Severity::Warning` works), a stable `code` such as `css.unknown-property`, a
`message`, a `source` (a file name, `<style> 1`, `resources`, `pdf`) and a 1-based `line` and
`column`, 0 where unknown. `Diagnostic::new(severity, code, message, source)` with `at(line, column)` makes one, for tests
or for diagnostics of your own. Its `Display` writes it on one line:

```text
<style> 1:12:3: warning[css.unknown-property]: unknown property `colr`; the declaration is ignored
```

Diagnostics never make `render` fail. To treat them as errors, check `output.diagnostics`
yourself.

### The layout

With `report_layout(true)`, `output.layout` tells where each element with an `id` ended up, in
PDF points from the top left corner of each page box (before `page-orientation` and `transform`):

```rust
use fagus::{Document, RowSection};

let output = engine.render(&Document::html(html).report_layout(true))?;
let layout = output.layout.expect("asked for");
if let Some(total) = layout.element("total") {
    let last = total.boxes.last().expect("rendered");
    println!("the total ends {}pt down page {}", last.y + last.height, last.page);
}
for row in layout.element("items").map_or(&[][..], |table| &table.rows) {
    if row.section == RowSection::Body {
        println!("row {} is on pages {:?}", row.index, row.pages());
    }
}
```

- `Layout { pages: Vec<LayoutPage>, elements: Vec<LayoutElement> }`, with `element(id)` for the
  first element with that `id`. A `LayoutPage` has the `width` and `height` of the page box.
- `LayoutElement { id, tag, boxes: Vec<LayoutBox>, rows: Vec<LayoutRow> }`: a box for each page
  the element is on (and each line of an inline element), none for an element that is not
  rendered; `rows` for a table. `pages()` lists the page numbers.
- `LayoutBox { page, x, y, width, height }`, its `page` counted from 1.
- `LayoutRow { section, index, id, boxes }`: a row of a table (`<tr>`) in document order, its
  `section` a `RowSection` (`Header`, `Body`, `Footer`) and `index` its place among the rows of
  that section; its box on a page is the area of its cells there.

The types are `#[non_exhaustive]`, so that fields can be added ([stability](#stability));
[php.md](php.md#find-where-elements-landed) shows how to render twice to adjust a document to
its layout.

## Errors

`render` fails with `fagus::Error`:

| Variant | When |
|---|---|
| `Error::Pdf` | The document does not meet PDF/A or PDF/UA (the message lists every problem), the version or description of an invoice is empty or has control characters, or the PDF could not be written |
| `Error::Font` | A font could not be read (also from `add_font_dir` and `add_font_data`) |
| `Error::MemoryBudget { budget, held }` | The document needs more memory than its budget (`budget` bytes); `held` names what its parts held then, in bytes, the largest first |
| `Error::TimeLimit { limit }` | The document took longer than its time limit |
| `Error::Internal(String)` | A bug in Fagus: rendering stopped with a panic, which the engine caught |

The types inside are exported too: `Error::Pdf` holds a `fagus::PdfError`, either
`PdfError::Conformance { standard, problems }` (the standard, like `PDF/UA-1`, and the problems in
words, separated by semicolons), `PdfError::Write(String)` or `PdfError::Sign(String)` (the
signature could not be made: a wrong password, a signer that failed); `Error::Font` holds a
`fagus::FontError`, `FontError::InvalidFont` or `FontError::Io(std::io::Error)`. The three enums
are `#[non_exhaustive]`: a `match` over them needs a `_` arm. Their `Display` gives the whole
message:

```rust
match engine.render(&document.accessible()) {
    Ok(output) => std::fs::write("letter.pdf", output.pdf)?,
    Err(fagus::Error::Pdf(error)) => eprintln!("refused: {error}"),
    Err(error) => return Err(error.into()),
}
// refused: the document does not meet PDF/UA-1: the document has no title (add a <title>); …
```

`render_with_diagnostics` fails with a `fagus::Failure` instead, which keeps the diagnostics
found before the failure (an image that could not be loaded may explain why PDF/UA refuses the
document; for `MemoryBudget` and `TimeLimit` it holds the diagnostic `limit.memory` or
`limit.time`). Its `error` is the `Error` that `render` returns, and it displays as that error:

```rust
match engine.render_with_diagnostics(&document.accessible()) {
    Ok(output) => std::fs::write("letter.pdf", output.pdf)?,
    Err(failure) => {
        for diagnostic in &failure.diagnostics {
            eprintln!("{diagnostic}");
        }
        return Err(failure.error.into());
    }
}
```

## Threads

`Engine`, `Document`, `Output`, `Error` and `Failure` are `Send` and `Sync`. `render` takes `&self`, so one
engine, shared by reference or in an `Arc`, renders documents from many threads at once:

```rust
let pdfs = std::thread::scope(|scope| {
    let handles: Vec<_> = invoices
        .iter()
        .map(|html| scope.spawn(|| engine.render(&fagus::Document::html(html.as_str()))))
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.join().expect("rendering thread").map(|output| output.pdf))
        .collect::<Result<Vec<_>, _>>()
})?;
```

The engine renders each document on a thread of its own with a large stack (64 MiB, reserved,
used as needed), since deeply nested documents need more than the threads of most programs have.
It keeps these threads for the next documents and adds one while all are busy, up to one per
processor. `render` blocks until the document is done.

The methods that change the engine take `&mut self`: add fonts and settings before you share the
engine. They apply to every document rendered after them.

## Stability

The crate follows [stability.md](stability.md): from 1.0 on, a minor release does not break code
that uses it. So that a minor release can add a variant or a field, these types are
`#[non_exhaustive]`:

- the enums `Error`, `PdfError`, `FontError`, `Conformance`, `Relationship`, `FacturXProfile`,
  `Permission`, `Algorithm`, `Certification`, `Placement` and `RowSection`: a `match` over one
  needs a `_` arm;
- the structs with public fields `Output`, `Timings`, `Failure`, `Diagnostic`, `Attachment`,
  `Date`, `RevocationRequest`, `Layout`, `LayoutPage`, `LayoutElement`, `LayoutBox` and
  `LayoutRow`: read their fields
  and set them, but make them with their constructors instead of struct literals
  (`Attachment::new`, `Date::new`, `Diagnostic::new`, `Output::default()`,
  `Timings::default()`, `Failure::from(error)`); a pattern that takes one apart needs `..`.

`Severity` is not: `Info`, `Warning` and `Error` are the severities of the command line and the
worker protocol and stay the same within a major release. Which severity a diagnostic gets is not
stable. `Document`, `Engine`, `Encryption`, `Signature` and `Limits` have no public fields. The
traits `ResourceLoader`, `Signer`, `Timestamper` and `Revocation` get new methods only with a
default implementation.

Something deprecated gets `#[deprecated(since = "…", note = "use … instead")]`, a compiler
warning, and keeps working until the next major release.

## A complete example

An invoice with fonts of its own, an attachment and the Factur-X XML, as PDF/A-3b with a fixed
date. `assets/` holds `invoice.html`, `invoice.xml` and `fonts/` (the HTML and XML of the example
in [pdf.md](pdf.md#a-complete-example) fit):

```rust
use fagus::{Attachment, Date, Document, Engine, Relationship, Severity};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = Engine::new();
    engine.add_font_dir("assets/fonts")?;
    engine.set_locale(Some("de-DE".to_owned()));

    let html = std::fs::read_to_string("assets/invoice.html")?;
    let xml = std::fs::read("assets/invoice.xml")?;

    let document = Document::html(html)
        .base_directory("assets")
        .stylesheet("print.css", "body { color: #000 }")
        .factur_x(xml)
        .attach(
            Attachment::new("timesheet.csv", "text/csv", b"date,hours\n2026-09-30,6\n".to_vec())
                .description("Stundenzettel")
                .relationship(Relationship::Supplement),
        )
        .created(Date::new(2026, 11, 1, 8, 0, 0));

    let output = engine.render(&document)?;
    for diagnostic in &output.diagnostics {
        if diagnostic.severity >= Severity::Warning {
            eprintln!("{diagnostic}");
        }
    }
    std::fs::write("invoice.pdf", &output.pdf)?;
    println!("{} pages, layout took {:?}", output.pages, output.timings.layout);
    Ok(())
}
```

The command line tool in `crates/fagus-cli` is a larger example of the same API, and
[cli.md](cli.md) describes it.
