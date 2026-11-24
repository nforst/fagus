//! Raster images as JPEG, for documents that ask for an image quality: PDF takes a JPEG as it
//! is (`DCTDecode`), where the pixels of other images are compressed without loss.
//!
//! Only images whose pixels are all opaque become a JPEG: one with transparent pixels keeps its
//! pixels and its alpha channel, as a JPEG has none. An image of a format without loss (PNG,
//! GIF, lossless WebP, BMP, TIFF) stays as it is unless the JPEG is smaller than its file: a
//! logo, a chart or a QR code compresses better without loss, and looks better.
//!
//! The encoder is given every parameter, and computes with integers only, so the same pixels
//! make the same bytes on every platform: a baseline JPEG, its chroma at half the resolution
//! (4:2:0) below a quality of 90 and at full resolution (4:4:4) from there, and one gray
//! component where all pixels are gray. It has the Huffman tables of the standard and all its
//! components in one scan, the plainest JPEG there is: tables made for the image would save a
//! few percent (3 % of a photo at a quality of 80), but the encoder then writes each component
//! in a scan of its own, which not every decoder reads right where the chroma is halved
//! (zune-jpeg 0.5.15 does not).

use std::io::Cursor;

use fagus_layout::ImageFormat;
use jpeg_encoder::{ChromaSubsamplingMethod, ColorType, Encoder, SamplingFactor};
use krilla::image::Image;

/// The quality from which the chroma keeps its full resolution.
const FULL_CHROMA: u8 = 90;

/// The image `data` of `format` as a JPEG of `quality` (1 to 100); `None` where it stays as it
/// is: a JPEG that PDF takes itself, an image with transparent pixels, one without loss whose
/// JPEG is no smaller than its file, a gray PNG with an ICC profile (which does not fit the
/// decoded RGB pixels), an SVG or PDF image, and one that does not decode.
pub(crate) fn image(data: &[u8], format: ImageFormat, quality: u8, interpolate: bool) -> Option<Image> {
    let source = source(data, format)?;
    let jpeg = encode(
        source.width,
        source.height,
        &source.pixels,
        source.icc.as_deref(),
        quality,
    )?;
    if source.lossless && jpeg.len() >= data.len() {
        return None;
    }
    Image::from_jpeg(jpeg.into(), interpolate).ok()
}

/// The pixels of an image after its filters (see [`crate::decode::filtered_pixels`]) as a JPEG
/// of `quality`, under the rules of [`image`]; `source` and `format` are the image's file. An
/// SVG image drawn to be filtered stays as it is: its pixels are made, not a photo.
pub(crate) fn filtered(
    (width, height, pixels): (usize, usize, &[u8]),
    (source, format): (&[u8], ImageFormat),
    quality: u8,
    interpolate: bool,
) -> Option<Image> {
    let lossless = match format {
        ImageFormat::Svg | ImageFormat::Pdf(_) => return None,
        ImageFormat::Jpeg | ImageFormat::Avif => false,
        ImageFormat::Webp => !lossy_webp(source),
        ImageFormat::Png | ImageFormat::Gif | ImageFormat::Bmp | ImageFormat::Tiff => true,
    };
    let jpeg = encode(
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
        pixels,
        None,
        quality,
    )?;
    if lossless && jpeg.len() >= source.len() {
        return None;
    }
    Image::from_jpeg(jpeg.into(), interpolate).ok()
}

/// An image that may become a JPEG, decoded.
struct Source {
    width: u32,
    height: u32,
    /// RGBA.
    pixels: Vec<u8>,
    /// The ICC profile its colors are in.
    icc: Option<Vec<u8>>,
    /// Whether its format is one without loss.
    lossless: bool,
}

