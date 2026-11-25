//! Documents that once ended in an internal error (a panic caught by the engine): they render.

use std::fmt::Write as _;

use fagus::{Document, Engine};

fn renders(html: &str) {
    let output = Engine::new().render(&Document::html(html));
    assert!(output.is_ok(), "{html}: {:?}", output.err());
}

#[test]
fn renders_css_that_makes_the_css_parser_panic() {
    // lightningcss reaches an `unreachable!()` where `calc()` of a number stands for a
    // percentage; the declarations, rules and conditions with it are left out.
    renders("<p style='font-stretch: calc(5); color: red'>x</p>");
    renders("<p style='o:('');font-stretch:calc(infinity'>x</p>");
    renders(
        "<style>p { font-stretch: calc(50% + 1) } @keyframes k { calc(5) { color: red } }</style><p>x</p>",
    );
    renders("<style>@supports (font-stretch: calc(5)) { p { color: red } }</style><p>x</p>");
    renders("<style>p { --x: calc(5); font-stretch: var(--x) }</style><p>x</p>");
}

#[test]
fn a_document_without_a_root_box_has_a_page() {
    // Found by the layout invariants: with `display: none` on the root element, the PDF had no
    // page at all. Chrome prints one empty page, with its margin boxes.
    let html = "<style>@page { @top-center { content: 'Page ' counter(page) } }</style>\
                <html style='display: none'>x";
    let output = Engine::new().render(&Document::html(html)).expect("rendered");
    assert_eq!(output.pages, 1);
    let layout = Engine::new()
        .render(&Document::html("<html style='display: none'><p id=text>x").report_layout(true))
        .expect("rendered")
        .layout
        .expect("layout");
    assert_eq!(layout.pages.len(), 1);
    assert_eq!(layout.elements[0].boxes, []);
}

#[test]
fn renders_text_at_absurd_font_sizes() {
    // The advance of a glyph is taken in fixed point as FreeType scales it, whose products were
    // past 64 bits: a panic where overflow is checked, any advance where it is not.
    renders("<p style='font-size: 1e30px; line-height: 0'>x</p>");
    renders("<p style='font-size: 1e15px; line-height: 0; letter-spacing: 1e30px'>AV fi</p>");
}

#[test]
fn cuts_the_font_size_to_the_largest_of_chrome() {
    // Found by the layout invariants: `zoom` times a large `font-size` was infinite, and the
    // glyphs of its text were placed at offsets that are no numbers. Chrome sets no text larger
    // than 10,000 pixels, zoomed, and `em` follows the size cut to that.
    let pages = |content: &str| {
        let html = format!(
            "<style>@page {{ size: 1000px 1000px; margin: 0 }} body {{ margin: 0 }} \
             p {{ margin: 0; line-height: 1 }}</style>{content}"
        );
        Engine::new()
            .render(&Document::html(html))
            .expect("rendered")
            .pages
    };
    let largest = pages("<p style='font-size: 10000px'>x</p>");
    assert!(largest < 20, "{largest} pages");
    assert_eq!(pages("<p style='font-size: 1e9px'>x</p>"), largest);
    assert_eq!(
        pages("<div style='zoom: 4'><p style='font-size: 9000px'>x</p></div>"),
        largest
    );
    assert_eq!(
        pages("<div style='zoom: 1e30'><p style='font-size: 1e30px'>x</p></div>"),
        largest
    );
    let em = pages("<div style='font-size: 10000px'><p style='height: 1em'></p></div>");
    assert_eq!(
        pages("<div style='font-size: 1e9px'><p style='height: 1em'></p></div>"),
        em
    );
}

#[test]
fn keeps_the_zoom_within_the_range_of_chrome() {
    // Found by the layout invariants: lengths times a `zoom` of 1e30 were infinite, and text
    // with such a `letter-spacing` had advances that are no numbers. Chrome keeps the zoom of
    // an element with those of its ancestors between a millionth and a million.
    let pages = |content: &str| {
        let html = format!(
            "<style>@page {{ size: 1000px 1000px; margin: 0 }} body {{ margin: 0 }}</style>{content}"
        );
        Engine::new()
            .render(&Document::html(html))
            .expect("rendered")
            .pages
    };
    assert_eq!(
        pages("<div style='zoom: 1e6'><p style='height: 5e-3px'></p></div>"),
        5
    );
    assert_eq!(
        pages("<div style='zoom: 1e8'><p style='height: 5e-5px'></p></div>"),
        1
    );
    assert_eq!(
        pages("<div style='zoom: 1e4'><div style='zoom: 1e4'><p style='height: 5e-5px'></p></div></div>"),
        1
    );
    assert_eq!(
        pages("<div style='zoom: 1e-8'><p style='height: 5e9px'></p></div>"),
        5
    );
    renders("<div style='zoom: 1e30; letter-spacing: 1e30px'>y z</div>");
}

