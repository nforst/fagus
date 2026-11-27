//! PDF files as images: a page of the file drawn as a form `XObject`, as a page template
//! (`@page { background: url(letterhead.pdf) }`), as an element and in the other places images
//! go. The PDFs Fagus writes are read back with hayro-syntax, the reader krilla embeds pages with.

use fagus::{Conformance, Date, Document, Engine, Error, Output, PdfError};
use hayro_syntax::Pdf;
use hayro_syntax::object::{Dict, Stream};

const CREATED: Date = Date::new(2026, 11, 19, 12, 0, 0);

/// What the first page of `shapes.pdf` draws (its green rectangle), and the second.
const FIRST: &[u8] = b"0 0.5 0 rg 10 10 90 80 re f";
const SECOND: &[u8] = b"0.8 0.4 0 rg 0 0 50 25 re f";

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
        .expect("fonts");
    engine
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../tests/corpus/images/media/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).expect("fixture")
}

fn render(html: &str, resources: &[(&str, Vec<u8>)]) -> Output {
    let mut document = Document::html(html).created(CREATED).tagged(false);
    for (name, data) in resources {
        document = document.resource(*name, data.clone());
    }
    engine().render(&document).expect("rendered")
}

fn codes(output: &Output) -> Vec<&'static str> {
    output
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code)
        .collect()
}

/// The decoded content of the form `XObject`s that `resources` name, and of those they name in
/// turn, and of the patterns and theirs.
fn forms(resources: &Dict<'_>, depth: u32, out: &mut Vec<Vec<u8>>) {
    if depth > 8 {
        return;
    }
    for kind in [&b"XObject"[..], b"Pattern"] {
        let Some(named) = resources.get::<Dict<'_>>(kind) else {
            continue;
        };
        for name in named.keys() {
            let Some(stream) = named.get::<Stream<'_>>(name.as_ref()) else {
                continue;
            };
            if kind == b"XObject"
                && stream
                    .dict()
                    .get::<hayro_syntax::object::Name<'_>>(b"Subtype")
                    .as_deref()
                    != Some(b"Form")
            {
                continue;
            }
            out.push(stream.decoded().map(|data| data.to_vec()).unwrap_or_default());
            if let Some(inner) = stream.dict().get::<Dict<'_>>(b"Resources") {
                forms(&inner, depth + 1, out);
            }
        }
    }
}

/// For each page of `pdf`, which of `markers` its form `XObject`s draw.
fn drawn(pdf: &[u8], markers: &[&[u8]]) -> Vec<Vec<bool>> {
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    pdf.pages()
        .iter()
        .map(|page| {
            let mut found = Vec::new();
            if let Some(resources) = page.raw().get::<Dict<'_>>(b"Resources") {
                forms(&resources, 0, &mut found);
            }
            markers
                .iter()
                .map(|marker| {
                    found
                        .iter()
                        .any(|content| content.windows(marker.len()).any(|window| window == *marker))
                })
                .collect()
        })
        .collect()
}

