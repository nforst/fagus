//! The memory budget and the time limit of a document: small pathological documents end with
//! `Error::MemoryBudget` or `Error::TimeLimit` and a diagnostic, and documents within them render
//! to the same bytes as without them.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fagus::{Date, Document, Engine, Error, Failure, Limits, Output, ResourceLoader};

const MIB: u64 = 1 << 20;

fn render(document: &Document, limits: Limits) -> Result<Output, Failure> {
    Engine::new().render_with_diagnostics(
        &document
            .clone()
            .limits(limits)
            .created(Date::from_unix(1_793_620_800)),
    )
}

/// What held the most memory when the budget ran out, the largest first.
fn exceeded_memory(result: Result<Output, Failure>) -> Vec<&'static str> {
    let failure = result.expect_err("over the budget");
    assert_eq!(failure.diagnostics.len(), 1, "{:?}", failure.diagnostics);
    assert_eq!(failure.diagnostics[0].code, "limit.memory");
    assert_eq!(failure.diagnostics[0].severity, fagus::Severity::Error);
    match failure.error {
        Error::MemoryBudget { held, .. } => held.into_iter().map(|(kind, _)| kind).collect(),
        other => panic!("not the memory budget: {other}"),
    }
}

/// A PNG of `width` × `height` pixels of one color, a few kilobytes however large.
fn png(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let sum = crc(&out[start..]);
        out.extend_from_slice(&sum.to_be_bytes());
    }
    let mut row = vec![0];
    for _ in 0..width {
        row.extend_from_slice(&color);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    for _ in 0..height {
        encoder.write_all(&row).expect("compressed");
    }
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &encoder.finish().expect("compressed"));
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn ends_a_document_with_an_image_too_large_to_decode() {
    // 36 megapixels, 144 MiB decoded, from a file of a few kilobytes.
    let document = Document::html("<img src='big.png' style='width: 50px'>")
        .resource("big.png", png(6000, 6000, [0, 90, 200]));
    let held = exceeded_memory(render(&document, Limits::new().memory(Some(64 * MIB))));
    assert_eq!(held[0], "decoded images");
}

#[test]
fn ends_a_document_with_too_many_images_decoded_at_once() {
    // PDF/A-1 decodes images itself (to drop their alpha) and holds the pixels until the PDF is
    // written: six images of 2.25 megapixels, 9 MiB each, on two pages. The budget runs out
    // while the first page is written.
    let mut html = String::new();
    for index in 0..6 {
        write!(html, "<img src='big{index}.png' style='width: 50px'>").expect("written");
        if index == 2 {
            html.push_str("<div style='break-before: page'></div>");
        }
    }
    let mut document = Document::html(html).conformance(fagus::Conformance::PdfA1b);
    for index in 0..6u8 {
        document = document.resource(format!("big{index}.png"), png(1500, 1500, [index * 30, 90, 200]));
    }
    let held = exceeded_memory(render(&document, Limits::new().memory(Some(24 * MIB))));
    assert_eq!(held[0], "decoded images");
    // With room for them, it renders.
    assert_eq!(render(&document, Limits::new()).expect("rendered").pages, 2);
}

#[test]
fn ends_a_document_whose_filter_nests_many_groups() {
    // A box with an SVG filter that CSS cannot express is rasterized at 300 ppi; each group with
    // opacity inside it is drawn into a pixmap of its own, all of them alive at once.
    let depth = 40;
    let html = format!(
        "<svg width=0 height=0><filter id=f><feMorphology radius='1'/></filter></svg>\
         <div style='filter: url(#f); width: 200px; height: 200px'>{}x{}</div>",
        "<div style='opacity: .9'>".repeat(depth),
        "</div>".repeat(depth)
    );
    let document = Document::html(html);
    let held = exceeded_memory(render(&document, Limits::new().memory(Some(16 * MIB))));
    assert_eq!(held[0], "rasterized pixels");
    assert!(render(&document, Limits::new()).is_ok());
}

