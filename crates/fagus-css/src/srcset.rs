//! Responsive images: the candidates of a `srcset` attribute, the size `sizes` gives the image,
//! and the candidate Chrome chooses (HTML, §4.8.4.3; Blink's `HTMLSrcsetParser` and
//! `SizesAttributeParser`).
//!
//! Chrome chooses as it loads the document, on the screen of its headless window, and keeps that
//! choice when it prints: see [`LOAD_SCREEN`].

use lightningcss::values::calc::{Calc, MathFunction};
use lightningcss::values::length::{Length, LengthValue};

use crate::MediaEnvironment;

/// The window Chrome loads a document in when it prints it headless (`--print-to-pdf`): 780 by
/// 380 CSS pixels at one device pixel per CSS pixel (Chrome 157 on Linux, measured with
/// `innerWidth` and `innerHeight`, which include the scroll bars).
/// `sizes` and the `media` of `<picture>` sources are evaluated on this screen.
pub const LOAD_SCREEN: MediaEnvironment = MediaEnvironment {
    width: 780.0,
    height: 380.0,
};

/// The font size `em` and `rem` in `sizes` refer to: the initial one.
const FONT_SIZE: f32 = 16.0;

/// How a candidate of `srcset` describes itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Descriptor {
    /// No descriptor: a density of 1.
    None,
    /// `2x`: image pixels per CSS pixel.
    Density(f32),
    /// `640w`: the width of the image in image pixels.
    Width(f32),
}

/// An image candidate of `srcset`: its URL and descriptor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate<'a> {
    pub url: &'a str,
    pub descriptor: Descriptor,
}

/// The image candidates of a `srcset` attribute, those with invalid descriptors left out (HTML,
/// "parse a srcset attribute").
pub fn parse_srcset(srcset: &str) -> Vec<Candidate<'_>> {
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r');
    let mut candidates = Vec::new();
    let mut rest = srcset;
    loop {
        rest = rest.trim_start_matches(|c: char| is_space(c) || c == ',');
        if rest.is_empty() {
            return candidates;
        }
        let end = rest.find(is_space).unwrap_or(rest.len());
        let (url, after) = rest.split_at(end);
        rest = after;
        let mut descriptors: Vec<&str> = Vec::new();
        let url = if url.ends_with(',') {
            url.trim_end_matches(',')
        } else {
            // The descriptors up to the next comma outside parentheses.
            let mut start = None;
            let mut depth = 0usize;
            let mut stop = rest.len();
            for (at, c) in rest.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    ',' if depth == 0 => {
                        stop = at + 1;
                        if let Some(from) = start.take() {
                            descriptors.push(&rest[from..at]);
                        }
                        break;
                    }
                    c if is_space(c) && depth == 0 => {
                        if let Some(from) = start.take() {
                            descriptors.push(&rest[from..at]);
                        }
                        continue;
                    }
                    _ => {}
                }
                start.get_or_insert(at);
            }
            if stop == rest.len()
                && let Some(from) = start
            {
                descriptors.push(&rest[from..]);
            }
            rest = &rest[stop..];
            url
        };
        if url.is_empty() {
            continue;
        }
        if let Some(descriptor) = descriptor(&descriptors) {
            candidates.push(Candidate { url, descriptor });
        }
    }
}

/// The descriptor of a candidate from its descriptor tokens; `None` where they are invalid.
fn descriptor(tokens: &[&str]) -> Option<Descriptor> {
    let (mut width, mut density, mut height) = (None, None, None);
    for token in tokens {
        // (The last character, which may take more than one byte.)
        let (number, unit) = token.split_at(token.char_indices().next_back()?.0);
        match unit {
            "w" if width.is_none() && density.is_none() => {
                let value: u32 = number
                    .parse()
                    .ok()
                    .filter(|_| number.bytes().all(|b| b.is_ascii_digit()))?;
                if value == 0 {
                    return None;
                }
                width = Some(value as f32);
            }
            "x" if width.is_none() && density.is_none() && height.is_none() => {
                let value: f32 = number
                    .parse()
                    .ok()
                    .filter(|value: &f32| value.is_finite() && *value >= 0.0)?;
                density = Some(value);
            }
            "h" if height.is_none() && density.is_none() => {
                let value: u32 = number
                    .parse()
                    .ok()
                    .filter(|_| number.bytes().all(|b| b.is_ascii_digit()))?;
                if value == 0 {
                    return None;
                }
                height = Some(value);
            }
            _ => return None,
        }
    }
    // A height alone describes nothing.
    if height.is_some() && width.is_none() {
        return None;
    }
    Some(match (width, density) {
        (Some(width), _) => Descriptor::Width(width),
        (None, Some(density)) => Descriptor::Density(density),
        (None, None) => Descriptor::None,
    })
}