#[test]
fn spaces_letters_and_words_by_lengths_layout_holds() {
    // Found by the layout invariants: with a `letter-spacing` near the largest number or
    // infinite, the advances of glyphs were infinite or no numbers and their text was laid out
    // with a negative width. Such spacing is the longest length of layout.
    let width = |spacing: &str| {
        let html = format!("<span id=text style='{spacing}'>abc def</span>");
        let output = Engine::new()
            .render(&Document::html(html).report_layout(true))
            .expect("rendered");
        output.layout.expect("layout").elements[0].boxes[0].width
    };
    for spacing in [
        "letter-spacing: 3e38px",
        "letter-spacing: calc(infinity * 1px)",
        "letter-spacing: 3e38px; word-spacing: 3e38px",
    ] {
        assert!(width(spacing) > 1.0e6, "{spacing}: {}", width(spacing));
    }
    // The words are on lines of their own.
    assert!(width("word-spacing: 3e38px") > 0.0);
    renders("<p style='letter-spacing: -3e38px; word-spacing: calc(-infinity * 1px)'>abc def</p>");
}

#[test]
fn renders_clip_shapes_too_large_to_draw() {
    // The clip was left out but its end was not, which krilla could not pop.
    renders("<div style='clip-path: circle(1e30px)'>x</div>");
    renders("<div style='clip-path: polygon(0 0, 1e30px 0, 0 1e30px)'>x</div>");
}

#[test]
fn renders_leaders_in_absurdly_wide_lines() {
    renders("<style>@page { margin: -1e30px } a::after { content: leader('.') }</style><a href>x</a>");
}

#[test]
fn blurs_a_shadow_repeated_on_every_page_once() {
    // A fixed box with blurred shadows is on each of 2,800 pages; each page blurred them again
    // (seconds per thousand pages, minutes with `filter: blur()` on ten thousand).
    let html = "<div style='position: fixed; top: 0; width: 600px; height: 300px; \
                text-shadow: 0 0 300px red; box-shadow: 0 0 300px blue'>Kopf</div>\
                <div style='height: 3000000px'></div>";
    let start = std::time::Instant::now();
    let output = Engine::new().render(&Document::html(html)).expect("rendered");
    assert!(output.pages > 2000);
    if !cfg!(debug_assertions) {
        assert!(start.elapsed().as_secs() < 3, "took {:?}", start.elapsed());
    }
}

#[test]
fn renders_subgrid_items_placed_past_the_subgrid() {
    // A subgrid placed its items before it knew how many rows it spans; those past its rows
    // indexed past the rows of its parent.
    renders("x<style>* { display: grid; grid-template-rows: subgrid }</style>");
    renders(
        "<style>.o { display: grid; grid-template-rows: repeat(5, 10px); grid-auto-flow: column } \
         .s { display: grid; grid-row: span 5; grid-template-rows: subgrid } .i { grid-row: span 6 }\
         </style><div class=o><div class=s><div class=i>item</div></div></div>",
    );
}

#[test]
fn renders_svgs_with_images_that_do_not_decode() {
    // krilla decodes the images of an SVG as it writes the PDF, and failed the document for a
    // GIF without colors; the image is left out.
    renders(
        "<svg><image width=10 height=10 \
         href='data:image/gif;base64,R0lGODlhAQABAAD/ACwAAAAAAQABAAACADs='/></svg>",
    );
}

#[test]
fn breaks_a_long_word_between_letters_in_linear_time() {
    // `word-break: break-all` in a word of 80,000 letters: each line measured the word up to
    // each place it could break from its start, and shaped the rest of the word again (25 s).
    let html = format!("<p style='word-break: break-all'>{}</p>", "ab".repeat(40_000));
    let start = std::time::Instant::now();
    let output = Engine::new().render(&Document::html(html)).expect("rendered");
    assert!(output.pages > 10);
    if !cfg!(debug_assertions) {
        assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
    }
}

#[test]
fn skips_ink_of_long_underlined_text_quickly() {
    // Each glyph of underlined text had its outline drawn again to find where the underline
    // skips it: 200 pages of underlined text took 7 s to paint.
    let html = format!(
        "<u>{}</u>",
        "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(15_000)
    );
    let start = std::time::Instant::now();
    let output = Engine::new().render(&Document::html(html)).expect("rendered");
    assert!(output.pages > 100);
    if !cfg!(debug_assertions) {
        assert!(start.elapsed().as_secs() < 4, "took {:?}", start.elapsed());
    }
}

