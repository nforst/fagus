//! `Document::image_quality`: opaque images written as JPEGs. The PDFs are read back with
//! hayro-syntax, and their JPEGs told apart from compressed pixels by their filter.

use fagus::{Conformance, Date, Document, Engine, Error, PdfError};
use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Dict, Name, Stream};

const CREATED: Date = Date::new(2026, 11, 25, 12, 0, 0);

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
        .expect("fonts");
    engine
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/../../tests/corpus/images/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).expect("fixture")
}

/// A page that shows the image `name`, written with `quality`.
fn page(name: &str, quality: Option<u8>) -> Document {
    let mut document =
        Document::html("<!doctype html><html lang=en><title>Image</title><img src=i alt=Image width=240>")
            .resource("i", fixture(name))
            .created(CREATED)
            .tagged(false);
    if let Some(quality) = quality {
        document = document.image_quality(quality);
    }
    document
}

fn render(document: &Document) -> Vec<u8> {
    let output = engine().render(document).expect("rendered");
    let codes: Vec<&str> = output
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code)
        .collect();
    assert_eq!(codes, Vec::<&str>::new());
    output.pdf
}

/// An image of a PDF as it is stored.
#[derive(Debug, PartialEq)]
struct Picture {
    /// `DCTDecode` for a JPEG, `FlateDecode` for compressed pixels.
    filter: String,
    /// Whether it has a soft mask: an alpha channel.
    mask: bool,
    /// `DeviceRGB`, `DeviceGray` or `ICCBased`.
    color: String,
    /// The size of its ICC profile, if it has one.
    profile: Option<usize>,
    /// Its data, as stored.
    data: Vec<u8>,
}

/// The images that the pages of `pdf` and their form `XObject`s draw.
fn pictures(pdf: &[u8]) -> Vec<Picture> {
    fn collect(resources: &Dict<'_>, depth: u32, out: &mut Vec<Picture>) {
        let Some(named) = resources.get::<Dict<'_>>(b"XObject") else {
            return;
        };
        for name in named.keys() {
            let Some(stream) = named.get::<Stream<'_>>(name.as_ref()) else {
                continue;
            };
            let dict = stream.dict();
            if dict.get::<Name<'_>>(b"Subtype").as_deref() != Some(b"Image") {
                if depth < 8
                    && let Some(inner) = dict.get::<Dict<'_>>(b"Resources")
                {
                    collect(&inner, depth + 1, out);
                }
                continue;
            }
            let name = |key: &[u8]| {
                dict.get::<Name<'_>>(key)
                    .map(|name| String::from_utf8_lossy(&name).into_owned())
            };
            // A name, or `[/ICCBased profile]`.
            let space = dict.get::<Array<'_>>(b"ColorSpace").map(|space| {
                let mut items = space.flex_iter();
                let family = items.next::<Name<'_>>();
                let profile = items.next::<Stream<'_>>();
                (
                    family.map(|name| String::from_utf8_lossy(&name).into_owned()),
                    profile.and_then(|profile| profile.decoded().ok().map(|data| data.len())),
                )
            });
            let (color, profile) = match space {
                Some((family, profile)) => (family.unwrap_or_default(), profile),
                None => (name(b"ColorSpace").unwrap_or_default(), None),
            };
            out.push(Picture {
                filter: name(b"Filter").unwrap_or_default(),
                mask: dict.contains_key(b"SMask"),
                color,
                profile,
                data: stream.raw_data().to_vec(),
            });
        }
    }
    let pdf = Pdf::new(pdf.to_vec()).expect("a PDF hayro reads");
    let mut out = Vec::new();
    for page in pdf.pages().iter() {
        if let Some(resources) = page.raw().get::<Dict<'_>>(b"Resources") {
            collect(&resources, 0, &mut out);
        }
    }
    out
}

/// The one image of the page that shows `name`, written with `quality`, and the size of the PDF.
fn picture(name: &str, quality: Option<u8>) -> (Picture, usize) {
    let pdf = render(&page(name, quality));
    let mut pictures = pictures(&pdf);
    assert_eq!(pictures.len(), 1, "{name}");
    (pictures.remove(0), pdf.len())
}

#[test]
fn compresses_images_without_loss_unless_asked() {
    for name in ["photo.webp", "photo.png", "lossless.webp", "alpha.webp"] {
        let (picture, _) = picture(name, None);
        assert_eq!(picture.filter, "FlateDecode", "{name}");
    }
}

