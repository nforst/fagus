//! The pixels of raster images, for what `filter` does to them: PDF cannot filter an image, so a
//! filtered one is embedded as the pixels it filters to.

use std::io::Cursor;

use fagus_layout::{ImageFormat, PixelFilter};
use fagus_style::values::Rgba;
use krilla::image::Image;

use crate::raster::RgbaImage;

/// The most pixels Fagus blurs: the blur needs sixteen bytes per pixel.
const MAX_BLURRED: usize = 25_000_000;

/// The pixels of an image after its filters, with its padding around them, as an image to
/// embed; `None` if it cannot be decoded. `decoded` are its pixels where they do not come from
/// its data (an SVG image, drawn), unpremultiplied RGBA.
/// With a `quality`, they are a JPEG of it where
/// [`crate::lossy`] makes one.
pub(crate) fn filtered(
    image: &fagus_layout::Image,
    interpolate: bool,
    decoded: Option<(u32, u32, Vec<u8>)>,
    quality: Option<u8>,
) -> Option<Image> {
    let (width, height, pixels) = filtered_pixels(image, decoded)?;
    // As a JPEG where the document asks for one and the pixels allow it.
    if let Some(jpeg) = quality.and_then(|quality| {
        crate::lossy::filtered(
            (width, height, &pixels),
            (&image.data, image.format),
            quality,
            interpolate,
        )
    }) {
        return Some(jpeg);
    }
    let mut color = Vec::with_capacity(width * height * 3);
    let mut alpha = Vec::with_capacity(width * height);
    for [r, g, b, a] in pixels.as_chunks::<4>().0 {
        color.extend([*r, *g, *b]);
        alpha.push(*a);
    }
    // An opaque image needs no alpha.
    if alpha.iter().all(|value| *value == 255) {
        alpha.clear();
    }
    Image::from_custom(
        RgbaImage {
            color,
            alpha,
            width: width as u32,
            height: height as u32,
        },
        interpolate,
    )
    .ok()
}

/// The pixels of an image after its filters, with its padding around them, as unpremultiplied
/// RGBA, and their width and height; `None` if it cannot be decoded (see [`filtered`]).
pub(crate) fn filtered_pixels(
    image: &fagus_layout::Image,
    decoded: Option<(u32, u32, Vec<u8>)>,
) -> Option<(usize, usize, Vec<u8>)> {
    let (width, height, pixels) = match decoded {
        Some(decoded) => decoded,
        None => rgba(&image.data, image.format)?,
    };
    if pixels.len() != width as usize * height as usize * 4 {
        return None;
    }
    // A part of the image comes first: the filters apply to it.
    let (width, height, pixels) = match image.filters.first() {
        Some(PixelFilter::Part(bounds)) => part(&pixels, (width, height), image.orientation, *bounds)?,
        _ => (width, height, pixels),
    };
    let pad = image.pad as usize;
    let (width, height) = (width as usize + 2 * pad, height as usize + 2 * pad);
    let mut padded = vec![0_u8; width * height * 4];
    let stored = width - 2 * pad;
    for (row, line) in pixels.chunks_exact(stored * 4).enumerate() {
        let start = ((row + pad) * width + pad) * 4;
        padded[start..start + line.len()].copy_from_slice(line);
    }
    let mut pixels = padded;
    // After a silhouette, all pixels have its color.
    let mut uniform = None;
    for filter in image.filters.iter() {
        match filter {
            PixelFilter::Color(filter) => {
                if let Some(color) = &mut uniform {
                    *color = filter.apply(*color);
                }
                for pixel in pixels.as_chunks_mut::<4>().0 {
                    let [r, g, b, a] = pixel.map(|value| f32::from(value) / 255.0);
                    let color = filter.apply(Rgba { r, g, b, a });
                    *pixel = [color.r, color.g, color.b, color.a].map(crate::channel);
                }
            }
            PixelFilter::Silhouette(color) => {
                let rgb = [
                    crate::channel(color.r),
                    crate::channel(color.g),
                    crate::channel(color.b),
                ];
                for pixel in pixels.as_chunks_mut::<4>().0 {
                    *pixel = [
                        rgb[0],
                        rgb[1],
                        rgb[2],
                        (f32::from(pixel[3]) * color.a).round() as u8,
                    ];
                }
                uniform = Some(Rgba { a: 1.0, ..*color });
            }
            PixelFilter::Blur(sigma) if width * height <= MAX_BLURRED => {
                blur(&mut pixels, width, height, *sigma, uniform.is_some());
            }
            PixelFilter::Blur(_) => {}
            PixelFilter::Part(_) => {}
            PixelFilter::Crop { bounds, radii } => {
                crop(
                    &mut pixels,
                    (width, height, pad),
                    image.orientation,
                    (*bounds, *radii),
                );
            }
        }
    }
    Some((width, height, pixels))
}