/// The width in CSS pixels that `sizes` gives an image in `environment`, on a screen of its size
/// with `screen`, else on its page: that of the first entry whose media condition holds, 100vw
/// without one (HTML, "parse a sizes attribute").
pub fn source_size(sizes: Option<&str>, environment: &MediaEnvironment, screen: bool) -> f32 {
    let entries = sizes.map(split_top_level).unwrap_or_default();
    for entry in entries {
        let entry = entry.trim();
        let (condition, value) = split_last_component(entry);
        let Some(size) = length_px(value, environment) else {
            continue;
        };
        let condition = condition.trim();
        let holds = if screen {
            environment.matches_condition_on_screen(condition)
        } else {
            environment.matches_condition(condition)
        };
        if condition.is_empty() || holds {
            return size;
        }
    }
    environment.width
}

/// `text` split at the commas outside parentheses.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0usize, 0);
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// An entry of `sizes` split into its media condition and its last component value (a length,
/// or a function such as `calc()`).
fn split_last_component(entry: &str) -> (&str, &str) {
    let start = if entry.ends_with(')') {
        // The opening parenthesis that matches the last one, and the function name before it.
        let mut depth = 0usize;
        let mut open = 0;
        for (at, c) in entry.char_indices().rev() {
            match c {
                ')' => depth += 1,
                '(' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        open = at;
                        break;
                    }
                }
                _ => {}
            }
        }
        entry[..open]
            .rfind(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .map_or(0, |at| at + 1)
    } else {
        entry
            .rfind(|c: char| c.is_whitespace() || c == ')')
            .map_or(0, |at| at + 1)
    };
    (&entry[..start], &entry[start..])
}

/// A source size value in CSS pixels: a non-negative length, `calc()` of lengths, or a
/// unitless 0; percentages are not allowed.
fn length_px(value: &str, screen: &MediaEnvironment) -> Option<f32> {
    use lightningcss::stylesheet::ParserOptions;
    use lightningcss::traits::ParseWithOptions;
    if value.trim() == "0" {
        return Some(0.0);
    }
    let value = &*crate::flat_math(value);
    let length = crate::guarded(|| Length::parse_string_with_options(value, ParserOptions::default()).ok())
        .flatten()?;
    let px = match &length {
        Length::Value(value) => value_px(value, screen)?,
        Length::Calc(calc) => calc_px(calc, screen)?,
    };
    (px.is_finite() && px >= 0.0).then_some(px)
}

fn value_px(value: &LengthValue, screen: &MediaEnvironment) -> Option<f32> {
    let (width, height) = (screen.width, screen.height);
    value.to_px().or(match *value {
        LengthValue::Em(em) | LengthValue::Rem(em) => Some(em * FONT_SIZE),
        LengthValue::Ex(ex) | LengthValue::Ch(ex) => Some(ex * FONT_SIZE / 2.0),
        LengthValue::Vw(v) | LengthValue::Svw(v) | LengthValue::Lvw(v) | LengthValue::Dvw(v) => {
            Some(v * width / 100.0)
        }
        LengthValue::Vh(v) | LengthValue::Svh(v) | LengthValue::Lvh(v) | LengthValue::Dvh(v) => {
            Some(v * height / 100.0)
        }
        LengthValue::Vmin(v) | LengthValue::Svmin(v) | LengthValue::Lvmin(v) | LengthValue::Dvmin(v) => {
            Some(v * width.min(height) / 100.0)
        }
        LengthValue::Vmax(v) | LengthValue::Svmax(v) | LengthValue::Lvmax(v) | LengthValue::Dvmax(v) => {
            Some(v * width.max(height) / 100.0)
        }
        _ => None,
    })
}