#[test]
fn ends_a_document_whose_cells_span_thousands_of_rows() {
    // Each cell spans every row below it: the rows each cell spans add up to millions.
    let mut html = String::from("<table>");
    for row in 0..4000 {
        write!(html, "<tr><td rowspan=0>{row}</td></tr>").expect("written");
    }
    let document = Document::html(html);
    let held = exceeded_memory(render(&document, Limits::new().memory(Some(64 * MIB))));
    assert_eq!(held[0], "table cells");
}

#[test]
fn ends_a_long_document_with_a_small_budget() {
    let document = Document::html("<p style='border: 1px solid'>Hallo Welt, ein Absatz.</p>".repeat(20_000));
    let held = exceeded_memory(render(&document, Limits::new().memory(Some(16 * MIB))));
    assert!(
        held.iter()
            .take(2)
            .any(|kind| ["styles", "layout"].contains(kind)),
        "{held:?}"
    );
}

#[test]
fn ends_a_document_when_its_time_is_up() {
    let document = Document::html("<p style='border: 1px solid'>Hallo Welt, ein Absatz.</p>".repeat(20_000));
    let start = Instant::now();
    let failure =
        render(&document, Limits::new().time(Some(Duration::from_millis(20)))).expect_err("too slow");
    let elapsed = start.elapsed();
    assert!(matches!(failure.error, Error::TimeLimit { limit } if limit == Duration::from_millis(20)));
    assert_eq!(failure.diagnostics[0].code, "limit.time");
    // It stops close to the deadline (rendering it whole takes seconds).
    if !cfg!(debug_assertions) {
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
    }
}

#[test]
fn ends_a_document_laid_out_again_for_scroll_bars_when_its_time_is_up() {
    // Each box overflows, gets a scroll bar and makes the document be laid out again.
    let document = Document::html(
        "<div style='overflow: auto; height: 20px'>Hallo Welt,<br>ein Absatz,<br>der überläuft.</div>"
            .repeat(10_000),
    );
    let failure =
        render(&document, Limits::new().time(Some(Duration::from_millis(20)))).expect_err("too slow");
    assert!(matches!(failure.error, Error::TimeLimit { .. }));
    assert_eq!(failure.diagnostics[0].code, "limit.time");
}

#[test]
fn counts_the_time_a_loader_takes() {
    struct Slow;
    impl ResourceLoader for Slow {
        fn load(&self, _: &str) -> Option<Vec<u8>> {
            std::thread::sleep(Duration::from_millis(20));
            None
        }
    }
    let html: String = (0..100).fold(String::new(), |mut html, index| {
        write!(html, "<img src='{index}.png'>").expect("written");
        html
    });
    let document = Document::html(html).loader(Arc::new(Slow));
    let failure =
        render(&document, Limits::new().time(Some(Duration::from_millis(100)))).expect_err("too slow");
    assert!(matches!(failure.error, Error::TimeLimit { .. }));
}

#[test]
fn counts_the_time_a_loader_takes_for_a_batch() {
    // The limit is looked at after each batch a loader answers: of the four batches the 200
    // images make, at most the first is asked for (none where the limit has passed before it,
    // as on a slow machine).
    struct Slow(AtomicUsize);
    impl ResourceLoader for Slow {
        fn load(&self, _: &str) -> Option<Vec<u8>> {
            None
        }

        fn load_many(&self, urls: &[&str]) -> Vec<Option<Vec<u8>>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(150));
            vec![None; urls.len()]
        }
    }
    let html: String = (0..200).fold(String::new(), |mut html, index| {
        write!(html, "<img src='{index}.png'>").expect("written");
        html
    });
    let loader = Arc::new(Slow(AtomicUsize::new(0)));
    let document = Document::html(html).loader(Arc::clone(&loader) as Arc<dyn ResourceLoader>);
    let failure =
        render(&document, Limits::new().time(Some(Duration::from_millis(100)))).expect_err("too slow");
    assert!(matches!(failure.error, Error::TimeLimit { .. }));
    assert!(loader.0.load(Ordering::Relaxed) <= 1);
}