/// The pixels of an image of `width` by `height` within `bounds` (fractions of the image as
/// it is seen, turned by its EXIF `orientation`), as an image of their own, stored as the image
/// is.
fn part(
    pixels: &[u8],
    (width, height): (u32, u32),
    orientation: u8,
    bounds: [f32; 4],
) -> Option<(u32, u32, Vec<u8>)> {
    let (columns, rows) = (width as f32, height as f32);
    let (wide, high) = if orientation >= 5 {
        (rows, columns)
    } else {
        (columns, rows)
    };
    let [left, top, right, bottom] = [
        bounds[0] * wide,
        bounds[1] * high,
        bounds[2] * wide,
        bounds[3] * high,
    ];
    // The bounds among the stored pixels: as the PDF writer turns the image, reversed.
    let [x0, y0, x1, y1] = match orientation {
        2 => [wide - right, top, wide - left, bottom],
        3 => [wide - right, high - bottom, wide - left, high - top],
        4 => [left, high - bottom, right, high - top],
        5 => [top, left, bottom, right],
        6 => [top, wide - right, bottom, wide - left],
        7 => [high - bottom, wide - right, high - top, wide - left],
        8 => [high - bottom, left, high - top, right],
        _ => [left, top, right, bottom],
    };
    let pixel = |value: f32, limit: u32| (value.round().max(0.0) as u32).min(limit);
    let (x0, x1) = (pixel(x0, width), pixel(x1, width));
    let (y0, y1) = (pixel(y0, height), pixel(y1, height));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let mut part = Vec::with_capacity(((x1 - x0) * (y1 - y0) * 4) as usize);
    for row in y0..y1 {
        let start = ((row * width + x0) * 4) as usize;
        part.extend_from_slice(&pixels[start..start + ((x1 - x0) * 4) as usize]);
    }
    Some((x1 - x0, y1 - y0, part))
}

/// Leaves only the pixels within `bounds`, with rounded corners of `radii` (fractions of the
/// image as it is seen, turned by its EXIF `orientation`), of an image of `width` by `height`
/// pixels with `pad` of them around it; pixels at the edge are kept as far as they lie within.
fn crop(
    pixels: &mut [u8],
    (width, height, pad): (usize, usize, usize),
    orientation: u8,
    (bounds, radii): ([f32; 4], Option<[(f32, f32); 4]>),
) {
    let (columns, rows) = ((width - 2 * pad) as f32, (height - 2 * pad) as f32);
    let (wide, high) = if orientation >= 5 {
        (rows, columns)
    } else {
        (columns, rows)
    };
    let area = fagus_paint::RectF {
        x: bounds[0] * wide,
        y: bounds[1] * high,
        width: (bounds[2] - bounds[0]) * wide,
        height: (bounds[3] - bounds[1]) * high,
    };
    let radii = radii.map(|radii| radii.map(|(x, y)| (x * wide, y * high)));
    for (row, line) in pixels.chunks_exact_mut(width * 4).enumerate() {
        let down = row as f32 - pad as f32 + 0.5;
        for (column, pixel) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let across = column as f32 - pad as f32 + 0.5;
            // Where the stored pixel is seen, as the PDF writer turns the image.
            let (x, y) = match orientation {
                2 => (wide - across, down),
                3 => (wide - across, high - down),
                4 => (across, high - down),
                5 => (down, across),
                6 => (wide - down, across),
                7 => (wide - down, high - across),
                8 => (down, high - across),
                _ => (across, down),
            };
            let inside = crate::shadow::coverage(x, y, area, radii.as_ref(), 1.0);
            pixel[3] = (f32::from(pixel[3]) * inside).round() as u8;
        }
    }
}

/// Blurs RGBA pixels with a Gaussian of a standard deviation of `sigma` pixels: their colors
/// weighted by their alpha, as Filter Effects blurs. Where all pixels have one color, only the
/// alpha needs it.
fn blur(pixels: &mut [u8], width: usize, height: usize, sigma: f32, uniform: bool) {
    let pixels = pixels.as_chunks_mut::<4>().0;
    let mut alpha: Vec<f32> = pixels.iter().map(|pixel| f32::from(pixel[3])).collect();
    crate::shadow::gaussian(&mut alpha, width, height, sigma);
    if !uniform {
        let mut plane = vec![0.0_f32; width * height];
        for channel in 0..3 {
            for (value, pixel) in plane.iter_mut().zip(pixels.iter()) {
                *value = f32::from(pixel[channel]) * f32::from(pixel[3]);
            }
            crate::shadow::gaussian(&mut plane, width, height, sigma);
            for ((pixel, value), weight) in pixels.iter_mut().zip(&plane).zip(&alpha) {
                pixel[channel] = if *weight > 0.0 {
                    (value / weight).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                };
            }
        }
    }
    for (pixel, value) in pixels.iter_mut().zip(&alpha) {
        pixel[3] = value.round().clamp(0.0, 255.0) as u8;
    }
}

/// The alpha channel of a raster image, for `shape-outside`; `None` for SVG and PDF images and
/// images that cannot be decoded.
pub fn alpha(image: &fagus_layout::Image) -> Option<fagus_layout::AlphaMask> {
    let (width, height, pixels) = rgba(&image.data, image.format)?;
    let alpha = pixels.as_chunks::<4>().0.iter().map(|pixel| pixel[3]).collect();
    Some(fagus_layout::AlphaMask { width, height, alpha })
}