#[test]
fn shapes_a_cluster_of_thousands_of_marks_in_linear_time() {
    // Each glyph looked for the next cluster among all glyphs after it: 160,000 marks on one
    // letter took seconds.
    let html = format!("<p>a{}</p>", "\u{301}\u{302}\u{327}\u{308}".repeat(40_000));
    let start = std::time::Instant::now();
    Engine::new().render(&Document::html(html)).expect("rendered");
    if !cfg!(debug_assertions) {
        assert!(start.elapsed().as_secs() < 3, "took {:?}", start.elapsed());
    }
}

#[test]
fn spaces_letters_of_text_without_a_script_in_linear_time() {
    // Each character before the first of a script of its own looked for that one among all
    // characters after it: 100,000 zero width spaces with letter spacing took 24 s.
    let html = format!(
        "<p style='letter-spacing: 1px'>{}</p>",
        "\u{200B}".repeat(100_000)
    );
    let start = std::time::Instant::now();
    Engine::new().render(&Document::html(html)).expect("rendered");
    if !cfg!(debug_assertions) {
        assert!(start.elapsed().as_secs() < 3, "took {:?}", start.elapsed());
    }
}

#[test]
fn draws_turbulence_of_a_billion_octaves_as_of_nine() {
    // resvg added up as many octaves of noise as the filter asks for, for each pixel.
    let filter = "<filter id=f><feTurbulence numOctaves='1000000000' baseFrequency='0.05'/></filter>\
                  <rect width='100' height='100' filter='url(#f)'/>";
    let start = std::time::Instant::now();
    renders(&format!("<svg width=100 height=100>{filter}</svg>"));
    renders(&format!(
        "<img src=\"data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='100' \
         height='100'>{}</svg>\">",
        filter.replace('#', "%23")
    ));
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
}

#[test]
fn lays_out_a_paragraph_of_hundreds_of_pages_in_linear_time() {
    // A `<pre>` of 40,000 lines of a log: the rest of the paragraph was copied for each page
    // (9 s for 600 pages).
    let mut log = String::new();
    for line in 0..40_000 {
        let _ = writeln!(
            log,
            "2026-01-01 12:00:{:02} INFO request handled id={line} status=200",
            line % 60
        );
    }
    let start = std::time::Instant::now();
    let output = Engine::new()
        .render(&Document::html(format!("<pre>{log}</pre>")))
        .expect("rendered");
    assert!(output.pages > 600);
    if !cfg!(debug_assertions) {
        assert!(start.elapsed().as_secs() < 8, "took {:?}", start.elapsed());
    }
}

#[test]
fn warns_where_a_document_reaches_the_page_limit() {
    let output = Engine::new()
        .render(&Document::html("<p>x</p><div style='height: 1e9px'></div>"))
        .expect("rendered");
    assert_eq!(output.pages, 10_000);
    assert!(
        output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "layout.too-many-pages")
    );
    let output = Engine::new()
        .render(&Document::html("<p>x</p>"))
        .expect("rendered");
    assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
}

#[test]
fn lays_out_deeply_nested_columns_in_linear_time() {
    // Columns inside columns are laid out as one, and were laid out again to leave room for
    // their bottom border and padding where they did not fit: twice for each level of nesting.
    let html = format!(
        "<style>blockquote {{ columns: 2 }} *::before {{ content: counter(c) }}</style>{}x{}",
        "<blockquote>".repeat(40),
        "</blockquote>".repeat(40)
    );
    let start = std::time::Instant::now();
    renders(&html);
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
}

#[test]
fn raises_text_by_an_initial_letter_of_billions_of_lines() {
    // A line break for each line the letter raised the text by: billions, and gigabytes.
    let start = std::time::Instant::now();
    renders("<style>p::first-letter { initial-letter: 2147483647 3 }</style><p>Lorem ipsum</p>");
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
}

#[test]
fn leaves_a_small_image_unblurred_rather_than_padding_it_to_gigabytes() {
    // The padding for the blur was allocated before the blur was found too large to do: 57 GB
    // for a progress bar.
    renders("<div style='filter: blur(20000px)'><progress value=1 max=2></progress></div>");
    renders(
        "<div style='filter: blur(20000px)'><img src='data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7'></div>",
    );
}

/// A loader that has `data` under every URL.
struct Everywhere(Vec<u8>);

