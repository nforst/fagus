//! Merging PDFs: the pages of PDFs Fagus wrote (also PDF/A, Factur-X, tagged, signed and with
//! forms) and of PDFs other programs wrote, one after another, read back with hayro-syntax (and
//! with qpdf and pdftotext where they are installed). What a merge takes over of a page and what
//! it leaves out, and the errors of PDFs it cannot merge.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use fagus::{
    Conformance, Date, Document, Encryption, Engine, Error, Limits, Merge, PageRanges, PdfError, Signature,
};
use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, Object};
use hayro_syntax::page::Rotation;

const CREATED: Date = Date::new(2026, 11, 27, 12, 0, 0);

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
        .expect("fonts");
    engine
}

fn render(document: Document) -> Vec<u8> {
    engine().render(&document.created(CREATED)).expect("rendered").pdf
}

/// Pages of `text`, each its own page, of the size `size` (a value of `@page { size }`).
fn pages(size: &str, texts: &[&str]) -> Document {
    let body = texts
        .iter()
        .map(|text| format!("<p style='break-after: page'>{text}</p>"))
        .collect::<Vec<_>>()
        .concat();
    Document::html(format!(
        "<!doctype html><html lang=de><title>{}</title><style>@page {{ size: {size}; margin: 20mm }}</style>{body}",
        texts[0]
    ))
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../tests/corpus/images/media/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

/// A PDF file of `objects` (numbered from 1, the first the catalog), with a cross-reference
/// table.
fn handmade(objects: &[&str]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        out.extend(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A content stream object that writes `text` in Helvetica.
fn text_stream(text: &str) -> String {
    let content = format!("BT /F1 12 Tf 20 20 Td ({text}) Tj ET");
    format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len())
}

/// A PDF another program could have written: three pages, the first upright, the second turned
/// with `/Rotate 90` and cropped, the third landscape and turned by 270 degrees, with the media
/// box inherited from the page tree; a link on the first page, an outline and a form field.
fn turned() -> Vec<u8> {
    handmade(&[
        "<< /Type /Catalog /Pages 2 0 R /Outlines 11 0 R /AcroForm << /Fields [10 0 R] >> >>",
        "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 /MediaBox [0 0 200 300] \
         /Resources << /Font << /F1 9 0 R >> >> >>",
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Annots [12 0 R 10 0 R] >>",
        "<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Rotate 90 /CropBox [10 20 190 280] \
         /TrimBox [10 20 190 280] >>",
        "<< /Type /Page /Parent 2 0 R /Contents 8 0 R /MediaBox [0 0 400 250] /Rotate -90 >>",
        &text_stream("Erste Seite"),
        &text_stream("Zweite Seite"),
        &text_stream("Dritte Seite"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        "<< /FT /Tx /T (name) /Type /Annot /Subtype /Widget /Rect [20 100 180 120] /P 3 0 R >>",
        "<< /Type /Outlines /First 13 0 R /Last 13 0 R /Count 1 >>",
        "<< /Type /Annot /Subtype /Link /Rect [20 20 100 40] /A << /S /URI /URI (https://example.com/) >> >>",
        "<< /Title (Anfang) /Parent 11 0 R /Dest [3 0 R /Fit] >>",
    ])
}

/// The media box, crop box and rotation (in degrees, clockwise) of each page of `pdf`.
fn boxes(pdf: &[u8]) -> Vec<([f64; 4], [f64; 4], u16)> {
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    pdf.pages()
        .iter()
        .map(|page| {
            let rect = |rect: hayro_syntax::object::Rect| [rect.x0, rect.y0, rect.x1, rect.y1];
            let degrees = match page.rotation() {
                Rotation::None => 0,
                Rotation::Horizontal => 90,
                Rotation::Flipped => 180,
                Rotation::FlippedHorizontal => 270,
            };
            (rect(page.media_box()), rect(page.crop_box()), degrees)
        })
        .collect()
}

/// How wide and high each page of `pdf` shows, in points, turned as viewers turn it.
fn sizes(pdf: &[u8]) -> Vec<(f32, f32)> {
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    pdf.pages()
        .iter()
        .map(|page| {
            let (width, height) = page.render_dimensions();
            ((width * 100.0).round() / 100.0, (height * 100.0).round() / 100.0)
        })
        .collect()
}

/// The keys of the document catalog of `pdf`.
fn catalog_keys(pdf: &[u8]) -> Vec<String> {
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    let catalog: Dict<'_> = pdf.xref().get(pdf.xref().root_id()).expect("a catalog");
    catalog
        .keys()
        .map(|key| String::from_utf8_lossy(key.as_ref()).into_owned())
        .collect()
}

/// Whether a page of `pdf` has annotations.
fn annotated(pdf: &[u8]) -> bool {
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    pdf.pages()
        .iter()
        .any(|page| page.raw().get::<Array<'_>>(b"Annots").is_some())
}

/// Whether `text` is in the objects of `pdf`, those in object streams too: its dictionary keys,
/// names and strings and the decoded data of its streams (the XMP metadata, attached files).
fn contains(pdf: &[u8], text: &str) -> bool {
    fn dump(object: &Object<'_>, out: &mut Vec<u8>, depth: u32) {
        if depth > 32 {
            return;
        }
        let dict = |dict: &Dict<'_>, out: &mut Vec<u8>| {
            for key in dict.keys() {
                out.push(b'/');
                out.extend_from_slice(key.as_ref());
                out.push(b' ');
                if let Some(MaybeRef::NotRef(value)) = dict.get_raw::<Object<'_>>(key.as_ref()) {
                    dump(&value, out, depth + 1);
                }
            }
        };
        match object {
            Object::Name(name) => {
                out.push(b'/');
                out.extend_from_slice(name.as_ref());
            }
            Object::String(string) => {
                out.push(b'(');
                out.extend_from_slice(string.as_ref());
                out.push(b')');
            }
            Object::Dict(entries) => dict(entries, out),
            Object::Array(items) => {
                for item in items.raw_iter() {
                    if let MaybeRef::NotRef(item) = item {
                        dump(&item, out, depth + 1);
                    }
                }
            }
            Object::Stream(stream) => {
                dict(stream.dict(), out);
                out.extend_from_slice(&stream.decoded().map(|data| data.to_vec()).unwrap_or_default());
            }
            _ => {}
        }
        out.push(b' ');
    }
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    let mut out = Vec::new();
    for object in pdf.objects() {
        dump(&object, &mut out, 0);
    }
    out.windows(text.len()).any(|window| window == text.as_bytes())
}

/// A file of its own in the temporary directory, removed when dropped.
struct Temporary(PathBuf);

impl Temporary {
    fn new(data: &[u8]) -> Self {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fagus-merge-{}-{}.pdf",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, data).expect("written");
        Self(path)
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The text of each page of `pdf` as pdftotext extracts it, unless pdftotext is not installed.
fn text(pdf: &[u8]) -> Option<Vec<String>> {
    let file = Temporary::new(pdf);
    let Ok(output) = Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(&file.0)
        .arg("-")
        .output()
    else {
        eprintln!("pdftotext is not installed; the text is not checked");
        return None;
    };
    assert!(output.status.success(), "pdftotext failed");
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    let mut pages: Vec<String> = text
        .split('\u{c}')
        .map(|page| page.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    // After the last page.
    if pages.last().is_some_and(String::is_empty) {
        pages.pop();
    }
    Some(pages)
}

/// Checks `pdf` with `qpdf --check`, unless qpdf is not installed.
fn qpdf_check(pdf: &[u8]) {
    let file = Temporary::new(pdf);
    let Ok(output) = Command::new("qpdf").arg("--check").arg(&file.0).output() else {
        eprintln!("qpdf is not installed; the merged PDFs are not checked with it");
        return;
    };
    let report =
        String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && report.contains("No syntax or stream encoding errors"),
        "qpdf --check: {report}"
    );
    assert!(!report.contains("WARNING"), "qpdf --check: {report}");
}

fn merged(merge: &Merge) -> Vec<u8> {
    let output = merge.write().expect("merged");
    assert_eq!(output.pages, sizes(&output.pdf).len());
    qpdf_check(&output.pdf);
    output.pdf
}

#[test]
fn puts_the_pages_of_pdfs_one_after_another() {
    let invoice = render(pages("A4", &["Rechnung 1", "Rechnung 1 Seite 2"]));
    let reminder = render(pages("A5 landscape", &["Mahnung 2"]));
    let letter = render(pages("letter", &["Brief 3"]));
    let output = fagus::merge([invoice.clone(), reminder.clone(), letter]).expect("merged");
    assert_eq!(output.pages, 4);
    assert!(output.timings.pdf > std::time::Duration::ZERO);
    assert_eq!(
        sizes(&output.pdf),
        [
            (594.96, 841.92),
            (594.96, 841.92),
            (594.96, 420.0),
            (612.0, 792.0)
        ]
    );
    qpdf_check(&output.pdf);
    if let Some(text) = text(&output.pdf) {
        assert_eq!(text, ["Rechnung 1", "Rechnung 1 Seite 2", "Mahnung 2", "Brief 3"]);
    }
    // The same PDF twice, and pages of it in another order and more than once.
    let merge = Merge::new()
        .add_pages(invoice.clone(), "2,1".parse().expect("pages"))
        .add(reminder)
        .add_pages(invoice, PageRanges::parse("2-, 2").expect("pages"));
    let pdf = merged(&merge);
    if let Some(text) = text(&pdf) {
        assert_eq!(
            text,
            [
                "Rechnung 1 Seite 2",
                "Rechnung 1",
                "Mahnung 2",
                "Rechnung 1 Seite 2",
                "Rechnung 1 Seite 2"
            ]
        );
    }
}

#[test]
fn reads_a_pdf_added_more_than_once_once() {
    // The same terms after each invoice: one PDF, shared, gives the PDF that copies of it give.
    let terms = std::sync::Arc::new(render(pages("A4", &["AGB 1", "AGB 2"])));
    let invoices: Vec<Vec<u8>> = (1..=3)
        .map(|number| render(pages("A4", &[&format!("Rechnung {number}")])))
        .collect();
    let mut shared = Merge::new();
    let mut copied = Merge::new();
    for invoice in &invoices {
        shared = shared
            .add(invoice.clone())
            .add_pages(std::sync::Arc::clone(&terms), "2".parse().expect("pages"));
        copied = copied
            .add(invoice.clone())
            .add_pages(terms.to_vec(), "2".parse().expect("pages"));
    }
    let pdf = merged(&shared);
    assert_eq!(pdf, merged(&copied));
    if let Some(text) = text(&pdf) {
        assert_eq!(
            text,
            [
                "Rechnung 1",
                "AGB 2",
                "Rechnung 2",
                "AGB 2",
                "Rechnung 3",
                "AGB 2"
            ]
        );
    }
}

#[test]
fn keeps_the_size_crop_box_and_rotation_of_pages() {
    let pdf = merged(&Merge::new().add(turned()).add(turned()));
    let original = boxes(&turned());
    let boxes = boxes(&pdf);
    assert_eq!(boxes.len(), 6);
    assert_eq!(boxes[..3], original[..]);
    assert_eq!(boxes[3..], original[..]);
    assert_eq!(
        boxes[..3],
        [
            ([0.0, 0.0, 200.0, 300.0], [0.0, 0.0, 200.0, 300.0], 0),
            ([0.0, 0.0, 200.0, 300.0], [10.0, 20.0, 190.0, 280.0], 90),
            ([0.0, 0.0, 400.0, 250.0], [0.0, 0.0, 400.0, 250.0], 270),
        ]
    );
    assert_eq!(sizes(&pdf)[..3], [(200.0, 300.0), (260.0, 180.0), (250.0, 400.0)]);
    if let Some(text) = text(&pdf) {
        assert_eq!(text[..3], ["Erste Seite", "Zweite Seite", "Dritte Seite"]);
    }
    // The trim box is left out, as are the link, the field, the outline and the form.
    assert!(contains(&turned(), "/TrimBox"));
    assert!(!contains(&pdf, "/TrimBox"));
    assert!(!annotated(&pdf));
    let keys = catalog_keys(&pdf);
    assert!(
        !keys.iter().any(|key| key == "Outlines" || key == "AcroForm"),
        "{keys:?}"
    );
}

#[test]
fn merges_pdfa_and_factur_x_into_a_pdf_without_their_attachments() {
    let xml = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/corpus/invoice/factur-x.xml"
    ))
    .expect("invoice XML");
    let invoice = render(
        pages("A4", &["Rechnung mit XML"])
            .conformance(Conformance::PdfA3b)
            .factur_x(xml),
    );
    assert!(contains(&invoice, "factur-x.xml"));
    assert!(contains(&invoice, "pdfaid"));
    let archived = render(pages("A4", &["Archiv 1b"]).conformance(Conformance::PdfA1b));
    let modern = render(pages("A4", &["Archiv 4"]).conformance(Conformance::PdfA4));
    assert!(modern.starts_with(b"%PDF-2.0"));

    let output = Merge::new()
        .add(invoice.clone())
        .add(archived.clone())
        .write()
        .expect("merged");
    assert!(output.pdf.starts_with(b"%PDF-1.7"), "PDF 1.7 for PDFs up to 1.7");
    qpdf_check(&output.pdf);
    let keys = catalog_keys(&output.pdf);
    for key in ["Names", "AF", "OutputIntents", "StructTreeRoot", "MarkInfo"] {
        assert!(!keys.iter().any(|found| found == key), "{key} in {keys:?}");
    }
    assert!(!contains(&output.pdf, "factur-x.xml"));
    assert!(!contains(&output.pdf, "pdfaid"));
    if let Some(text) = text(&output.pdf) {
        assert_eq!(text, ["Rechnung mit XML", "Archiv 1b"]);
    }

    // A PDF 2.0 input makes the merged PDF PDF 2.0.
    let output = Merge::new().add(invoice).add(modern).write().expect("merged");
    assert!(output.pdf.starts_with(b"%PDF-2.0"));
    qpdf_check(&output.pdf);
    if let Some(text) = text(&output.pdf) {
        assert_eq!(text, ["Rechnung mit XML", "Archiv 4"]);
    }
}

#[test]
fn leaves_out_links_tags_outlines_forms_and_signatures() {
    let html = "<!doctype html><html lang=de><title>Alles</title>\
        <h1>Kapitel</h1><p><a href='https://example.com/'>Link</a> <a href='#zwei'>intern</a></p>\
        <form><input name=kunde value=Muster></form>\
        <h2 id=zwei style='break-before: page'>Zwei</h2><div id=unterschrift style='height: 40px'>Unterschrift</div>";
    let tagged = render(Document::html(html).forms(true).accessible());
    let p12 = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/signing/rsa.p12"
    ))
    .expect("PKCS#12 file");
    let signature = Signature::pkcs12(&p12, "fagus-test-only")
        .expect("PKCS#12 file")
        .visible_on("unterschrift");
    let signed = render(Document::html(html).sign(signature));
    for pdf in [&tagged, &signed] {
        let keys = catalog_keys(pdf);
        assert!(keys.iter().any(|key| key == "AcroForm"), "{keys:?}");
        assert!(annotated(pdf));
    }
    assert!(catalog_keys(&tagged).iter().any(|key| key == "StructTreeRoot"));
    assert!(catalog_keys(&tagged).iter().any(|key| key == "Outlines"));
    assert!(contains(&tagged, "/StructParents"));
    assert!(contains(&signed, "/Sig"));

    let pdf = merged(&Merge::new().add(tagged).add(signed));
    assert_eq!(sizes(&pdf).len(), 4);
    assert!(!annotated(&pdf));
    let keys = catalog_keys(&pdf);
    for key in [
        "AcroForm",
        "StructTreeRoot",
        "Outlines",
        "Names",
        "Dests",
        "MarkInfo",
        "Perms",
    ] {
        assert!(!keys.iter().any(|found| found == key), "{key} in {keys:?}");
    }
    assert!(!contains(&pdf, "/Sig"));
    assert!(!contains(&pdf, "/StructParents"));
    // What the pages show stays, the signature's appearance aside, which is an annotation.
    if let Some(text) = text(&pdf) {
        assert!(
            text[0].contains("Kapitel") && text[0].contains("Link"),
            "{text:?}"
        );
        assert!(
            text[1].contains("Zwei") && text[1].contains("Unterschrift"),
            "{text:?}"
        );
    }
}