/// Decodes an image to 8-bit RGBA, row by row, as krilla would embed it: its width, height and
/// pixels.
pub(crate) fn rgba(data: &[u8], format: ImageFormat) -> Option<(u32, u32, Vec<u8>)> {
    match format {
        ImageFormat::Png => png(data),
        ImageFormat::Jpeg => jpeg(data),
        ImageFormat::Gif => gif(data),
        ImageFormat::Webp => webp(data),
        ImageFormat::Bmp => fagus_layout::bmp::decode(data),
        ImageFormat::Tiff => tiff(data),
        ImageFormat::Avif => crate::avif::decode(data).map(|avif| (avif.width, avif.height, avif.pixels)),
        ImageFormat::Svg | ImageFormat::Pdf(_) => None,
    }
}

/// A BMP, TIFF or AVIF image as its pixels, which krilla cannot read itself: RGB, with an alpha
/// channel unless all its pixels are opaque, and an AVIF image's ICC profile. `None` for other
/// images and those that do not decode.
pub(crate) fn decoded(data: &[u8], format: ImageFormat, interpolate: bool) -> Option<Image> {
    if format == ImageFormat::Avif {
        let avif = crate::avif::decode(data)?;
        let (color, alpha) = split(&avif.pixels);
        return Image::from_custom(
            FlatImage {
                color,
                alpha,
                icc: avif.icc,
                width: avif.width,
                height: avif.height,
            },
            interpolate,
        )
        .ok();
    }
    if !matches!(format, ImageFormat::Bmp | ImageFormat::Tiff) {
        return None;
    }
    let (width, height, pixels) = rgba(data, format)?;
    let mut color = Vec::with_capacity(pixels.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(pixels.len() / 4);
    for [r, g, b, a] in pixels.as_chunks::<4>().0 {
        color.extend([*r, *g, *b]);
        alpha.push(*a);
    }
    if alpha.iter().all(|value| *value == 255) {
        alpha.clear();
    }
    Image::from_custom(
        RgbaImage {
            color,
            alpha,
            width,
            height,
        },
        interpolate,
    )
    .ok()
}

/// RGBA pixels as RGB and alpha, the alpha empty where all of them are opaque.
fn split(pixels: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut color = Vec::with_capacity(pixels.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(pixels.len() / 4);
    for [r, g, b, a] in pixels.as_chunks::<4>().0 {
        color.extend([*r, *g, *b]);
        alpha.push(*a);
    }
    if alpha.iter().all(|value| *value == 255) {
        alpha.clear();
    }
    (color, alpha)
}

/// The data of a BMP, TIFF or AVIF image as a PNG of its pixels, for SVG images, which only take
/// PNG, JPEG, GIF and WebP: an AVIF image turned as its `irot` and `imir` say, with its ICC
/// profile. `None` for other data and images that do not decode.
pub(crate) fn as_png(data: &[u8]) -> Option<Vec<u8>> {
    if fagus_layout::avif::is_avif(data) {
        let orientation = fagus_layout::avif::parse(data)?.orientation;
        let avif = crate::avif::decode(data)?;
        let (width, height, pixels) = turned(avif.width, avif.height, &avif.pixels, orientation);
        let mut info = png::Info::with_size(width, height);
        info.color_type = png::ColorType::Rgba;
        info.bit_depth = png::BitDepth::Eight;
        info.icc_profile = avif.icc.map(std::borrow::Cow::Owned);
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::with_info(&mut png, info).ok()?;
            encoder.set_compression(png::Compression::Fastest);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(&pixels).ok()?;
        }
        return Some(png);
    }
    let format = if data.starts_with(b"BM") {
        ImageFormat::Bmp
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        ImageFormat::Tiff
    } else {
        return None;
    };
    let (width, height, pixels) = rgba(data, format)?;
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_compression(png::Compression::Fastest);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&pixels).ok()?;
    }
    Some(png)
}

/// A PNG, GIF or WebP image as PDF/A-1 can take it, where krilla would embed it otherwise: with
/// 8 bits per component (PDF 1.4 has no 16), and without its alpha channel where all its pixels
/// are opaque (PDF/A-1 forbids soft masks). `None` for other images and for gray PNGs with an
/// ICC profile, which does not fit the decoded RGB pixels.
pub(crate) fn flat(data: &[u8], format: ImageFormat) -> Option<Image> {
    let (needed, icc) = match format {
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
            let alpha = matches!(
                info.color_type,
                png::ColorType::Rgba | png::ColorType::GrayscaleAlpha
            ) || info.trns.is_some();
            (
                alpha || info.bit_depth == png::BitDepth::Sixteen,
                info.icc_profile.as_ref().map(|icc| icc.to_vec()),
            )
        }
        // krilla embeds every GIF with an alpha channel.
        ImageFormat::Gif => (true, None),
        ImageFormat::Webp => {
            let mut decoder = image_webp::WebPDecoder::new(Cursor::new(data)).ok()?;
            (decoder.has_alpha(), decoder.icc_profile().ok()?)
        }
        // Embedded as their pixels (see [`decoded`]), already as PDF/A-1 takes them.
        ImageFormat::Jpeg
        | ImageFormat::Bmp
        | ImageFormat::Tiff
        | ImageFormat::Avif
        | ImageFormat::Svg
        | ImageFormat::Pdf(_) => (false, None),
    };
    if !needed {
        return None;
    }
    let (width, height, pixels) = rgba(data, format)?;
    let mut color = Vec::with_capacity(pixels.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(pixels.len() / 4);
    for [r, g, b, a] in pixels.as_chunks::<4>().0 {
        color.extend([*r, *g, *b]);
        alpha.push(*a);
    }
    if alpha.iter().all(|value| *value == 255) {
        alpha.clear();
    }
    Image::from_custom(
        FlatImage {
            color,
            alpha,
            icc,
            width,
            height,
        },
        false,
    )
    .ok()
}