impl fagus::ResourceLoader for Everywhere {
    fn load(&self, _url: &str) -> Option<Vec<u8>> {
        Some(self.0.clone())
    }
}

/// Renders `html` whose every resource is `data`, in less than `seconds` in a release build.
/// The time is wall time, so a busy machine gets three tries.
fn renders_quickly(html: &str, data: Option<Vec<u8>>, seconds: u64) -> fagus::Output {
    let mut document = Document::html(html);
    if let Some(data) = data {
        document = document.loader(std::sync::Arc::new(Everywhere(data)));
    }
    let mut elapsed = Vec::new();
    for _ in 0..3 {
        let start = std::time::Instant::now();
        let output = Engine::new().render(&document);
        let output = output.unwrap_or_else(|error| panic!("{error:?}"));
        elapsed.push(start.elapsed());
        if cfg!(debug_assertions) || start.elapsed().as_secs() < seconds {
            return output;
        }
    }
    panic!("took {elapsed:?}");
}

#[test]
fn parses_nested_math_functions_that_do_not_parse_in_linear_time() {
    // lightningcss parses a math function that does not parse twice in another one: twice as
    // long for each level of nesting, minutes for 30 levels.
    let calc = "calc(".repeat(30);
    let closed = format!("{}x{}", "calc(".repeat(30), ")".repeat(30));
    let min = format!("{}x{}", "min(".repeat(30), ")".repeat(30));
    let chain = joined(0..30, |level| format!("--a{}: calc(var(--a{level}));", level + 1));
    renders_quickly(
        &format!(
            "<style>p {{ width: {closed} }} :root {{ --a0: x; {chain} }} div {{ width: var(--a30) }}</style>\
             <p style='margin: {min}'>x</p><div>x</div><img sizes='{calc}' srcset='a.png 100w'>\
             <picture><source media='(min-width: {closed})' srcset='a.png'><img></picture>"
        ),
        None,
        5,
    );
}

#[test]
fn places_thousands_of_boxes_against_anchors_in_quadratic_time() {
    // The anchors on the page were found for each box placed against them, each looked up among
    // all found before: cubic, 13 seconds for 2,000 boxes.
    let html = format!(
        "<style>* {{ anchor-name: --a, --b }} .p {{ position: absolute; position-anchor: --a; \
         left: anchor(--b right); top: anchor(bottom) }}</style>{}",
        "<div class=p><span class=p>x</span></div>".repeat(1000)
    );
    renders_quickly(&html, None, 5);
}

#[test]
fn copies_a_sprite_used_under_thousands_of_urls_once() {
    // A sprite of 2,000 symbols used under 5,000 URLs was copied in for each (gigabytes), its ids
    // prefixed in quadratic time.
    let symbol =
        |index| format!("<symbol id='s{index}'><rect id=\"r{index}\" width='1' height='1'/></symbol>");
    let sprite = joined(0..2000, symbol);
    let sprite = format!("<svg xmlns='http://www.w3.org/2000/svg'>{sprite}</svg>").into_bytes();
    let uses = joined(0..5000, |index| {
        format!("<use href='s.svg?v={index}#s{}'/>", index % 2000)
    });
    renders_quickly(
        &format!("<svg width=10 height=10>{uses}</svg>"),
        Some(sprite.clone()),
        5,
    );
    // One sprite of 60,000 symbols.
    let big = joined(0..60_000, symbol);
    let big = format!("<svg xmlns='http://www.w3.org/2000/svg'>{big}</svg>").into_bytes();
    renders_quickly(
        "<svg width=10 height=10><use href='s.svg#s1'/></svg>",
        Some(big),
        5,
    );
}

#[test]
fn writes_an_image_shown_thousands_of_times_in_an_svg_a_limited_number_of_times() {
    // Each `<image>` had the image written into the SVG as a `data:` URL: 4,000 of an image of
    // 600 kB took gigabytes.
    let png = noise_png(400, 400);
    let images = "<image href='noise.png' width='1' height='1'/>".repeat(4000);
    renders_quickly(&format!("<svg width=10 height=10>{images}</svg>"), Some(png), 10);
}

#[test]
fn draws_svg_arcs_of_astronomic_radii_as_few_curves() {
    // usvg cuts arcs into curves a tenth of a unit from them: millions for a circle of 1e38
    // units, gigabytes of PDF and tens of seconds.
    renders_quickly(
        "<svg width=10 height=10 viewBox='0 0 1e38 1e38'><circle r='1e38'/><ellipse rx='1e38' ry='1e38'/>\
         <rect width='1e38' height='1e38' rx='1e38'/><path d='M0 0 A 1 1 0 1 0 1e38 0'/></svg>",
        None,
        5,
    );
}

