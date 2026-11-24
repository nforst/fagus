//! Features that change the PDF after krilla wrote it, together: fill-in forms and layers (an
//! update before the file is compacted), encryption (while it is compacted) and signatures (an
//! incremental update after it). qpdf and OpenSSL check the files where they are installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use fagus::{Date, Document, Encryption, Engine, InitialView, PageLayout, PageMode, Signature, Zoom};

const CREATED: Date = Date::new(2026, 11, 19, 12, 0, 0);

const FORM: &str = "<!DOCTYPE html><html lang=en><title>Order</title>\
    <p><label>Name <input name=customer value='Ada Lovelace'></label>\
    <p><label><input type=checkbox name=terms checked> I agree</label>\
    <p><select name=country><option value=de>Germany<option value=fr selected>France</select>";

const LAYERS: &str = "<!DOCTYPE html><html lang=en><title>Invoice</title>\
    <p style='-fagus-layer: \"Notes\" screen'>Pay online with the link below.\
    <p style='-fagus-layer: \"Copy\" print'>COPY";

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
        .expect("fonts");
    engine
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/signing")
        .join(name)
}

fn signature() -> Signature {
    let p12 = std::fs::read(fixture("rsa.p12")).expect("fixture");
    Signature::pkcs12(&p12, "fagus-test-only").expect("PKCS#12 file")
}

/// Whether a program runs here; the checks with it are skipped otherwise.
fn installed(program: &str) -> bool {
    let found = Command::new(program).arg("--version").output().is_ok();
    if !found {
        eprintln!("{program} is not installed; the files are not checked with it");
    }
    found
}

/// Writes `pdf` to a file of its own, named for the test.
fn file(name: &str, pdf: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("fagus-interplay-{name}-{}.pdf", std::process::id()));
    std::fs::write(&path, pdf).expect("written");
    path
}

/// qpdf with the arguments and the file: its exit code and output.
fn qpdf(arguments: &[&str], file: &Path) -> (i32, String) {
    let output = Command::new("qpdf")
        .args(arguments)
        .arg(file)
        .output()
        .expect("qpdf runs");
    let text =
        String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    (output.status.code().unwrap_or(-1), text)
}

/// The fields of the form as qpdf reads them: (name, field type, value) of each.
fn fields(file: &Path, password: &str) -> Vec<(String, String, String)> {
    let password = format!("--password={password}");
    let (status, json) = qpdf(&["--json", "--json-key=acroform", &password], file);
    assert_eq!(status, 0, "{json}");
    let json: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    json["acroform"]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|field| {
            // qpdf writes strings with `u:` before them.
            let text = |key: &str| {
                field[key].as_str().map_or_else(
                    || field[key].to_string(),
                    |text| text.strip_prefix("u:").unwrap_or(text).to_owned(),
                )
            };
            (text("fullname"), text("fieldtype"), text("value"))
        })
        .collect()
}

#[test]
fn encrypts_fill_in_forms() {
    let pdf = engine()
        .render(
            &Document::html(FORM)
                .created(CREATED)
                .forms(true)
                .encrypt(Encryption::new().user_password("benutzer")),
        )
        .expect("rendered")
        .pdf;
    let has = |text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
    assert!(has("/Encrypt"));
    assert!(!has("Ada Lovelace"), "the values are encrypted");
    if !installed("qpdf") {
        return;
    }
    let file = file("forms-encrypted", &pdf);
    let (status, output) = qpdf(&["--check", "--password=benutzer"], &file);
    assert_eq!(status, 0, "{output}");
    let fields = fields(&file, "benutzer");
    assert!(
        fields.contains(&("customer".to_owned(), "/Tx".to_owned(), "Ada Lovelace".to_owned())),
        "{fields:?}"
    );
    assert!(
        fields
            .iter()
            .any(|(name, kind, value)| name == "terms" && kind == "/Btn" && value != "/Off")
    );
    assert!(
        fields
            .iter()
            .any(|(name, kind, value)| name == "country" && kind == "/Ch" && value == "fr")
    );
    assert_ne!(qpdf(&["--check", "--password=falsch"], &file).0, 0);
    std::fs::remove_file(file).expect("removed");
}