/// The pixels of an image that may become a JPEG; `None` for one that stays as it is whatever
/// its pixels are, and one that does not decode.
fn source(data: &[u8], format: ImageFormat) -> Option<Source> {
    let (icc, lossless) = match format {
        ImageFormat::Png => {
            let decoder = png::Decoder::new(Cursor::new(data));
            let reader = decoder.read_info().ok()?;
            let info = reader.info();
            let gray = matches!(
                info.color_type,
                png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha
            );
            if gray && info.icc_profile.is_some() {
                return None;
            }
            (info.icc_profile.as_ref().map(|icc| icc.to_vec()), true)
        }
        ImageFormat::Webp => {
            let mut decoder = image_webp::WebPDecoder::new(Cursor::new(data)).ok()?;
            (decoder.icc_profile().ok()?, !decoder.is_lossy())
        }
        ImageFormat::Gif | ImageFormat::Bmp | ImageFormat::Tiff => (None, true),
        ImageFormat::Avif => {
            let avif = crate::avif::decode(data)?;
            return Some(Source {
                width: avif.width,
                height: avif.height,
                pixels: avif.pixels,
                icc: avif.icc,
                lossless: false,
            });
        }
        // Only a CMYK JPEG without a profile is decoded (see [`crate::decode::cmyk_jpeg`]); PDF
        // takes every other JPEG as it is.
        ImageFormat::Jpeg => {
            let (width, height, color) = crate::decode::cmyk_rgb(data)?;
            let pixels = color
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|[r, g, b]| [*r, *g, *b, 255])
                .collect();
            return Some(Source {
                width,
                height,
                pixels,
                icc: None,
                lossless: false,
            });
        }
        ImageFormat::Svg | ImageFormat::Pdf(_) => return None,
    };
    let (width, height, pixels) = crate::decode::rgba(data, format)?;
    Some(Source {
        width,
        height,
        pixels,
        icc,
        lossless,
    })
}

/// Whether a WebP image is compressed with loss (VP8, not VP8L).
fn lossy_webp(data: &[u8]) -> bool {
    image_webp::WebPDecoder::new(Cursor::new(data)).is_ok_and(|mut decoder| decoder.is_lossy())
}