#[test]
fn measures_dotted_borders_with_shaped_corners_of_millions_of_pixels_quickly() {
    // The corners were cut in 65,536 pieces each to measure the border, on each of 10,000 pages.
    renders_quickly(
        "<div style='corner-shape: squircle; border-radius: 1e8px; border: 1e7px dotted red; \
         width: 0; height: 0; display: inline-block'>x</div>",
        None,
        10,
    );
}

#[test]
fn lays_out_a_hundred_thousand_paragraphs_in_linear_time() {
    // All children of the body were walked on each page to find the first and last in flow:
    // four times the paragraphs took sixteen times as long (15 seconds for 150,000).
    let pages = renders_linearly(|count| "<p>x".repeat(count), 160_000);
    assert!(pages > 5000);
}

#[test]
fn draws_a_fixed_background_of_thousands_of_tiles_on_ten_thousand_pages() {
    // 2,500 tiles of a fixed box's background on each of 10,000 pages: 25 million images,
    // 5 GB and 10 seconds.
    let pixel = "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    let output = renders_quickly(
        &format!(
            "<div style='height: 2e7px'></div><div style='position: fixed; width: 100px; height: 100px; \
             background: url({pixel}) 0 0 / 2px 2px'>x</div>"
        ),
        None,
        10,
    );
    assert_eq!(output.pages, 10_000);
}

/// A PNG image of `width` × `height` pixels of noise, which does not compress.
fn noise_png(width: u32, height: u32) -> Vec<u8> {
    use std::io::Write as _;
    let chunk = |png: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&u32::try_from(data.len()).expect("small").to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc.sum().to_be_bytes());
    };
    // Each row starts with its filter (none), then has random bytes.
    let row = width as usize * 3 + 1;
    let mut state = 1u32;
    let raw: Vec<u8> = (0..row * height as usize)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if index % row == 0 {
                0
            } else {
                state.to_le_bytes()[0]
            }
        })
        .collect();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&raw).expect("compressed");
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &encoder.finish().expect("compressed"));
    chunk(&mut png, b"IEND", &[]);
    png
}

/// The strings `item` makes of `range`, one after the other.
fn joined(range: std::ops::Range<usize>, item: impl Fn(usize) -> String) -> String {
    range.fold(String::new(), |mut out, index| {
        out.push_str(&item(index));
        out
    })
}

#[test]
fn lays_out_a_column_flex_container_of_fifty_thousand_items_in_linear_time() {
    // Its items were sorted and checked for forced breaks again on each of its 1,000 pages:
    // four times the items took fifteen times as long (6.5 seconds for 50,000).
    let pages = renders_linearly(
        |count| {
            format!(
                "<div style='display: flex; flex-direction: column'>{}</div>",
                "<div>b</div>".repeat(count)
            )
        },
        50_000,
    );
    assert!(pages > 900);
}

#[test]
fn renders_svg_images_whose_scaling_overflows() {
    // usvg scales an image of 1e30 units to one as large: its size overflows to infinity, and
    // it unwraps the size that is not one.
    renders(
        "<svg><image href='data:image/svg+xml,<svg xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 1e38 1e38\" width=\"1e30\" height=\"1e30\"/>' width='1e30' height='1e30'/></svg>",
    );
}

/// Renders `document(size)` and checks (in optimized builds) that it takes less than 9 times as
/// long as `document(size / 4)`: the time alone depends on what else the machine runs, which
/// slows both. Where it does not, each is timed up to twice more and the fastest compared.
/// The page count of the whole document.
fn renders_linearly(document: impl Fn(usize) -> String, size: usize) -> usize {
    let time = |size: usize| {
        let start = std::time::Instant::now();
        let output = Engine::new()
            .render(&Document::html(document(size)))
            .expect("rendered");
        (start.elapsed(), output.pages)
    };
    let (mut quarter, _) = time(size / 4);
    let (mut whole, pages) = time(size);
    if !cfg!(debug_assertions) {
        for _ in 0..2 {
            if whole < quarter * 9 {
                break;
            }
            quarter = quarter.min(time(size / 4).0);
            whole = whole.min(time(size).0);
        }
        assert!(
            whole < quarter * 9,
            "{quarter:?} for a quarter, {whole:?} for all"
        );
    }
    pages
}