#[test]
fn signs_fill_in_forms_in_the_same_form() {
    let pdf = engine()
        .render(
            &Document::html(FORM)
                .created(CREATED)
                .forms(true)
                .sign(signature().reason("Bestellung")),
        )
        .expect("rendered")
        .pdf;
    // The signature is an update after the file with the form.
    let unsigned = engine()
        .render(&Document::html(FORM).created(CREATED).forms(true))
        .expect("rendered")
        .pdf;
    assert!(pdf.starts_with(&unsigned));
    let file = file("forms-signed", &pdf);
    if installed("qpdf") {
        let (status, output) = qpdf(&["--check"], &file);
        assert_eq!(status, 0, "{output}");
        let fields = fields(&file, "");
        let kinds: Vec<&str> = fields.iter().map(|(_, kind, _)| kind.as_str()).collect();
        assert!(kinds.contains(&"/Sig"), "{fields:?}");
        assert!(
            fields.contains(&("customer".to_owned(), "/Tx".to_owned(), "Ada Lovelace".to_owned())),
            "{fields:?}"
        );
        assert!(fields.iter().any(|(name, _, _)| name == "terms"));
        assert!(fields.iter().any(|(name, _, _)| name == "country"));
        // The signature field took a name no field of the form has.
        let mut names: Vec<&str> = fields.iter().map(|(name, _, _)| name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), fields.len(), "{fields:?}");
    }
    if installed("openssl") && installed("python3") {
        let verified = Command::new("python3")
            .arg(fixture("verify.py"))
            .arg(&file)
            .output()
            .expect("verify.py runs");
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stdout)
        );
    }
    std::fs::remove_file(file).expect("removed");
}

#[test]
fn encrypts_layers() {
    let pdf = engine()
        .render(
            &Document::html(LAYERS)
                .created(CREATED)
                .encrypt(Encryption::new().user_password("benutzer")),
        )
        .expect("rendered")
        .pdf;
    let has = |text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
    assert!(has("/OCProperties"), "the layers stay in the catalog");
    assert!(!has("(Notes)"), "their names are encrypted");
    if !installed("qpdf") {
        return;
    }
    let file = file("layers-encrypted", &pdf);
    let (status, output) = qpdf(&["--check", "--password=benutzer"], &file);
    assert_eq!(status, 0, "{output}");
    let (status, json) = qpdf(
        &[
            "--json",
            "--json-key=qpdf",
            "--password=benutzer",
            "--decode-level=all",
        ],
        &file,
    );
    assert_eq!(status, 0);
    assert!(json.contains("\"/Name\": \"u:Notes\""), "the names decrypt");
    assert!(json.contains("\"/Name\": \"u:Copy\""));
    std::fs::remove_file(file).expect("removed");
}

#[test]
fn signs_an_encrypted_form() {
    let pdf = engine()
        .render(
            &Document::html(FORM)
                .created(CREATED)
                .forms(true)
                .encrypt(Encryption::new().user_password("benutzer"))
                .sign(signature().reason("Bestellung")),
        )
        .expect("rendered")
        .pdf;
    let has = |text: &str| pdf.windows(text.len()).any(|window| window == text.as_bytes());
    assert!(
        !has("Ada Lovelace") && !has("Bestellung"),
        "the values are encrypted"
    );
    let file = file("forms-encrypted-signed", &pdf);
    if installed("qpdf") {
        let (status, output) = qpdf(&["--check", "--password=benutzer"], &file);
        assert_eq!(status, 0, "{output}");
        let fields = fields(&file, "benutzer");
        assert!(
            fields.contains(&("customer".to_owned(), "/Tx".to_owned(), "Ada Lovelace".to_owned())),
            "{fields:?}"
        );
        assert!(
            fields
                .iter()
                .any(|(name, kind, _)| name == "Signature1" && kind == "/Sig"),
            "{fields:?}"
        );
    }
    if installed("openssl") && installed("python3") {
        let verified = Command::new("python3")
            .arg(fixture("verify.py"))
            .arg(&file)
            .output()
            .expect("verify.py runs");
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stdout)
        );
    }
    std::fs::remove_file(file).expect("removed");
}