#[test]
fn writes_photos_as_jpegs_a_fraction_of_the_size() {
    for name in ["photo.webp", "photo.png"] {
        let (lossless, before) = picture(name, None);
        let (jpeg, after) = picture(name, Some(80));
        assert_eq!(
            (jpeg.filter.as_str(), jpeg.mask, jpeg.color.as_str()),
            ("DCTDecode", false, "DeviceRGB"),
            "{name}"
        );
        assert!(jpeg.data.starts_with(&[0xff, 0xd8]), "{name}");
        // The pixels of the photo are a third of a megabyte compressed without loss, a tenth of
        // it as a JPEG.
        assert!(
            jpeg.data.len() * 5 < lossless.data.len(),
            "{name}: {} of {}",
            jpeg.data.len(),
            lossless.data.len()
        );
        assert!(after * 3 < before, "{name}: {after} of {before}");
    }
}

#[test]
fn makes_smaller_files_the_lower_the_quality() {
    let sizes = [1, 30, 50, 80, 95, 100].map(|quality| picture("photo.webp", Some(quality)).1);
    assert!(sizes.is_sorted_by(|a, b| a < b), "{sizes:?}");
    // The lossless PDF is larger than the best JPEG.
    assert!(sizes[5] < picture("photo.webp", None).1);
}

#[test]
fn leaves_images_with_transparent_pixels_as_they_are() {
    let (lossless, _) = picture("alpha.webp", None);
    let (kept, _) = picture("alpha.webp", Some(80));
    assert_eq!((kept.filter.as_str(), kept.mask), ("FlateDecode", true));
    assert_eq!(kept, lossless);
}

#[test]
fn leaves_lossless_images_that_are_smaller_as_they_are() {
    // A chart of flat colors, and a logo: a JPEG of them is larger than their files.
    for name in ["lossless.webp", "logo.svg", "pattern.png"] {
        let with = render(&page(name, Some(80)));
        assert_eq!(pictures(&with), pictures(&render(&page(name, None))), "{name}");
        assert!(
            pictures(&with)
                .iter()
                .all(|picture| picture.filter != "DCTDecode"),
            "{name}"
        );
    }
}

#[test]
fn embeds_jpeg_images_as_they_are() {
    let source = fixture("photo.jpg");
    for quality in [None, Some(10), Some(80)] {
        let (picture, _) = picture("photo.jpg", quality);
        assert_eq!(picture.filter, "DCTDecode");
        assert_eq!(picture.data, source, "{quality:?}");
    }
}

#[test]
fn writes_the_same_bytes_without_the_option_and_again_with_it() {
    // The quality changes nothing in a document without images it applies to.
    let plain = "<!doctype html><title>Text</title><p>No image</p>";
    let document = |quality: Option<u8>| {
        let document = Document::html(plain).created(CREATED);
        match quality {
            Some(quality) => document.image_quality(quality),
            None => document,
        }
    };
    assert_eq!(render(&document(None)), render(&document(Some(80))));
    // And a photo is the same JPEG each time, in a new engine and in a warm one.
    let first = render(&page("photo.webp", Some(80)));
    assert_eq!(first, render(&page("photo.webp", Some(80))));
    let engine = engine();
    for _ in 0..3 {
        assert_eq!(
            engine
                .render(&page("photo.webp", Some(80)))
                .expect("rendered")
                .pdf,
            first
        );
    }
}

#[test]
fn keeps_the_images_of_each_quality_apart_in_one_engine() {
    // An engine keeps prepared images for its next documents: by their quality too.
    let engine = engine();
    let render = |quality| engine.render(&page("photo.webp", quality)).expect("rendered").pdf;
    let (low, high, lossless) = (render(Some(60)), render(Some(90)), render(None));
    assert!(low.len() < high.len() && high.len() < lossless.len());
    assert_eq!(render(Some(60)), low);
    assert_eq!(render(None), lossless);
    assert_eq!(render(Some(90)), high);
    assert_eq!(low, self::render(&page("photo.webp", Some(60))));
    assert_eq!(lossless, self::render(&page("photo.webp", None)));
}

#[test]
fn refuses_a_quality_outside_1_to_100() {
    for quality in [0, 101, 255] {
        let error = engine()
            .render(&page("photo.webp", Some(quality)))
            .expect_err("no quality");
        assert!(
            matches!(error, Error::Pdf(PdfError::ImageQuality(refused)) if refused == quality),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("the image quality must be between 1 and 100, not {quality}")
        );
    }
    for quality in [1, 100] {
        assert_eq!(picture("photo.webp", Some(quality)).0.filter, "DCTDecode");
    }
}