/// RGBA pixels of `width` by `height` as a JPEG of `quality`, with the ICC profile `icc` their
/// colors are in; `None` if a pixel is not opaque, for an image with a side of no pixels or
/// more than the 65,535 a JPEG can have, and for a quality outside 1 to 100.
pub(crate) fn encode(
    width: u32,
    height: u32,
    pixels: &[u8],
    icc: Option<&[u8]>,
    quality: u8,
) -> Option<Vec<u8>> {
    let (columns, rows) = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
    if columns == 0
        || rows == 0
        || !(1..=100).contains(&quality)
        || pixels.len() != usize::from(columns) * usize::from(rows) * 4
    {
        return None;
    }
    let pixels = pixels.as_chunks::<4>().0;
    if pixels.iter().any(|pixel| pixel[3] != 255) {
        return None;
    }
    let mut jpeg = Vec::new();
    let mut encoder = Encoder::new(&mut jpeg, quality);
    encoder.set_sampling_factor(if quality < FULL_CHROMA {
        SamplingFactor::F_2_2
    } else {
        SamplingFactor::F_1_1
    });
    encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
    encoder.set_optimized_huffman_tables(false);
    // A profile describes RGB colors, so an image with one keeps its three components.
    let gray = icc.is_none() && pixels.iter().all(|[r, g, b, _]| r == g && g == b);
    if let Some(icc) = icc {
        encoder.add_icc_profile(icc).ok()?;
    }
    if gray {
        let luma: Vec<u8> = pixels.iter().map(|pixel| pixel[0]).collect();
        encoder.encode(&luma, columns, rows, ColorType::Luma).ok()?;
    } else {
        encoder
            .encode(pixels.as_flattened(), columns, rows, ColorType::Rgba)
            .ok()?;
    }
    Some(jpeg)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    /// A picture of `width` by `height` that needs no file: a gradient in each channel with
    /// noise from a linear congruential generator, as RGBA.
    fn picture(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x2545_f491_u32;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 27) as u8;
                pixels.extend([
                    (x * 255 / (width - 1)) as u8 / 2 + noise,
                    (y * 255 / (height - 1)) as u8 / 2 + noise * 2,
                    ((x + y) * 255 / (width + height - 2)) as u8 / 2 + 64 - noise,
                    255,
                ]);
            }
        }
        pixels
    }

    fn sha256(data: &[u8]) -> String {
        use std::fmt::Write;
        Sha256::digest(data).iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    fn decoded(jpeg: &[u8]) -> (usize, Vec<u8>) {
        use zune_jpeg::zune_core::colorspace::ColorSpace;
        use zune_jpeg::zune_core::options::DecoderOptions;
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(jpeg), options);
        let pixels = decoder.decode().expect("a JPEG");
        let components = decoder
            .input_colorspace()
            .expect("a color space")
            .num_components();
        (components, pixels)
    }

    /// The peak signal-to-noise ratio of RGB pixels against the RGBA ones they were made from,
    /// in decibels.
    fn psnr(original: &[u8], rgb: &[u8]) -> f64 {
        let mut error = 0_u64;
        for (before, after) in original.as_chunks::<4>().0.iter().zip(rgb.as_chunks::<3>().0) {
            for channel in 0..3 {
                let difference = u64::from(before[channel].abs_diff(after[channel]));
                error += difference * difference;
            }
        }
        let mean = error as f64 / rgb.len() as f64;
        10.0 * libm::log10(255.0 * 255.0 / mean)
    }

    // The gate "same bytes on every platform" for the encoder: these hashes are those of
    // macOS on arm64, and CI runs the test on Linux, Windows and FreeBSD on x64 too.
    #[test]
    fn writes_the_same_bytes_on_every_platform() {
        let pixels = picture(67, 45);
        let hashes = [
            (
                80,
                "95c1c9de8fdf7a4178626e7cd2cd80947ff67b42301d40ca938ad646d7e8aa04",
            ),
            (
                95,
                "41d814da1d4d7c1e90a17cbc8d11f75a8e9011913ea05d38243fa72ac983d8fe",
            ),
        ];
        for (quality, hash) in hashes {
            let jpeg = encode(67, 45, &pixels, None, quality).expect("a JPEG");
            assert_eq!(sha256(&jpeg), hash, "quality {quality}");
        }
        let gray: Vec<u8> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], 255])
            .collect();
        let jpeg = encode(67, 45, &gray, None, 80).expect("a JPEG");
        assert_eq!(
            sha256(&jpeg),
            "55a7e3741a0675e140be97e0e097bfa08c7c26daaed247349acba2523562eb92"
        );
    }

    #[test]
    fn is_smaller_the_lower_the_quality() {
        let pixels = picture(160, 120);
        let size = |quality| encode(160, 120, &pixels, None, quality).expect("a JPEG").len();
        let sizes = [1, 25, 50, 80, 89, 90, 95, 100].map(size);
        assert!(sizes.is_sorted_by(|a, b| a < b), "{sizes:?}");
    }

    #[test]
    fn stays_close_to_the_pixels() {
        // Smooth, as a photo is: noise is what a JPEG drops first.
        let mut pixels = Vec::new();
        for y in 0..96_u32 {
            for x in 0..128_u32 {
                pixels.extend([(x * 2) as u8, (y * 2 + x / 2) as u8, 255 - (x + y) as u8, 255]);
            }
        }
        for (quality, least) in [(50, 32.0), (80, 35.0), (95, 40.0)] {
            let jpeg = encode(128, 96, &pixels, None, quality).expect("a JPEG");
            let (components, rgb) = decoded(&jpeg);
            assert_eq!(components, 3);
            let psnr = psnr(&pixels, &rgb);
            assert!(psnr > least, "quality {quality}: {psnr} dB");
        }
    }

    #[test]
    fn halves_the_chroma_below_a_quality_of_90() {
        // The sampling factors of the first component in the frame header (SOF0): 2 × 2 where
        // the chroma has half the resolution.
        let sampling = |jpeg: &[u8]| {
            let frame = jpeg
                .windows(2)
                .position(|marker| marker == [0xff, 0xc0])
                .expect("a baseline frame");
            jpeg[frame + 11]
        };
        let pixels = picture(32, 32);
        assert_eq!(
            sampling(&encode(32, 32, &pixels, None, 89).expect("a JPEG")),
            0x22
        );
        assert_eq!(
            sampling(&encode(32, 32, &pixels, None, 90).expect("a JPEG")),
            0x11
        );
        // Never progressive (SOF2), and all components in one scan (SOS).
        let jpeg = encode(32, 32, &pixels, None, 80).expect("a JPEG");
        assert!(!jpeg.windows(2).any(|marker| marker == [0xff, 0xc2]));
        let scans = jpeg
            .windows(4)
            .filter(|marker| *marker == [0xff, 0xda, 0x00, 0x0c])
            .count();
        assert_eq!(scans, 1);
    }

    #[test]
    fn writes_gray_pixels_as_one_component() {
        let gray: Vec<u8> = (0..64_u32 * 64)
            .flat_map(|index| {
                let value = (index % 64 * 4) as u8;
                [value, value, value, 255]
            })
            .collect();
        let jpeg = encode(64, 64, &gray, None, 80).expect("a JPEG");
        assert_eq!(decoded(&jpeg).0, 1);
        // With a profile, which describes RGB colors, three.
        let profile = vec![7_u8; 300];
        let jpeg = encode(64, 64, &gray, Some(&profile), 80).expect("a JPEG");
        assert_eq!(decoded(&jpeg).0, 3);
    }

    #[test]
    fn carries_the_icc_profile() {
        // More than one APP2 segment holds (65,519 bytes each).
        let profile: Vec<u8> = (0..70_000_u32).map(|index| (index % 251) as u8).collect();
        let jpeg = encode(16, 16, &picture(16, 16), Some(&profile), 80).expect("a JPEG");
        let mut decoder = zune_jpeg::JpegDecoder::new(Cursor::new(&jpeg));
        decoder.decode_headers().expect("headers");
        assert_eq!(decoder.icc_profile(), Some(profile));
    }

    #[test]
    fn leaves_images_with_transparent_pixels() {
        let mut pixels = picture(16, 16);
        assert!(encode(16, 16, &pixels, None, 80).is_some());
        pixels[4 * 100 + 3] = 254;
        assert!(encode(16, 16, &pixels, None, 80).is_none());
    }

    #[test]
    fn refuses_sizes_and_qualities_a_jpeg_cannot_have() {
        let pixels = picture(16, 16);
        assert!(encode(16, 16, &pixels, None, 0).is_none());
        assert!(encode(16, 16, &pixels, None, 101).is_none());
        assert!(encode(16, 15, &pixels, None, 80).is_none());
        assert!(encode(0, 16, &[], None, 80).is_none());
        assert!(encode(65_536, 1, &[], None, 80).is_none());
        assert!(encode(1, 1, &[1, 2, 3, 255], None, 1).is_some());
        assert!(encode(1, 1, &[1, 2, 3, 255], None, 100).is_some());
    }

    fn png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(png::ColorType::Rgba);
            let mut writer = encoder.write_header().expect("a header");
            writer.write_image_data(pixels).expect("pixels");
        }
        png
    }

    #[test]
    fn keeps_a_lossless_image_whose_jpeg_is_no_smaller() {
        // Noise, which PNG cannot compress: the JPEG is smaller.
        let photo = png(160, 120, &picture(160, 120));
        assert!(image(&photo, ImageFormat::Png, 80, false).is_some());
        // Two flat colors, as a logo: a few bytes as PNG.
        let logo: Vec<u8> = (0..160_u32 * 120)
            .flat_map(|index| {
                if index % 160 < 80 {
                    [200, 30, 30, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();
        let logo = png(160, 120, &logo);
        assert!(image(&logo, ImageFormat::Png, 80, false).is_none());
        // With a transparent pixel, the photo keeps its pixels.
        let mut pixels = picture(160, 120);
        pixels[3] = 0;
        let transparent = png(160, 120, &pixels);
        assert!(image(&transparent, ImageFormat::Png, 80, false).is_none());
        // What is no image stays what it is.
        assert!(image(b"no image", ImageFormat::Png, 80, false).is_none());
        assert!(image(b"<svg/>", ImageFormat::Svg, 80, false).is_none());
    }
}