fn calc_px(calc: &Calc<Length>, screen: &MediaEnvironment) -> Option<f32> {
    match calc {
        Calc::Value(length) => match &**length {
            Length::Value(value) => value_px(value, screen),
            Length::Calc(calc) => calc_px(calc, screen),
        },
        Calc::Sum(a, b) => Some(calc_px(a, screen)? + calc_px(b, screen)?),
        Calc::Product(factor, value) => Some(factor * calc_px(value, screen)?),
        Calc::Function(function) => {
            let all = |values: &[Calc<Length>]| -> Option<Vec<f32>> {
                values.iter().map(|value| calc_px(value, screen)).collect()
            };
            match &**function {
                MathFunction::Calc(inner) => calc_px(inner, screen),
                MathFunction::Min(values) => all(values)?.into_iter().reduce(f32::min),
                MathFunction::Max(values) => all(values)?.into_iter().reduce(f32::max),
                MathFunction::Clamp(min, value, max) => {
                    let (min, value, max) = (
                        calc_px(min, screen)?,
                        calc_px(value, screen)?,
                        calc_px(max, screen)?,
                    );
                    Some(value.min(max).max(min))
                }
                _ => None,
            }
        }
        Calc::Number(_) => None,
    }
}

/// The candidate Chrome shows of `candidates` and an image's `src` (taken as a density of 1,
/// left out beside width descriptors), at `scale` device pixels per CSS pixel for an image
/// `source_size` CSS pixels wide: its URL and density (Blink's `PickBestImageCandidate` and
/// `SelectionLogic`).
pub fn choose<'a>(
    candidates: &[Candidate<'a>],
    src: Option<&'a str>,
    source_size: f32,
    scale: f32,
) -> Option<(&'a str, f32)> {
    let widths = candidates
        .iter()
        .any(|candidate| matches!(candidate.descriptor, Descriptor::Width(_)));
    // Densities, the `src` last (so that a candidate of the same density comes first).
    let mut dense: Vec<(&str, f32)> = candidates
        .iter()
        .map(|candidate| {
            let density = match candidate.descriptor {
                Descriptor::None => 1.0,
                Descriptor::Density(density) => density,
                Descriptor::Width(width) => width / source_size,
            };
            (candidate.url, density)
        })
        .collect();
    if !widths && let Some(src) = src.filter(|src| !src.is_empty()) {
        dense.push((src, 1.0));
    }
    // Sorted by density, the first of equal densities kept.
    dense.sort_by(|a, b| a.1.total_cmp(&b.1));
    dense.dedup_by(|later, earlier| later.1.total_cmp(&earlier.1).is_eq());
    let last = dense.len().checked_sub(1)?;
    let mut chosen = last;
    for at in 0..last {
        let (current, next) = (dense[at].1, dense[at + 1].1);
        if next < scale {
            continue;
        }
        let mean = (current * next).sqrt();
        chosen = if (scale <= 1.0 && scale > current) || scale >= mean {
            at + 1
        } else {
            at
        };
        break;
    }
    dense.get(chosen).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_srcset_candidates_as_html_does() {
        let parsed =
            parse_srcset(" a.png 1x,b.png  2x , c.png 640w, d.png, e.png 3x 4x, f(1).png 1.5x,, g.png 0w");
        assert_eq!(
            parsed,
            vec![
                Candidate {
                    url: "a.png",
                    descriptor: Descriptor::Density(1.0)
                },
                Candidate {
                    url: "b.png",
                    descriptor: Descriptor::Density(2.0)
                },
                Candidate {
                    url: "c.png",
                    descriptor: Descriptor::Width(640.0)
                },
                Candidate {
                    url: "d.png",
                    descriptor: Descriptor::None
                },
                Candidate {
                    url: "f(1).png",
                    descriptor: Descriptor::Density(1.5)
                },
            ]
        );
        // A URL with commas in it ends at the first space; trailing commas end it too.
        assert_eq!(
            parse_srcset("data:image/png;base64,AAA= 2x,x.png,"),
            vec![
                Candidate {
                    url: "data:image/png;base64,AAA=",
                    descriptor: Descriptor::Density(2.0)
                },
                Candidate {
                    url: "x.png",
                    descriptor: Descriptor::None
                },
            ]
        );
        assert_eq!(
            parse_srcset("a.png 100w 50h, b.png 50h"),
            vec![Candidate {
                url: "a.png",
                descriptor: Descriptor::Width(100.0)
            }]
        );
        // A descriptor that ends with a character of several bytes is invalid, like any other
        // unit.
        assert_eq!(
            parse_srcset("a.png 640w\u{f2}, b.png 2\u{d7}, c.png 2x"),
            vec![Candidate {
                url: "c.png",
                descriptor: Descriptor::Density(2.0)
            }]
        );
    }

    #[test]
    fn sizes_images_on_the_screen_chrome_loads_them_on() {
        // Measured in Chrome 157 printing on Linux: 10vw is 78 px, `(min-width: 770px)` holds,
        // the window is landscape and not print.
        let size = |sizes: &str| source_size(Some(sizes), &LOAD_SCREEN, true);
        assert!((size("10vw") - 78.0).abs() < 1e-3);
        assert!((size("calc(10vw + 100px)") - 178.0).abs() < 1e-3);
        assert_eq!(size("min(50vw, 300px)"), 300.0);
        assert_eq!(size("clamp(100px, 10vw, 200px)"), 100.0);
        assert_eq!(size("(min-width: 770px) 50px, 70px"), 50.0);
        assert_eq!(size("(orientation: portrait) 60px, 90px"), 90.0);
        assert_eq!(
            size("(min-width: 790px) 40px, (min-width: 600px) 80px, 120px"),
            80.0
        );
        assert_eq!(size("20em"), 320.0);
        assert_eq!(size("(print) 33px, 66px"), 66.0);
        // Invalid entries are skipped; without a valid one the image is 100vw.
        assert_eq!(size("50%, (min-width: 1px) -4px, 12px"), 12.0);
        assert_eq!(size("nonsense"), 780.0);
        assert_eq!(source_size(None, &LOAD_SCREEN, true), 780.0);
        // On an A4 page, which is portrait and printed at 300 dpi.
        let page = MediaEnvironment::default();
        assert_eq!(
            source_size(Some("(orientation: portrait) 33px, 66px"), &page, false),
            33.0
        );
        assert!((source_size(Some("(min-resolution: 2dppx) 10vw, 66px"), &page, false) - 79.37).abs() < 0.01);
    }

    #[test]
    fn chooses_the_candidate_chrome_chooses() {
        let candidates = parse_srcset("w320.jpg 320w, w640.jpg 640w, w1280.jpg 1280w");
        // 200 px wide: densities 1.6, 3.2 and 6.4; the first at least 1.
        assert_eq!(
            choose(&candidates, Some("src.jpg"), 200.0, 1.0),
            Some(("w320.jpg", 1.6))
        );
        assert_eq!(choose(&candidates, None, 500.0, 1.0), Some(("w640.jpg", 1.28)));
        assert_eq!(
            choose(&candidates, None, 756.0, 1.0).map(|c| c.0),
            Some("w1280.jpg")
        );
        // Wider than all: the densest.
        assert_eq!(
            choose(&candidates, None, 2000.0, 1.0).map(|c| c.0),
            Some("w1280.jpg")
        );
        // Densities with `src` as 1x; a 1x candidate of `srcset` comes before `src`.
        let dense = parse_srcset("b.png 2x, c.png 3x");
        assert_eq!(choose(&dense, Some("a.png"), 756.0, 1.0), Some(("a.png", 1.0)));
        assert_eq!(choose(&dense, None, 756.0, 1.0), Some(("b.png", 2.0)));
        assert_eq!(
            choose(&parse_srcset("one.png 1x"), Some("a.png"), 756.0, 1.0),
            Some(("one.png", 1.0))
        );
        // Below 1: the next density up once 1 lies above the current one.
        assert_eq!(
            choose(&parse_srcset("h.png 0.5x, k.png 1.5x"), None, 756.0, 1.0),
            Some(("k.png", 1.5))
        );
        assert_eq!(choose(&[], Some("a.png"), 756.0, 1.0), Some(("a.png", 1.0)));
        assert_eq!(choose(&[], None, 756.0, 1.0), None);
    }
}