/// The RGB pixels of an image, with its alpha channel (empty where it is opaque) and the ICC
/// profile its colors are in.
#[derive(Clone, Hash)]
struct FlatImage {
    color: Vec<u8>,
    alpha: Vec<u8>,
    icc: Option<Vec<u8>>,
    width: u32,
    height: u32,
}

impl krilla::image::CustomImage for FlatImage {
    fn color_channel(&self) -> &[u8] {
        &self.color
    }

    fn alpha_channel(&self) -> Option<&[u8]> {
        (!self.alpha.is_empty()).then_some(&self.alpha)
    }

    fn bits_per_component(&self) -> krilla::image::BitsPerComponent {
        krilla::image::BitsPerComponent::Eight
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn icc_profile(&self) -> Option<&[u8]> {
        self.icc.as_deref()
    }

    fn color_space(&self) -> krilla::image::ImageColorspace {
        krilla::image::ImageColorspace::Rgb
    }
}

/// A PNG whose `gAMA` chunk Chrome corrects its pixels for (see [`significant_gamma`]), as the
/// sRGB pixels Chrome draws; `None` for other PNGs, which are embedded as they are.
pub(crate) fn gamma_png(data: &[u8], interpolate: bool) -> Option<Image> {
    let decoder = png::Decoder::new(Cursor::new(data));
    significant_gamma(decoder.read_info().ok()?.info())?;
    let (width, height, pixels) = png(data)?;
    let mut color = Vec::with_capacity(pixels.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(pixels.len() / 4);
    for [r, g, b, a] in pixels.as_chunks::<4>().0 {
        color.extend([*r, *g, *b]);
        alpha.push(*a);
    }
    if alpha.iter().all(|value| *value == 255) {
        alpha.clear();
    }
    Image::from_custom(
        RgbaImage {
            color,
            alpha,
            width,
            height,
        },
        interpolate,
    )
    .ok()
}

/// The gamma of a PNG's `gAMA` chunk where Chrome corrects for it: one without an `sRGB`,
/// `iCCP` or `cHRM` chunk whose gamma is not within 5 % of sRGB's 1/2.2 (Chrome 157,
/// measured: 0.44 to 0.47 draw as stored, 0.43 and 0.48 do not).
fn significant_gamma(info: &png::Info<'_>) -> Option<f32> {
    if info.srgb.is_some() || info.icc_profile.is_some() || info.chrm_chunk.is_some() {
        return None;
    }
    let gamma = info.gama_chunk?.into_value();
    (gamma > 0.0 && (gamma * 2.2 - 1.0).abs() >= 0.05).then_some(gamma)
}

/// The sRGB value Chrome draws for each stored value of a PNG of gamma `gamma`: the stored value
/// to the power of 1 / `gamma` is the linear light, which the sRGB curve encodes.
fn gamma_table(gamma: f32) -> [u8; 256] {
    let mut table = [0; 256];
    for (value, out) in table.iter_mut().enumerate() {
        let linear = libm::powf(value as f32 / 255.0, 1.0 / gamma);
        let encoded = if linear <= 0.003_130_8 {
            12.92 * linear
        } else {
            1.055 * libm::powf(linear, 1.0 / 2.4) - 0.055
        };
        *out = (encoded * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    table
}

/// Whether an image of `width` × `height` pixels is small enough to decode: no larger than
/// [`fagus_layout::MAX_PIXELS`], as other images. Images in SVG images are decoded without
/// being probed first, and a header may claim any size: hundreds of gigabytes for a PNG of
/// 200 bytes.
fn decodable(width: impl Into<u64>, height: impl Into<u64>) -> bool {
    width.into().saturating_mul(height.into()) <= fagus_layout::MAX_PIXELS
}

fn png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    if !decodable(reader.info().width, reader.info().height) {
        return None;
    }
    let gamma = significant_gamma(reader.info()).map(gamma_table);
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    buffer.truncate(info.buffer_size());
    if let Some(table) = gamma {
        let channels = info.color_type.samples();
        // The color channels, not alpha.
        let colors = if matches!(
            info.color_type,
            png::ColorType::Rgba | png::ColorType::GrayscaleAlpha
        ) {
            channels - 1
        } else {
            channels
        };
        for pixel in buffer.chunks_exact_mut(channels) {
            for value in &mut pixel[..colors] {
                *value = table[usize::from(*value)];
            }
        }
    }
    let pixels = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[r, g, b]| [*r, *g, *b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[gray, alpha]| [*gray, *gray, *gray, *alpha])
            .collect(),
        png::ColorType::Grayscale => buffer
            .iter()
            .flat_map(|gray| [*gray, *gray, *gray, 255])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    Some((info.width, info.height, pixels))
}

fn jpeg(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(data), options);
    decoder.decode_headers().ok()?;
    let (width, height) = decoder.dimensions()?;
    if !decodable(width as u64, height as u64) {
        return None;
    }
    let pixels = decoder.decode().ok()?;
    (decoder.output_colorspace() == Some(ColorSpace::RGBA)).then_some((width as u32, height as u32, pixels))
}

/// A JPEG of CMYK (or YCCK) pixels without an ICC profile, as Chrome draws it: as RGB, each
/// channel the inverted (Adobe) cyan, magenta or yellow times the inverted black (Blink's
/// `JPEGImageDecoder`), where a viewer would convert a `DeviceCMYK` image as it sees fit (through
/// a print profile, in duller colors). `None` for other JPEGs and those that do not decode.
pub(crate) fn cmyk_jpeg(data: &[u8], interpolate: bool) -> Option<Image> {
    let (width, height, color) = cmyk_rgb(data)?;
    Image::from_custom(
        RgbaImage {
            color,
            alpha: Vec::new(),
            width,
            height,
        },
        interpolate,
    )
    .ok()
}

/// The RGB pixels of a CMYK JPEG without an ICC profile, as Chrome draws them (see
/// [`cmyk_jpeg`]), and their width and height.
pub(crate) fn cmyk_rgb(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let mut decoder = zune_jpeg::JpegDecoder::new(Cursor::new(data));
    decoder.decode_headers().ok()?;
    let (width, height) = decoder.dimensions()?;
    if !decodable(width as u64, height as u64) {
        return None;
    }
    if !matches!(
        decoder.input_colorspace(),
        Some(ColorSpace::CMYK | ColorSpace::YCCK)
    ) || decoder.icc_profile().is_some()
    {
        return None;
    }
    // zune-jpeg multiplies the stored (inverted) channels as Blink does.
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(data), options);
    let color = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;
    if decoder.output_colorspace() != Some(ColorSpace::RGB) || color.len() != width * height * 3 {
        return None;
    }
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?, color))
}