#[test]
fn merges_pdfs_of_other_programs() {
    // Written by hand (PDF 1.4), one with a cross-reference table that needs repair, and
    // Fagus's letterhead.
    let pdf = merged(
        &Merge::new()
            .add(fixture("pdf/shapes.pdf"))
            .add(fixture("pdf/repaired.pdf"))
            .add_pages(fixture("letterhead.pdf"), PageRanges::range(2, 2).expect("pages"))
            .add(turned()),
    );
    assert_eq!(sizes(&pdf).len(), 2 + 1 + 1 + 3);
    assert_eq!(sizes(&pdf)[..3], [(180.0, 80.0), (25.0, 50.0), (100.0, 100.0)]);
}

#[test]
fn writes_the_metadata_of_the_merge_and_the_same_bytes_each_time() {
    let first = render(pages("A4", &["Eins"]));
    let second = render(pages("A4", &["Zwei"]));
    let merge = Merge::new().add(first.clone()).add(second.clone());
    let once = merged(&merge);
    assert_eq!(once, merge.write().expect("merged").pdf);
    // Nothing of the inputs' metadata (they have titles), and no date without one.
    let metadata = Pdf::new(once.clone())
        .expect("a PDF hayro reads")
        .metadata()
        .clone();
    assert_eq!(metadata.title, None);
    assert_eq!(metadata.creation_date, None);
    assert_eq!(metadata.producer.as_deref(), Some(&b"Fagus"[..]));
    assert!(!contains(&once, "/CreationDate"));
    assert!(!contains(&once, "/DisplayDocTitle"));
    assert!(!contains(&once, "/Lang"));

    let merge = merge
        .title("Sammeldruck")
        .author("Buchhaltung")
        .author("Fagus")
        .language("de-DE")
        .created(CREATED);
    let described = merged(&merge);
    assert_eq!(described, merge.write().expect("merged").pdf);
    let pdf = Pdf::new(described).expect("a PDF hayro reads");
    let metadata = pdf.metadata();
    assert_eq!(metadata.title.as_deref(), Some(&b"Sammeldruck"[..]));
    assert_eq!(metadata.author.as_deref(), Some(&b"Buchhaltung, Fagus"[..]));
    assert!(metadata.creation_date.is_some());
    let catalog: Dict<'_> = pdf.xref().get(pdf.xref().root_id()).expect("a catalog");
    assert_eq!(
        catalog
            .get::<Dict<'_>>(b"ViewerPreferences")
            .and_then(|preferences| preferences.get::<bool>(b"DisplayDocTitle")),
        Some(true)
    );
    assert!(catalog.get::<Name<'_>>(b"PageMode").is_none());
    assert_eq!(
        catalog
            .get::<hayro_syntax::object::String<'_>>(b"Lang")
            .map(|language| language.as_ref().to_vec()),
        Some(b"de-DE".to_vec())
    );
}

#[test]
fn leaves_rendered_pdfs_as_they_were() {
    // A render does not change for the merge: the same bytes before and after one.
    let document = pages("A4", &["Rechnung"]);
    let before = render(document.clone());
    let _ = fagus::merge([before.clone(), before.clone()]).expect("merged");
    assert_eq!(render(document), before);
}

#[test]
fn says_which_pdf_it_cannot_merge_and_why() {
    let good = render(pages("A4", &["Eins", "Zwei"]));
    let error = |merge: Merge| match merge.write() {
        Err(Error::Pdf(error)) => error,
        Err(error) => panic!("another error: {error}"),
        Ok(_) => panic!("merged"),
    };

    assert!(matches!(error(Merge::new()), PdfError::NothingToMerge));
    assert_eq!(
        fagus::merge(Vec::<Vec<u8>>::new())
            .expect_err("nothing")
            .to_string(),
        "there are no PDF files to merge"
    );

    let not_pdf = error(Merge::new().add(good.clone()).add(b"<html>".to_vec()));
    assert!(
        matches!(not_pdf, PdfError::Unreadable { input: 1, .. }),
        "{not_pdf:?}"
    );
    assert_eq!(
        not_pdf.to_string(),
        "the PDF at index 1 cannot be read: it is not a PDF file (it has no `%PDF-` header)"
    );
    let broken = error(Merge::new().add(b"%PDF-1.7\nnothing else".to_vec()));
    assert!(
        matches!(broken, PdfError::Unreadable { input: 0, .. }),
        "{broken:?}"
    );
    // Cut off in the middle: hayro-syntax repairs what it can, the rest is refused, never a
    // panic.
    for length in (0..good.len()).step_by(good.len() / 40 + 1) {
        match Merge::new().add(good[..length].to_vec()).write() {
            Ok(output) => assert!(output.pages <= 2),
            Err(Error::Pdf(PdfError::Unreadable { input: 0, .. })) => {}
            Err(error) => panic!("cut at {length}: {error}"),
        }
    }

    // With a password to open it, and with one that only restricts it.
    for name in ["pdf/locked.pdf", "pdf/restricted.pdf"] {
        let encrypted = error(Merge::new().add(good.clone()).add(fixture(name)));
        assert!(
            matches!(encrypted, PdfError::Encrypted { input: 1 }),
            "{name}: {encrypted:?}"
        );
        assert_eq!(
            encrypted.to_string(),
            "the PDF at index 1 is encrypted; only PDFs without encryption can be merged"
        );
    }
    let owner_only = engine()
        .render(
            &pages("A4", &["Geschützt"])
                .encrypt(Encryption::new().owner_password("eigentümer").permissions([])),
        )
        .expect("rendered")
        .pdf;
    assert!(matches!(
        error(Merge::new().add(owner_only)),
        PdfError::Encrypted { input: 0 }
    ));

    let missing = error(Merge::new().add_pages(good.clone(), "1,3".parse().expect("pages")));
    assert!(matches!(
        missing,
        PdfError::NoPage {
            input: 0,
            page: 3,
            pages: 2
        }
    ));
    assert_eq!(missing.to_string(), "the PDF at index 0 has 2 pages, not page 3");
    let single = render(pages("A4", &["Eins"]));
    assert_eq!(
        error(
            Merge::new()
                .add(good)
                .add_pages(single, "2-".parse().expect("pages"))
        )
        .to_string(),
        "the PDF at index 1 has 1 page, not page 2"
    );

    let invalid = "1-x".parse::<PageRanges>().expect_err("invalid");
    assert_eq!(
        invalid.to_string(),
        "invalid pages `1-x`: expected page numbers and ranges like `1-3,5,8-`"
    );
}

#[test]
fn ends_a_merge_whose_budget_runs_out() {
    let pdf = render(pages("A4", &["Eins"]));
    let result = Merge::new()
        .add(pdf.clone())
        .add(pdf)
        .limits(Limits::new().memory(Some(1024)))
        .write();
    assert!(
        matches!(result, Err(Error::MemoryBudget { budget: 1024, .. })),
        "{result:?}"
    );
}