#[test]
fn renders_nested_svgs_under_transforms_past_what_f32_holds() {
    // krilla failed to transform the clip of the viewport of a nested `<svg>` scaled by 1e30
    // (once by CSS, twice by usvg) or by nested transforms that reach infinity.
    renders(
        "<svg width=200 height=200><svg width=50 height=50 style='transform: scale(1e30)'><rect width=5 height=5 /></svg></svg>",
    );
    renders(
        "<svg width=200 height=200><svg width=50 height=50 transform='scale(1e19)'><rect width=5 height=5 /></svg></svg>",
    );
    let nested = "<svg width=50 height=50 transform='scale(1e30)'>".repeat(5);
    renders(&format!(
        "<svg width=200 height=200>{nested}<rect width=5 height=5 />{}</svg>",
        "</svg>".repeat(5)
    ));
}

#[test]
fn lays_out_grids_in_grids_that_size_their_columns_twice_in_polynomial_time() {
    // A grid whose items take their widths from the heights of rows sized by their content
    // lays out its items twice; with grids such as it nested 40 deep, 2^40 times.
    let level = "<div style='display: grid; grid-template-columns: auto auto'>\
                 <div style='aspect-ratio: 1; height: 100%'>a</div>";
    let html = format!("{}x{}", level.repeat(40), "</div>".repeat(40));
    renders_quickly(&html, None, 5);
}

#[test]
fn loads_an_svg_document_of_clip_paths_under_thousands_of_urls_once() {
    // Each URL was parsed into a document of its own and kept: 40 URLs of a 5 MB file took
    // 12 seconds and 2 GB.
    let clips = joined(0..20_000, |index| {
        format!("<clipPath id='c{index}'><rect x='{index}' width='5' height='5'/></clipPath>")
    });
    let svg = format!("<svg xmlns='http://www.w3.org/2000/svg'>{clips}</svg>").into_bytes();
    let boxes = joined(0..2000, |index| {
        format!("<div style=\"height: 2px; clip-path: url('d.svg?v={index}#c{index}')\"></div>")
    });
    renders_quickly(&boxes, Some(svg), 5);
}

#[test]
fn lays_out_subgrids_nested_in_stretched_items_in_linear_time() {
    // An item with percentage heights inside is laid out again stretched to its area; the
    // layouts of items were not kept in documents with subgrids, and each of 30 nested subgrids
    // laid out the next twice over (2^30 times).
    let level =
        "<div style='display: grid; grid-template-columns: subgrid'><div style='height: 100%'>a</div>";
    let html = format!("{}x{}", level.repeat(30), "</div>".repeat(30));
    renders_quickly(&html, None, 5);
}

#[test]
fn parses_a_large_svg_image_named_under_thousands_of_fragments_once() {
    // `mask: url(m.svg#m1)`, `url(m.svg#m2)`, …: each URL parsed the same 3 MB file again,
    // twice (minutes for 20,000 of them).
    let masks = joined(0..30_000, |index| {
        format!("<mask id='m{index}'><rect width='5' height='5' fill='white'/></mask>")
    });
    let svg = format!("<svg xmlns='http://www.w3.org/2000/svg'>{masks}</svg>").into_bytes();
    let boxes = joined(0..3000, |index| {
        format!("<div style=\"height: 2px; mask: url('m.svg#m{index}')\"></div>")
    });
    renders_quickly(&boxes, Some(svg), 5);
}

#[test]
fn lays_out_a_table_of_eighty_thousand_rows_in_linear_time() {
    // Each page of the table looked through all its rows for positioned ones: four times the
    // rows took twelve times as long (10 seconds for 80,000).
    let pages = renders_linearly(
        |count| format!("<table>{}</table>", "<tr><td>x</td></tr>".repeat(count)),
        80_000,
    );
    assert!(pages > 1000);
}

/// The codes of the diagnostics of `output`.
fn codes(output: &fagus::Output) -> Vec<&'static str> {
    output
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code)
        .collect()
}

#[test]
fn ends_frames_that_include_themselves() {
    // Each document shows itself three times: without limits, 3^n documents n frames deep.
    let page = b"<p>x</p><iframe src='self.html'></iframe><object data='self.html'></object>\
                 <embed src='self.html'>"
        .to_vec();
    let output = renders_quickly("<iframe src='self.html'></iframe>", Some(page), 5);
    assert!(
        codes(&output).contains(&"frame.limit"),
        "{:?}",
        output.diagnostics
    );
    // A chain of documents each in the frame of the one before, as deep as frames nest.
    let chain = b"<iframe src='next.html'></iframe>".to_vec();
    let output = renders_quickly("<iframe src='first.html'></iframe>", Some(chain), 5);
    assert!(
        !codes(&output).contains(&"frame.limit"),
        "{:?}",
        output.diagnostics
    );
}