#[test]
fn writes_jpegs_in_pdf_a_and_pdf_ua() {
    for conformance in [
        Conformance::PdfA1b,
        Conformance::PdfA2b,
        Conformance::PdfA3u,
        Conformance::PdfA4,
    ] {
        for name in ["photo.webp", "photo.png"] {
            let pdf = render(&page(name, Some(80)).conformance(conformance));
            let pictures = pictures(&pdf);
            assert_eq!(pictures.len(), 1, "{conformance:?} {name}");
            assert_eq!(pictures[0].filter, "DCTDecode", "{conformance:?} {name}");
            assert!(!pictures[0].mask);
        }
        // A chart stays pixels without loss, as PDF/A takes them.
        let pdf = render(&page("lossless.webp", Some(80)).conformance(conformance));
        assert_eq!(pictures(&pdf)[0].filter, "FlateDecode", "{conformance:?}");
    }
    let accessible = render(&page("photo.webp", Some(80)).tagged(true).accessible());
    assert_eq!(pictures(&accessible)[0].filter, "DCTDecode");
    // PDF/A-1 has no transparency: an image with transparent pixels is refused with or without
    // a quality, and written with its alpha channel from PDF/A-2 on.
    for quality in [None, Some(80)] {
        let mut document = page("alpha.webp", quality).conformance(Conformance::PdfA1b);
        let refused = engine().render(&document).expect_err("transparency");
        assert!(
            matches!(refused, Error::Pdf(PdfError::Conformance { .. })),
            "{refused}"
        );
        document = page("alpha.webp", quality).conformance(Conformance::PdfA2b);
        assert!(pictures(&render(&document))[0].mask);
    }
}

#[test]
fn carries_the_icc_profile_of_an_image_into_its_jpeg() {
    let read = |quality: Option<u8>| {
        let mut document = Document::html("<img src=i width=100>")
            .resource("i", fixture("media/avif/icc.avif"))
            .created(CREATED)
            .tagged(false);
        if let Some(quality) = quality {
            document = document.image_quality(quality);
        }
        let mut pictures = pictures(&render(&document));
        assert_eq!(pictures.len(), 1);
        pictures.remove(0)
    };
    let (lossless, jpeg) = (read(None), read(Some(80)));
    assert_eq!(
        (
            lossless.filter.as_str(),
            lossless.color.as_str(),
            lossless.profile
        ),
        ("FlateDecode", "ICCBased", Some(736))
    );
    assert_eq!(
        (jpeg.filter.as_str(), jpeg.color.as_str(), jpeg.profile),
        ("DCTDecode", "ICCBased", Some(736))
    );
}

#[test]
fn writes_images_of_every_format_as_jpegs_where_their_pixels_allow() {
    // (image, whether it is a JPEG with a quality): opaque AVIF, BMP and CMYK JPEG images are
    // decoded and written as JPEGs; images with transparent pixels and bilevel scans, which
    // are smaller without loss, are not.
    for (name, jpeg) in [
        ("media/photo.avif", true),
        ("media/avif/opaque.avif", true),
        ("media/avif/alpha.avif", false),
        ("media/cmyk.jpg", true),
        ("media/scan-g4.tif", false),
        ("media/logo-alpha.bmp", false),
        ("media/anim.gif", false),
        ("stamp.png", false),
    ] {
        let document = |quality: Option<u8>| {
            let document = Document::html("<img src=i width=100>")
                .resource("i", fixture(name))
                .created(CREATED)
                .tagged(false);
            match quality {
                Some(quality) => document.image_quality(quality),
                None => document,
            }
        };
        let before = pictures(&render(&document(None)));
        let after = pictures(&render(&document(Some(80))));
        assert_eq!((before.len(), after.len()), (1, 1), "{name}");
        assert_eq!(before[0].filter, "FlateDecode", "{name}");
        assert_eq!(after[0].filter == "DCTDecode", jpeg, "{name}");
        if !jpeg {
            assert_eq!(before, after, "{name}");
        }
    }
}

#[test]
fn writes_filtered_images_as_jpegs_and_gray_ones_with_one_component() {
    let document = |style: &str, quality: Option<u8>| {
        let document = Document::html(format!("<img src=i width=240 style='{style}'>"))
            .resource("i", fixture("photo.webp"))
            .created(CREATED)
            .tagged(false);
        match quality {
            Some(quality) => document.image_quality(quality),
            None => document,
        }
    };
    let gray = pictures(&render(&document("filter: grayscale(1)", Some(80))));
    assert_eq!(gray.len(), 1);
    assert_eq!(
        (gray[0].filter.as_str(), gray[0].color.as_str()),
        ("DCTDecode", "DeviceGray")
    );
    let sepia = pictures(&render(&document("filter: sepia(1)", Some(80))));
    assert_eq!(
        (sepia[0].filter.as_str(), sepia[0].color.as_str()),
        ("DCTDecode", "DeviceRGB")
    );
    assert_eq!(
        pictures(&render(&document("filter: sepia(1)", None)))[0].filter,
        "FlateDecode"
    );
    // A blur reaches beyond the image with transparent pixels: no JPEG.
    let blurred = pictures(&render(&document("filter: blur(4px)", Some(80))));
    assert!(blurred.iter().all(|picture| picture.filter == "FlateDecode"));
}