#[test]
fn uses_the_pages_of_a_pdf_as_page_templates() {
    // The first page of the template behind the first page, the second behind the others.
    let html = "<style>
        @page { size: A4; margin: 30mm 20mm; background: url(t.pdf#page=2) no-repeat; background-size: 100% 100% }
        @page :first { background-image: url(t.pdf) }
        p { break-after: page }
        </style><p>One<p>Two<p>Three";
    let output = render(html, &[("t.pdf", fixture("pdf/shapes.pdf"))]);
    assert_eq!(codes(&output), Vec::<&str>::new());
    assert_eq!(output.pages, 3);
    assert_eq!(
        drawn(&output.pdf, &[FIRST, SECOND]),
        [[true, false], [false, true], [false, true]]
    );
    // Each page of the template is in the PDF once, however many pages show it.
    let pdf = String::from_utf8_lossy(&output.pdf);
    assert_eq!(pdf.matches("/BBox").count(), 2, "{pdf}");
    // And the same bytes again.
    assert_eq!(
        render(html, &[("t.pdf", fixture("pdf/shapes.pdf"))]).pdf,
        output.pdf
    );
}

#[test]
fn cycles_through_template_pages_with_page_selectors() {
    // mPDF's SetDocTemplate($file, true): the pages of the template in turn.
    let html = "<style>
        @page { size: A5; margin: 20mm; background: url(t.pdf) 0 0 / 100% 100% no-repeat }
        @page :nth(2n) { background-image: url(t.pdf#page=2) }
        p { break-after: page }
        </style><p>1<p>2<p>3<p>4<p>5";
    let output = render(html, &[("t.pdf", fixture("pdf/shapes.pdf"))]);
    assert_eq!(codes(&output), Vec::<&str>::new());
    let pages: Vec<bool> = drawn(&output.pdf, &[FIRST])
        .into_iter()
        .map(|page| page[0])
        .collect();
    assert_eq!(pages, [true, false, true, false, true]);
}

#[test]
fn shows_pdf_pages_wherever_images_go() {
    let html = "<img src=t.pdf alt='Shapes' style='width: 120px'>\
        <img src='t.pdf#page=2'>\
        <div style='width: 40px; height: 20px; background: url(t.pdf) center / contain'></div>\
        <div style='width: 30px; border: 10px solid; border-image: url(t.pdf) 30% stretch'></div>\
        <ul style='list-style-image: url(t.pdf)'><li>Item</ul>\
        <p style='height: 20px'><span style='content: url(t.pdf#page=2)'></span>\
        <object data=t.pdf type=application/pdf width=60 height=30>No PDF</object>\
        <picture><source type=application/pdf srcset=t.pdf><img src=missing.png alt=Fallback></picture>\
        <div style='width: 20px; height: 20px; background-image: image-set(\"t.pdf\" type(\"application/pdf\"))'></div>";
    let output = render(html, &[("t.pdf", fixture("pdf/shapes.pdf"))]);
    assert_eq!(codes(&output), Vec::<&str>::new());
    assert_eq!(drawn(&output.pdf, &[FIRST, SECOND]), [[true, true]]);
    // The fallback of the <object> is not shown.
    assert!(!String::from_utf8_lossy(&output.pdf).contains("No PDF"));
}

#[test]
fn keeps_the_text_of_the_template() {
    // The letterhead's text stays text, in its own fonts.
    let output = render(
        "<style>@page { size: A4; margin: 0; background: url(letterhead.pdf) }</style><p>Hello",
        &[("letterhead.pdf", fixture("letterhead.pdf"))],
    );
    assert_eq!(codes(&output), Vec::<&str>::new());
    let pdf = Pdf::new(output.pdf.clone()).expect("a PDF");
    let fonts = pdf
        .objects()
        .into_iter()
        .filter_map(hayro_syntax::object::Object::into_dict)
        .filter(|dict| dict.get::<hayro_syntax::object::Name<'_>>(b"Type").as_deref() == Some(b"Font"))
        .count();
    // Inter regular and bold of the letterhead, and the font of the document.
    assert!(fonts >= 3, "{fonts} fonts");
    // Its link annotations are not taken over (it has none), nor are its pages.
    assert_eq!(output.pages, 1);
}

#[test]
fn says_why_a_pdf_cannot_be_shown() {
    let shapes = fixture("pdf/shapes.pdf");
    let mut cut = shapes.clone();
    cut.truncate(40);
    for (url, data, code, message) in [
        (
            "t.pdf#page=3",
            shapes.clone(),
            "image.broken",
            "names page 3 of a PDF file of 2 pages",
        ),
        ("t.pdf", cut, "image.broken", "could not be read"),
        (
            "t.pdf",
            fixture("pdf/locked.pdf"),
            "image.unsupported",
            "needs a password",
        ),
    ] {
        let resource = url.split('#').next().expect("a name");
        let output = render(&format!("<img src='{url}' alt=x>"), &[(resource, data)]);
        assert_eq!(codes(&output), [code], "{url}");
        assert!(
            output.diagnostics[0].message.contains(message),
            "{}",
            output.diagnostics[0].message
        );
    }
    // A file its owner only restricts opens without a password, as in viewers.
    let output = render("<img src=t.pdf>", &[("t.pdf", fixture("pdf/restricted.pdf"))]);
    assert_eq!(codes(&output), Vec::<&str>::new());
    assert_eq!(drawn(&output.pdf, &[FIRST]), [[true]]);
}

#[test]
fn draws_the_template_as_an_artifact_in_tagged_pdf() {
    let document = Document::html(
        "<html lang=en><title>Letter</title>\
         <style>@page { background: url(t.pdf) 0 0 / 100% 100% }</style><p>Dear reader",
    )
    .resource("t.pdf", fixture("pdf/shapes.pdf"))
    .created(CREATED);
    let output = engine().render(&document).expect("tagged");
    assert_eq!(codes(&output), Vec::<&str>::new());
    assert_eq!(drawn(&output.pdf, &[FIRST]), [[true]]);
    // The page's content marks the template as an artifact around the XObject.
    let pdf = Pdf::new(output.pdf.clone()).expect("a PDF");
    let content = pdf.pages()[0].page_stream().expect("content").to_vec();
    let content = String::from_utf8_lossy(&content);
    let artifact = content.find("/Artifact").expect("an artifact");
    let form = content.find(" Do").expect("the template drawn");
    assert!(artifact < form, "{content}");
}

#[test]
fn refuses_pdf_templates_in_pdf_a_and_pdf_ua() {
    for document in [
        Document::html("<style>@page { background: url(t.pdf) }</style><p>Text")
            .conformance(Conformance::PdfA3b),
        // PDF/A-1 is written as PDF 1.4, where krilla stops at the PDF version of the template.
        Document::html("<style>@page { background: url(t.pdf) }</style><p>Text")
            .conformance(Conformance::PdfA1b),
        Document::html(
            "<html lang=en><title>Letter</title><style>@page { background: url(t.pdf) }</style><p>Text",
        )
        .accessible(),
    ] {
        let document = document
            .resource("t.pdf", fixture("pdf/shapes.pdf"))
            .created(CREATED);
        match engine().render(&document) {
            Err(Error::Pdf(PdfError::Conformance { problems, .. })) => {
                assert!(problems.contains("shows a PDF file as an image"), "{problems}");
            }
            other => panic!("not refused: {:?}", other.map(|output| output.pages)),
        }
    }
}

#[test]
fn survives_broken_pdf_files() {
    // The letterhead cut short everywhere, and with bytes changed: each renders, with the page
    // template or without it.
    let letterhead = fixture("letterhead.pdf");
    let html = "<style>@page { background: url(t.pdf) 0 0 / 100% 100% } @page :nth(2) { background-image: url('t.pdf#page=2') }
        p { break-after: page }</style><p>One<p>Two";
    let step = letterhead.len() / 24;
    for end in (9..letterhead.len()).step_by(step) {
        render(html, &[("t.pdf", letterhead[..end].to_vec())]);
    }
    for at in (9..letterhead.len()).step_by(step) {
        let mut changed = letterhead.clone();
        changed[at] ^= 0x5a;
        render(html, &[("t.pdf", changed)]);
    }
}

#[test]
fn leaves_out_pdf_files_that_krilla_cannot_write() {
    // A cross-reference table that hayro-syntax repairs halfway through, after which an object
    // the page refers to is a broken one further down the file: shown without it, without a
    // dangling reference in the PDF (found by the fuzz target `pdf`).
    let output = render("<img src=t.pdf>", &[("t.pdf", fixture("pdf/repaired.pdf"))]);
    assert_eq!(codes(&output), Vec::<&str>::new());
    Pdf::new(output.pdf).expect("a PDF hayro reads");
    // A resource whose value hayro-syntax lists but cannot read (`/Bad#zz`).
    let output = render("<img src=t.pdf>", &[("t.pdf", fixture("pdf/bad-name.pdf"))]);
    assert_eq!(codes(&output), ["image.broken"]);
    // An array whose items hayro-syntax cannot read, which it panics on (found by the fuzz
    // target `merge`).
    let output = render(
        "<img src=t.pdf>",
        &[("t.pdf", fixture("pdf/unreadable-array.pdf"))],
    );
    assert_eq!(codes(&output), ["image.broken"]);
}