#[test]
fn ends_srcdoc_frames_nested_in_one_another() {
    // Each level escapes the one inside it, so the markup grows with the depth.
    let mut html = "<p>x</p>".to_owned();
    for _ in 0..12 {
        let escaped = html.replace('&', "&amp;").replace('"', "&quot;");
        html = format!("<iframe srcdoc=\"{escaped}\"></iframe>");
    }
    renders_quickly(&html, None, 5);
}

#[test]
fn lays_out_a_limited_number_of_frames() {
    // Ten thousand frames: the first ones show their documents, the others nothing.
    let html = "<iframe srcdoc='<p>x</p>' width=20 height=10></iframe>".repeat(10_000);
    let output = renders_quickly(&html, None, 10);
    assert!(
        codes(&output).contains(&"frame.limit"),
        "{:?}",
        output.diagnostics
    );
}

#[test]
fn leaves_a_huge_srcdoc_empty() {
    // A document in a frame larger than all documents of frames may be together.
    let html = format!("<iframe srcdoc='{}'></iframe>", "<p>x</p>".repeat(1_200_000));
    let output = renders_quickly(&html, None, 5);
    assert!(
        codes(&output).contains(&"frame.limit"),
        "{:?}",
        output.diagnostics
    );
}

/// The font `font` with its table `tag` replaced by `table`, which is put at the end.
fn with_table(mut font: Vec<u8>, tag: &[u8; 4], table: &[u8]) -> Vec<u8> {
    let count = usize::from(u16::from_be_bytes([font[4], font[5]]));
    let entry = (0..count)
        .map(|index| 12 + 16 * index)
        .find(|entry| &font[*entry..*entry + 4] == tag)
        .expect("the table");
    while !font.len().is_multiple_of(4) {
        font.push(0);
    }
    let offset = u32::try_from(font.len()).expect("small");
    let length = u32::try_from(table.len()).expect("small");
    font[entry + 8..entry + 12].copy_from_slice(&offset.to_be_bytes());
    font[entry + 12..entry + 16].copy_from_slice(&length.to_be_bytes());
    font.extend_from_slice(table);
    font
}

#[test]
fn renders_text_in_a_font_whose_characters_map_to_glyphs_it_does_not_have() {
    // A `cmap` of format 12 numbers glyphs with 32 bits: `A` to `Z` here map to glyphs past the
    // 65,536 a font can have, which krilla could not write.
    let mut cmap = Vec::new();
    cmap.extend_from_slice(&[0, 0, 0, 1, 0, 3, 0, 10, 0, 0, 0, 12]);
    cmap.extend_from_slice(&[0, 12, 0, 0, 0, 0, 0, 28, 0, 0, 0, 0, 0, 0, 0, 1]);
    for value in [0x41u32, 0x5A, 70_000] {
        cmap.extend_from_slice(&value.to_be_bytes());
    }
    renders_in_font(with_table(inter(), b"cmap", &cmap));
}

#[test]
fn leaves_out_a_font_whose_em_square_has_no_size() {
    // Glyphs are scaled by the size of the em square: krilla divided by 0 for the font's box.
    let mut inter = inter();
    let count = usize::from(u16::from_be_bytes([inter[4], inter[5]]));
    let head = (0..count)
        .map(|index| 12 + 16 * index)
        .find(|entry| &inter[*entry..*entry + 4] == b"head")
        .expect("the table");
    let offset = u32::from_be_bytes(inter[head + 8..head + 12].try_into().expect("4 bytes")) as usize;
    let mut table = inter[offset..offset + 54].to_vec();
    table[18..20].copy_from_slice(&0u16.to_be_bytes());
    inter = with_table(inter, b"head", &table);
    renders_in_font(inter.clone());
    let mut engine = Engine::new();
    assert!(engine.add_font_data(inter).is_err());
}

/// The font file of Inter, regular.
fn inter() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fonts/Inter-Regular.ttf"
    ))
    .expect("Inter")
}

/// Renders text in `font`, loaded through `@font-face`.
fn renders_in_font(font: Vec<u8>) {
    let document = Document::html(
        "<style>@font-face { font-family: F; src: url(f.ttf) } p { font-family: F }</style><p>ABC</p>",
    )
    .resource("f.ttf", font);
    let output = Engine::new().render(&document);
    assert!(output.is_ok(), "{:?}", output.err());
}