/// The rectangles of the widgets of the form's fields as qpdf reads them, by field name.
fn widgets(file: &Path, password: &str) -> Vec<(String, Vec<f64>)> {
    let password = format!("--password={password}");
    let (status, json) = qpdf(&["--json", "--json-key=acroform", &password], file);
    assert_eq!(status, 0, "{json}");
    let json: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    let mut found = Vec::new();
    for field in json["acroform"]["fields"].as_array().expect("fields") {
        let name = field["fullname"].as_str().unwrap_or_default();
        let name = name.strip_prefix("u:").unwrap_or(name).to_owned();
        let object = field["annotation"]["object"].as_str().expect("annotation");
        let number = object.split_whitespace().next().expect("number");
        let (status, dictionary) = qpdf(&[&password, &format!("--show-object={number}")], file);
        assert_eq!(status, 0, "{dictionary}");
        let rect = dictionary
            .split("/Rect [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("rectangle")
            .split_whitespace()
            .map(|number| number.parse().expect("number"))
            .collect();
        found.push((name, rect));
    }
    found
}

const SIGNED_LETTER: &str = "<!DOCTYPE html><html lang=de><title>Vertrag</title>\
    <style>body { margin: 0 } #signature { margin: 40px; width: 240px; height: 60px; border: 1px solid }</style>\
    <p>Vertrag</p><div id=signature>Unterschrieben: Max Mustermann</div>\
    <div id=hidden style='display: none'>x</div>";

#[test]
fn signs_visibly_in_the_box_of_an_element() {
    for (encryption, password) in [
        (None, ""),
        (Some(Encryption::new().user_password("benutzer")), "benutzer"),
    ] {
        let mut document = Document::html(SIGNED_LETTER)
            .created(CREATED)
            .sign(signature().visible_on("signature").reason("Vertrag"));
        if let Some(encryption) = encryption {
            document = document.encrypt(encryption);
        }
        let output = engine().render(&document.report_layout(true)).expect("rendered");
        let layout = output.layout.expect("layout");
        let placed = layout.element("signature").expect("element").boxes[0];
        let pdf = output.pdf;
        let file = file("visible-element", &pdf);
        if installed("qpdf") {
            let (status, output) = qpdf(&["--check", &format!("--password={password}")], &file);
            assert_eq!(status, 0, "{output}");
            let fields = fields(&file, password);
            assert_eq!(fields.len(), 1, "{fields:?}");
            assert_eq!(
                (fields[0].0.as_str(), fields[0].1.as_str()),
                ("Signature1", "/Sig")
            );
            assert_ne!(fields[0].2, "null", "signed");
            let widgets = widgets(&file, password);
            assert_eq!(widgets.len(), 1, "{widgets:?}");
            // The border box of the element, in points from the bottom left corner.
            let rect = &widgets[0].1;
            let close = |a: f64, b: f32| (a - f64::from(b)).abs() < 0.1;
            assert!(close(rect[2] - rect[0], 242.0 * 0.75), "{rect:?}");
            assert!(close(rect[3] - rect[1], 62.0 * 0.75), "{rect:?}");
            assert!(close(rect[0], placed.x), "{rect:?} {placed:?}");
            assert!(close(rect[3], 841.92 - placed.y), "{rect:?} {placed:?}");
        }
        if installed("openssl") && installed("python3") {
            let verified = Command::new("python3")
                .arg(fixture("verify.py"))
                .arg(&file)
                .output()
                .expect("verify.py runs");
            assert!(
                verified.status.success(),
                "{}",
                String::from_utf8_lossy(&verified.stdout)
            );
        }
        std::fs::remove_file(file).expect("removed");
    }
}

#[test]
fn signs_visibly_beside_a_form() {
    let pdf = engine()
        .render(
            &Document::html(format!("{FORM}<div id=signature>Signed</div>"))
                .created(CREATED)
                .forms(true)
                .accessible()
                .sign(signature().visible_on("signature")),
        )
        .expect("rendered")
        .pdf;
    if !installed("qpdf") {
        return;
    }
    let file = file("visible-form", &pdf);
    let (status, output) = qpdf(&["--check"], &file);
    assert_eq!(status, 0, "{output}");
    let fields = fields(&file, "");
    let names: Vec<&str> = fields.iter().map(|(name, ..)| name.as_str()).collect();
    assert!(
        names.contains(&"customer") && names.contains(&"Signature1"),
        "{fields:?}"
    );
    assert_eq!(fields.len(), 4, "the form's fields and the signature's");
    std::fs::remove_file(file).expect("removed");
}

#[test]
fn signs_visibly_in_an_area() {
    let pdf = engine()
        .render(
            &Document::html(SIGNED_LETTER)
                .created(CREATED)
                .sign(signature().visible_at(1, 100.0, 200.0, 150.0, 50.0)),
        )
        .expect("rendered")
        .pdf;
    if !installed("qpdf") {
        return;
    }
    let file = file("visible-area", &pdf);
    let (status, output) = qpdf(&["--check"], &file);
    assert_eq!(status, 0, "{output}");
    let widgets = widgets(&file, "");
    assert_eq!(widgets.len(), 1, "{widgets:?}");
    // In points from the bottom of the page, which is 841.92 points tall (A4).
    let rect = &widgets[0].1;
    let close = |a: f64, b: f64| (a - b).abs() < 0.01;
    assert!(close(rect[0], 100.0) && close(rect[2], 250.0), "{rect:?}");
    assert!(
        close(rect[3], 841.92 - 200.0) && close(rect[1], 841.92 - 250.0),
        "{rect:?}"
    );
    std::fs::remove_file(file).expect("removed");
}

#[test]
fn says_why_a_signature_cannot_be_visible() {
    let render = |signature: Signature| {
        engine()
            .render(&Document::html(SIGNED_LETTER).sign(signature))
            .expect_err("refused")
            .to_string()
    };
    assert_eq!(
        render(signature().visible_on("unterschrift")),
        "the PDF could not be signed: the document has no element with the id `unterschrift` for the signature"
    );
    assert!(render(signature().visible_on("hidden")).contains("has no box to show it in"));
    assert!(render(signature().visible_at(2, 0.0, 0.0, 10.0, 10.0)).contains("(page 2 of 1)"));
}

/// The catalog of a PDF as qpdf reads it, as JSON.
fn catalog(file: &Path, password: &str) -> String {
    let output = Command::new("qpdf")
        .arg(format!("--password={password}"))
        .args(["--json", "--json-key=qpdf", "--json-object=trailer"])
        .arg(file)
        .output()
        .expect("qpdf runs");
    let json = String::from_utf8_lossy(&output.stdout).into_owned();
    let root = json.split("\"/Root\": \"").nth(1).expect("a catalog");
    let root = &root[..root.find('"').expect("a reference")];
    let output = Command::new("qpdf")
        .arg(format!("--password={password}"))
        .args(["--json", "--json-key=qpdf"])
        .arg(format!(
            "--json-object={}",
            root.trim_end_matches(" R").replace(' ', ",")
        ))
        .arg(file)
        .output()
        .expect("qpdf runs");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn opens_encrypted_and_signed_pdfs_as_told() {
    let view = InitialView::new()
        .page_mode(PageMode::Outlines)
        .page_layout(PageLayout::OneColumn)
        .zoom(Zoom::FitWidth)
        .fit_window(true);
    let seeded = Encryption::new()
        .user_password("user")
        .owner_password("owner")
        .seed([7; 32]);
    for (name, encryption, signed, password) in [
        ("view-encrypted", Some(seeded.clone()), false, "user"),
        ("view-signed", None, true, ""),
        ("view-encrypted-signed", Some(seeded), true, "user"),
    ] {
        let mut document = Document::html(SIGNED_LETTER)
            .created(CREATED)
            .initial_view(view.clone());
        if let Some(encryption) = encryption {
            document = document.encrypt(encryption);
        }
        if signed {
            document = document.sign(signature());
        }
        let output = engine().render(&document).expect("rendered");
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        if !installed("qpdf") {
            continue;
        }
        let file = file(name, &output.pdf);
        let (status, text) = qpdf(&[&format!("--password={password}"), "--check"], &file);
        assert_eq!(status, 0, "{name}: {text}");
        let catalog = catalog(&file, password);
        for entry in [
            "\"/PageMode\": \"/UseOutlines\"",
            "\"/PageLayout\": \"/OneColumn\"",
            "\"/DisplayDocTitle\": true",
            "\"/FitWindow\": true",
            "\"/FitH\", 841.92 ]",
        ] {
            assert!(catalog.contains(entry), "{name}: {entry} in {catalog}");
        }
        std::fs::remove_file(file).expect("removed");
    }
}
