//! Decodes arbitrary image data (PNG, JPEG, GIF, WebP, BMP, TIFF, AVIF) as the image of an
//! element, a background, a mask, a border image, an image in SVG and an image whose pixels a
//! filter changes. Data of an odd length is written with an image quality (1 to 100, from the
//! length), so that the JPEG encoder sees the decoded pixels too.
#![no_main]

use fagus::Document;
use libfuzzer_sys::fuzz_target;

const HTML: &str = "<img src=i><img src=i style='width: 30px; image-orientation: none'>\
    <img src=i style='width: 20px; filter: grayscale(1) blur(1px)'>\
    <div style='width: 20px; height: 20px; background: url(i) 0 0 / 7px; mask-image: url(i)'></div>\
    <div style='width: 10px; border: 5px solid; border-image: url(i) 30%'></div>\
    <svg width=20 height=20><image href=i width=20 height=20 /></svg>";

fuzz_target!(|data: &[u8]| {
    let mut document = Document::html(HTML).resource("i", data.to_vec());
    if data.len() % 2 == 1 {
        document = document.image_quality((data.len() / 2 % 100 + 1) as u8);
    }
    fagus_fuzz::render(&document);
});