#[test]
fn leaves_out_an_image_in_svg_that_claims_billions_of_pixels() {
    // A PNG of 258 bytes whose header gives 200 × 788,529,252 pixels: images in SVG were decoded
    // without a limit on their size, into a buffer of 473 GB (which aborts the process where the
    // system refuses to promise that much memory, as Linux does).
    let png = "iVBORw0KGgoAAAANSUhEUgAAAMgvAABkCAIAAABM5OhcAAAAyUlEQVR42u3SMREAAAjEMEDm+xeCCSaGREKvnaTg2kiAsTAWxgJjYSyMBcbCWBgLjIWxMBYYC2NhLDAWxsJYYCyMhbHAWBgLY4GxMBbGAmNhLIwFxsJYGAuMhbEwFhgLY2EsMBbGwlhgLIyFscBYGAtBgbEwFsYCY2EsjAXGwlgYC4yFsTAWGAtjYSwwFsbCWGAsjIWxwFgYC2OBsTAWxgJjYSyMBcbCWBgLjIWxMBYYC2NhLIwFxsJYGAuMhbEwFhgLY2EsMBa/LUvcAdaljmuGAAAAAElFTkSuQmCC";
    renders(&format!(
        "<svg width=20 height=20><image width=20 height=20 href='data:image/png;base64,{png}'/></svg>"
    ));
}

#[test]
fn reads_svg_images_with_other_white_space_in_their_root_tag() {
    // A no-break space between attributes was taken for white space of one byte, and the tag was
    // cut inside it.
    let svg = "<svg xmlns='http://www.w3.org/2000/svg'\u{a0}width='10' height='10'><rect width='5' height='5'/></svg>";
    let document = Document::html("<img src=s.svg><div style='background: url(s.svg)'>x</div>")
        .resource("s.svg", svg.as_bytes().to_vec());
    let output = Engine::new().render(&document);
    assert!(output.is_ok(), "{:?}", output.err());
    renders(&format!("<p>x</p>{svg}"));
}

#[test]
fn reads_srcset_descriptors_that_end_with_a_character_of_several_bytes() {
    // Found by the fuzz target `invariants`: the unit of a descriptor was taken for its last
    // byte, and the descriptor was cut inside the character.
    renders("<img src=a.png srcset='a.png 640w\u{f2}, b.png 2x'>");
    renders("<picture><source srcset='a.png 2\u{d7}'><img src=a.png></picture>");
}

/// An `<img>` of an SVG document, as a `data:` URL.
fn svg_image(svg: &str) -> String {
    let encoded: String = svg
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => char::from(byte).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect();
    format!("<img src='data:image/svg+xml,{encoded}'>")
}

#[test]
fn refuses_svgs_that_expand_to_gigabytes() {
    // Entities of entities: ten levels of ten references are 10^10 characters of text.
    let mut laughs = String::from(r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY l0 "lol">"#);
    for level in 1..10 {
        let _ = write!(
            laughs,
            r#"<!ENTITY l{level} "{}">"#,
            format!("&l{};", level - 1).repeat(10)
        );
    }
    laughs.push_str(
        r#"]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><text y="5">&l9;</text></svg>"#,
    );
    renders_quickly(&svg_image(&laughs), None, 5);
    // `<use>` of `<use>`: ten to the eighth copies of a rectangle.
    let mut uses = String::from(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><defs><rect id="u0" width="1" height="1"/>"#,
    );
    for level in 1..9 {
        let _ = write!(
            uses,
            r#"<g id="u{level}">{}</g>"#,
            format!(r##"<use xlink:href="#u{}"/>"##, level - 1).repeat(10)
        );
    }
    uses.push_str(r##"</defs><use xlink:href="#u8"/></svg>"##);
    renders_quickly(&svg_image(&uses), None, 5);
    // A thousand primitives in a filter, each kept as an image of the filter's region.
    let filter = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="1000"><filter id="f">{}</filter><rect width="1000" height="1000" filter="url(#f)"/></svg>"#,
        r#"<feFlood flood-color="red"/>"#.repeat(1000)
    );
    renders_quickly(&svg_image(&filter), None, 5);
    renders_quickly(&filter, None, 5);
    // SVGZ of a few hundred kilobytes that inflates past 128 MiB.
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(
        &mut gzip,
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><desc>"#,
    )
    .expect("written");
    let spaces = vec![b' '; 1 << 20];
    for _ in 0..130 {
        std::io::Write::write_all(&mut gzip, &spaces).expect("written");
    }
    let bomb = gzip.finish().expect("compressed");
    assert!(bomb.len() < 4 << 20);
    renders_quickly(
        "<img src='bomb.svgz'><div style='background: url(bomb.svgz)'>x</div>",
        Some(bomb),
        5,
    );
}