fn gif(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(Cursor::new(data)).ok()?;
    let (width, height) = (usize::from(decoder.width()), usize::from(decoder.height()));
    if !decodable(decoder.width(), decoder.height()) {
        return None;
    }
    let frame = decoder.read_next_frame().ok()??;
    // The first frame, where it lies on the canvas.
    let mut pixels = vec![0_u8; width * height * 4];
    let (left, top, columns) = (
        usize::from(frame.left),
        usize::from(frame.top),
        usize::from(frame.width),
    );
    if columns == 0 {
        return Some((width as u32, height as u32, pixels));
    }
    for (row, line) in frame.buffer.chunks_exact(columns * 4).enumerate() {
        let y = top + row;
        if y >= height || left >= width {
            continue;
        }
        let count = columns.min(width - left) * 4;
        let start = (y * width + left) * 4;
        pixels[start..start + count].copy_from_slice(&line[..count]);
    }
    Some((width as u32, height as u32, pixels))
}

/// The first image of a TIFF file as 8-bit RGBA, as Safari shows it (Chrome shows none): gray
/// (1 to 16 bits, black or white as zero), gray with alpha, RGB and RGBA (8 or 16 bits, alpha
/// associated or not), CMYK (as Chrome converts CMYK JPEGs without a profile), and YCbCr (as
/// the JPEG compression of TIFF stores it). Palette, Lab and float images are not decoded.
fn tiff(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use tiff::ColorType;
    use tiff::decoder::{Decoder, DecodingResult};
    let mut decoder = Decoder::new(Cursor::new(data)).ok()?;
    let (width, height) = decoder.dimensions().ok()?;
    if !decodable(width, height) || width == 0 || height == 0 {
        return None;
    }
    let color = decoder.colortype().ok()?;
    // The meaning of a fourth (or second) sample: alpha premultiplied, alpha, or other data.
    let extra = decoder
        .find_tag_unsigned_vec::<u16>(tiff::tags::Tag::ExtraSamples)
        .ok()
        .flatten()
        .and_then(|extra| extra.first().copied());
    let samples = decoder.read_image().ok()?;
    let (columns, rows) = (width as usize, height as usize);
    // The samples as 8 bits, and how many bits they had.
    let (values, bits): (Vec<u8>, u8) = match samples {
        DecodingResult::U8(values) => (values, 8),
        DecodingResult::U16(values) => (
            values
                .iter()
                .map(|value| ((u32::from(*value) * 255 + 32_767) / 65_535) as u8)
                .collect(),
            16,
        ),
        _ => return None,
    };
    let channels = |depth: u8, count: usize| (depth == bits).then_some(count);
    let (count, kind) = match color {
        ColorType::Gray(depth @ (1 | 2 | 4)) if bits == 8 => {
            return packed_gray(&values, columns, rows, depth).map(|pixels| (width, height, pixels));
        }
        ColorType::Gray(depth) => (channels(depth, 1)?, color),
        ColorType::Multiband {
            bit_depth,
            num_samples: 2,
        } => (channels(bit_depth, 2)?, ColorType::GrayA(bit_depth)),
        ColorType::RGB(depth) | ColorType::YCbCr(depth) => (channels(depth, 3)?, color),
        ColorType::RGBA(depth) | ColorType::CMYK(depth) => (channels(depth, 4)?, color),
        ColorType::CMYKA(depth) => (channels(depth, 5)?, color),
        _ => return None,
    };
    if values.len() < columns * rows * count {
        return None;
    }
    let mut pixels = Vec::with_capacity(columns * rows * 4);
    for sample in values.chunks_exact(count).take(columns * rows) {
        let (mut rgb, alpha) = match kind {
            ColorType::Gray(_) => ([sample[0]; 3], None),
            ColorType::GrayA(_) => ([sample[0]; 3], Some(sample[1])),
            ColorType::RGB(_) => ([sample[0], sample[1], sample[2]], None),
            ColorType::RGBA(_) => ([sample[0], sample[1], sample[2]], Some(sample[3])),
            ColorType::YCbCr(_) => (ycbcr_rgb(sample[0], sample[1], sample[2]), None),
            ColorType::CMYK(_) => (cmyk_rgb_pixel(sample), None),
            _ => (cmyk_rgb_pixel(sample), Some(sample[4])),
        };
        let alpha = match (alpha, extra) {
            // Premultiplied: the colors are divided by it again.
            (Some(alpha), Some(1)) => {
                for value in &mut rgb {
                    *value = if alpha == 0 {
                        0
                    } else {
                        ((u32::from(*value) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
                    };
                }
                alpha
            }
            (Some(alpha), Some(2)) => alpha,
            // An extra sample that is not alpha.
            _ => 255,
        };
        pixels.extend([rgb[0], rgb[1], rgb[2], alpha]);
    }
    Some((width, height, pixels))
}

/// RGBA pixels of `width` by `height` turned as the EXIF `orientation` says: the pixels as they
/// are seen, and their width and height.
fn turned(width: u32, height: u32, pixels: &[u8], orientation: u8) -> (u32, u32, Vec<u8>) {
    if orientation <= 1 || orientation > 8 {
        return (width, height, pixels.to_vec());
    }
    let (columns, rows) = (width as usize, height as usize);
    let (wide, high) = if orientation >= 5 {
        (rows, columns)
    } else {
        (columns, rows)
    };
    let mut out = vec![0; pixels.len()];
    for y in 0..high {
        for x in 0..wide {
            // The stored pixel seen at (x, y).
            let (column, row) = match orientation {
                2 => (columns - 1 - x, y),
                3 => (columns - 1 - x, rows - 1 - y),
                4 => (x, rows - 1 - y),
                5 => (y, x),
                6 => (y, rows - 1 - x),
                7 => (columns - 1 - y, rows - 1 - x),
                _ => (columns - 1 - y, x),
            };
            let from = (row * columns + column) * 4;
            let to = (y * wide + x) * 4;
            out[to..to + 4].copy_from_slice(&pixels[from..from + 4]);
        }
    }
    (wide as u32, high as u32, out)
}

/// Gray pixels of `depth` bits (1, 2 or 4) packed into bytes, each row starting at a byte, as
/// RGBA.
fn packed_gray(values: &[u8], columns: usize, rows: usize, depth: u8) -> Option<Vec<u8>> {
    let depth = usize::from(depth);
    let stride = (columns * depth).div_ceil(8);
    if values.len() < stride * rows {
        return None;
    }
    let most = (1_u16 << depth) - 1;
    let mut pixels = Vec::with_capacity(columns * rows * 4);
    for line in values.chunks_exact(stride).take(rows) {
        for column in 0..columns {
            let at = column * depth;
            let value = u16::from(line[at / 8] >> (8 - depth - at % 8)) & most;
            let gray = ((value * 255 + most / 2) / most) as u8;
            pixels.extend([gray, gray, gray, 255]);
        }
    }
    Some(pixels)
}

/// A YCbCr pixel (full range, as JPEG stores it) as RGB.
fn ycbcr_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (y, cb, cr) = (f32::from(y), f32::from(cb) - 128.0, f32::from(cr) - 128.0);
    [
        y + 1.402 * cr,
        y - 0.344_136 * cb - 0.714_136 * cr,
        y + 1.772 * cb,
    ]
    .map(|value| value.round().clamp(0.0, 255.0) as u8)
}

/// A CMYK pixel without a profile as RGB, as Chrome draws CMYK JPEGs: each channel the inverted
/// cyan, magenta or yellow times the inverted black.
fn cmyk_rgb_pixel(sample: &[u8]) -> [u8; 3] {
    let black = 255 - u32::from(sample[3]);
    [sample[0], sample[1], sample[2]].map(|value| (((255 - u32::from(value)) * black + 127) / 255) as u8)
}

fn webp(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(data)).ok()?;
    let (width, height) = decoder.dimensions();
    if !decodable(width, height) {
        return None;
    }
    let mut buffer = vec![0; decoder.output_buffer_size()?];
    decoder.read_image(&mut buffer).ok()?;
    let pixels = if decoder.has_alpha() {
        buffer
    } else {
        buffer
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[r, g, b]| [*r, *g, *b, 255])
            .collect()
    };
    Some((width, height, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_images_up_to_the_limit_of_other_images() {
        assert!(decodable(20_000_u32, 10_000_u32));
        assert!(!decodable(20_000_u32, 10_001_u32));
        assert!(!decodable(u32::MAX, u32::MAX));
        // A PNG of a few bytes that claims 200 × 788,529,252 pixels.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8], data: &[u8]| {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(data);
            png.extend_from_slice(kind);
            png.extend_from_slice(data);
            png.extend_from_slice(&crc.sum().to_be_bytes());
        };
        chunk(b"IHDR", &[0, 0, 0, 200, 47, 0, 0, 100, 8, 2, 0, 0, 0]);
        chunk(b"IDAT", &[120, 1, 1, 0, 0, 255, 255, 0, 0, 0, 1]);
        chunk(b"IEND", &[]);
        assert!(png::Decoder::new(Cursor::new(&png)).read_info().is_ok());
        assert!(rgba(&png, ImageFormat::Png).is_none());
    }

    #[test]
    fn draws_cmyk_jpegs_without_a_profile_as_chrome() {
        // 8 × 8 pixels of pure cyan (C 255, M, Y and K 0), stored inverted with an Adobe marker
        // as Photoshop and libjpeg store CMYK: Chrome 157 draws them rgb(0 255 255), measured.
        let cmyk: &[u8] = &[
            255, 216, 255, 238, 0, 14, 65, 100, 111, 98, 101, 0, 100, 0, 0, 0, 0, 0, 255, 219, 0, 67, 0, 16,
            11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35, 37, 29,
            40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80, 109, 81, 87, 95,
            98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99, 255, 192, 0, 20, 8, 0, 8,
            0, 8, 4, 67, 17, 0, 77, 17, 0, 89, 17, 0, 75, 17, 0, 255, 196, 0, 20, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 7, 255, 196, 0, 20, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 255, 218, 0, 14, 4, 67, 0, 77, 0, 89, 0, 75, 0, 0, 63, 0, 63, 32, 16, 8, 15, 255, 217,
        ];
        let (width, height, pixels) = cmyk_rgb(cmyk).expect("CMYK");
        assert_eq!((width, height, pixels.len()), (8, 8, 8 * 8 * 3));
        for pixel in pixels.as_chunks::<3>().0 {
            assert!(pixel[0] < 3 && pixel[1] > 252 && pixel[2] > 252, "{pixel:?}");
        }
        assert!(cmyk_jpeg(cmyk, false).is_some());
        // An RGB JPEG is embedded as it is.
        let rgb: &[u8] = &[
            255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67, 0, 16,
            11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35, 37, 29,
            40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80, 109, 81, 87, 95,
            98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99, 255, 219, 0, 67, 1, 17, 18,
            18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
            99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
            99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 255, 192, 0, 17, 8, 0, 8, 0, 8, 3, 1, 34,
            0, 2, 17, 1, 3, 17, 1, 255, 196, 0, 21, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5,
            255, 196, 0, 20, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 196, 0, 21, 1, 1, 1,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 6, 255, 196, 0, 20, 17, 1, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 255, 218, 0, 12, 3, 1, 0, 2, 17, 3, 17, 0, 63, 0, 180, 1, 68, 27, 255,
            217,
        ];
        assert!(cmyk_rgb(rgb).is_none());
    }

    /// A TIFF image of `width` × `height` pixels of color type `C`, with `extra` samples after
    /// the color, compressed with LZW.
    fn tiff_of<C: tiff::encoder::colortype::ColorType>(
        width: u32,
        height: u32,
        extra: &[tiff::tags::ExtraSamples],
        data: &[C::Inner],
    ) -> Vec<u8>
    where
        [C::Inner]: tiff::encoder::TiffValue,
    {
        let mut file = Cursor::new(Vec::new());
        let mut encoder = tiff::encoder::TiffEncoder::new(&mut file)
            .expect("encoder")
            .with_compression(tiff::encoder::Compression::Lzw);
        let mut image = encoder.new_image::<C>(width, height).expect("image");
        image.extra_samples(extra).expect("extra samples");
        image.write_data(data).expect("pixels");
        file.into_inner()
    }

    #[test]
    fn decodes_tiff_images() {
        use tiff::encoder::colortype::{CMYK8, Gray8, Gray16, RGB8, RGB16};
        use tiff::tags::ExtraSamples;
        let pixels = |data: &[u8]| rgba(data, ImageFormat::Tiff).map(|(_, _, pixels)| pixels);
        let rgb = tiff_of::<RGB8>(2, 1, &[], &[255, 0, 0, 0, 0, 255]);
        assert_eq!(pixels(&rgb), Some(vec![255, 0, 0, 255, 0, 0, 255, 255]));
        // 16 bits to the nearest 8.
        let deep = tiff_of::<RGB16>(1, 1, &[], &[65_535, 32_896, 257]);
        assert_eq!(pixels(&deep), Some(vec![255, 128, 1, 255]));
        let gray = tiff_of::<Gray16>(2, 1, &[], &[0, 65_535]);
        assert_eq!(pixels(&gray), Some(vec![0, 0, 0, 255, 255, 255, 255, 255]));
        // Alpha as it is, premultiplied, and a sample that is not alpha.
        let straight = tiff_of::<RGB8>(1, 1, &[ExtraSamples::UnassociatedAlpha], &[0, 255, 0, 128]);
        assert_eq!(pixels(&straight), Some(vec![0, 255, 0, 128]));
        let premultiplied = tiff_of::<RGB8>(1, 1, &[ExtraSamples::AssociatedAlpha], &[0, 128, 0, 128]);
        assert_eq!(pixels(&premultiplied), Some(vec![0, 255, 0, 128]));
        let other = tiff_of::<RGB8>(1, 1, &[ExtraSamples::Unspecified], &[0, 255, 0, 128]);
        assert_eq!(pixels(&other), Some(vec![0, 255, 0, 255]));
        let gray_alpha = tiff_of::<Gray8>(1, 1, &[ExtraSamples::UnassociatedAlpha], &[200, 50]);
        assert_eq!(pixels(&gray_alpha), Some(vec![200, 200, 200, 50]));
        // CMYK without a profile, as Chrome draws CMYK JPEGs: cyan, and black.
        let cmyk = tiff_of::<CMYK8>(2, 1, &[], &[255, 0, 0, 0, 0, 0, 0, 255]);
        assert_eq!(pixels(&cmyk), Some(vec![0, 255, 255, 255, 0, 0, 0, 255]));
        // A 1-bit fax scan (CCITT Group 4, white is zero) of a black label with white text.
        let scan = include_bytes!("../../../tests/corpus/images/media/scan-g4.tif");
        let (width, height, scanned) = rgba(scan, ImageFormat::Tiff).expect("the scan");
        assert_eq!((width, height), (240, 80));
        let at = |x: usize, y: usize| &scanned[(y * 240 + x) * 4..(y * 240 + x) * 4 + 4];
        assert_eq!(at(0, 0), [255, 255, 255, 255]);
        assert_eq!(at(120, 10), [0, 0, 0, 255]);
        // JPEG compression with subsampled YCbCr: the gray band starts black, ends white.
        let receipt = include_bytes!("../../../tests/corpus/images/media/receipt-jpeg.tif");
        let (width, _, band) = rgba(receipt, ImageFormat::Tiff).expect("the receipt");
        assert!(band[..3].iter().all(|value| *value < 8), "{:?}", &band[..4]);
        let last = (width as usize - 1) * 4;
        assert!(
            band[last..last + 3].iter().all(|value| *value > 247),
            "{:?}",
            &band[last..last + 4]
        );
        assert_eq!(ycbcr_rgb(128, 128, 128), [128, 128, 128]);
        assert_eq!(ycbcr_rgb(76, 85, 255), [254, 0, 0]);
        // Not TIFF, and a TIFF cut short.
        assert!(pixels(b"II*\0").is_none());
        assert!(pixels(&rgb[..rgb.len() - 20]).is_none());
    }

    #[test]
    fn packs_bmp_and_tiff_images_as_png_for_svg() {
        use tiff::encoder::colortype::RGB8;
        let tiff = tiff_of::<RGB8>(2, 1, &[], &[255, 0, 0, 0, 0, 255]);
        let png = as_png(&tiff).expect("a PNG");
        assert_eq!(
            super::png(&png),
            Some((2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]))
        );
        let bmp = include_bytes!("../../../tests/corpus/images/media/logo-alpha.bmp");
        let png = as_png(bmp).expect("a PNG");
        assert_eq!(super::png(&png), fagus_layout::bmp::decode(bmp));
        assert!(as_png(&png).is_none());
        assert!(as_png(b"BM").is_none());
        // Embedded as pixels, with an alpha channel where pixels are not opaque.
        assert!(decoded(bmp, ImageFormat::Bmp, false).is_some());
        assert!(decoded(&tiff, ImageFormat::Tiff, false).is_some());
        assert!(decoded(&png, ImageFormat::Png, false).is_none());
    }

    #[test]
    fn corrects_the_gamma_of_pngs_as_chrome() {
        // A PNG of gamma 1.0 stores linear light: Chrome 157 draws 25 as 88, 51 as 124, 85 as
        // 156 and 170 as 213 (measured); gamma 0.5 draws 51 as 56, 0.7 as 89.
        let table = gamma_table(1.0);
        assert_eq!(
            [table[25], table[51], table[85], table[170], table[255]],
            [88, 124, 156, 213, 255]
        );
        assert_eq!(gamma_table(0.5)[51], 56);
        assert_eq!(gamma_table(0.7)[51], 89);
        // A 1 × 1 gray PNG with a `gAMA` chunk of 1.0, then of 1/2.2, which draws as stored.
        let gray = |gamma: u32| {
            let mut data = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut data, 1, 1);
                encoder.set_color(png::ColorType::Grayscale);
                encoder.set_source_gamma(png::ScaledFloat::from_scaled(gamma));
                let mut writer = encoder.write_header().expect("header");
                writer.write_image_data(&[51]).expect("pixels");
            }
            data
        };
        assert_eq!(
            png(&gray(100_000)).map(|(_, _, pixels)| pixels),
            Some(vec![124, 124, 124, 255])
        );
        assert_eq!(
            png(&gray(45_455)).map(|(_, _, pixels)| pixels),
            Some(vec![51, 51, 51, 255])
        );
        assert!(gamma_png(&gray(100_000), false).is_some());
        assert!(gamma_png(&gray(45_455), false).is_none());
    }
}