#[test]
fn renders_the_same_bytes_within_the_limits() {
    let mut html = String::from(
        "<style>td { border: 1px solid; box-shadow: 0 0 4px red } .f { filter: blur(1px) }</style>\
         <img src='logo.png'><div class=f><div style='opacity: .5'>Filter</div></div>\
         <div style='overflow: auto; height: 30px'>Laid out again<br>for the scroll bar<br>it gets</div><table>",
    );
    for row in 0..300 {
        write!(html, "<tr><td>{row}</td><td rowspan=2>Posten</td></tr>").expect("written");
    }
    html.push_str("</table>");
    let document = Document::html(html).resource("logo.png", png(300, 100, [10, 20, 30]));
    let unlimited = render(&document, Limits::unlimited()).expect("rendered");
    let limited = render(
        &document,
        Limits::new()
            .memory(Some(512 * MIB))
            .time(Some(Duration::from_secs(600))),
    )
    .expect("rendered");
    assert!(unlimited.pages > 3);
    assert_eq!(unlimited.pdf, limited.pdf);
    assert_eq!(
        render(&document, Limits::new()).expect("rendered").pdf,
        unlimited.pdf
    );
}

/// Hands out the same bytes under every URL, and counts how often it was asked.
struct Logos(Vec<u8>, AtomicUsize);

impl ResourceLoader for Logos {
    fn load(&self, _: &str) -> Option<Vec<u8>> {
        self.1.fetch_add(1, Ordering::Relaxed);
        Some(self.0.clone())
    }
}

#[test]
fn holds_a_resource_under_many_urls_once() {
    // A style sheet of 1 MiB under 200 URLs: 200 MiB if each were held on its own.
    let directory = std::env::temp_dir().join(format!("fagus-limits-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("directory");
    let mut css = String::from("p { color: green }\n/*");
    css.push_str(&"x".repeat(1 << 20));
    css.push_str("*/");
    std::fs::write(directory.join("big.css"), css).expect("written");
    let links: String = (0..200).fold(String::new(), |mut links, index| {
        write!(links, "<link rel=stylesheet href='big.css?v={index}'>").expect("written");
        links
    });
    let document = Document::html(format!("{links}<p>x</p>")).base_directory(&directory);
    let output = render(&document, Limits::new().memory(Some(64 * MIB)));
    std::fs::remove_dir_all(&directory).expect("removed");
    assert!(output.is_ok(), "{:?}", output.err());

    // The same bytes from a loader under many names: one image in the PDF.
    let loader = Arc::new(Logos(png(400, 300, [200, 30, 30]), AtomicUsize::new(0)));
    let images: String = (0..50).fold(String::new(), |mut images, index| {
        write!(images, "<img src='logo.png?{index}' style='width: 40px'>").expect("written");
        images
    });
    let document = Document::html(images).loader(Arc::clone(&loader) as Arc<dyn ResourceLoader>);
    let output = render(&document, Limits::new()).expect("rendered");
    assert_eq!(loader.1.load(Ordering::Relaxed), 50);
    let images = output
        .pdf
        .windows(b"/Subtype/Image".len())
        .filter(|window| *window == b"/Subtype/Image")
        .count();
    assert_eq!(images, 1);
}

#[test]
fn keeps_the_engine_working_after_a_document_ran_out() {
    let engine = Engine::new();
    let document = Document::html("<p>Hallo</p>".repeat(5000));
    let failure = engine
        .render_with_diagnostics(&document.clone().limits(Limits::new().memory(Some(MIB))))
        .expect_err("over the budget");
    assert!(matches!(failure.error, Error::MemoryBudget { budget, .. } if budget == MIB));
    // The next document, and the same one with the engine's limits, render.
    assert!(engine.render(&Document::html("<p>Danach</p>")).is_ok());
    assert!(engine.render(&document).is_ok());
    let mut engine = engine;
    engine.set_limits(Limits::new().memory(Some(MIB)));
    assert_eq!(engine.limits().memory_budget(), Some(MIB));
    assert!(matches!(
        engine.render(&document),
        Err(Error::MemoryBudget { .. })
    ));
}
