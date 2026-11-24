//! Cascade and computed values for Fagus.
//!
//! [`compute_styles`] runs the cascade for every element of a document: it orders the matching
//! declarations by origin, importance, layer, specificity and source order, expands shorthands,
//! substitutes `var()`, and computes values ([`ComputedStyle`]).

#![forbid(unsafe_code)]

pub mod anchor;
mod background;
mod block_align;
pub mod calc_size;
mod cascade;
mod color_functions;
mod compute;
pub use compute::SVG_GEOMETRY;
mod controls;
mod current_color;
#[cfg(test)]
mod export_tests;
mod extra;
#[cfg(test)]
mod framework_tests;
mod functions;
mod hints;
mod math;
mod mathml;
mod missing_components;
#[cfg(test)]
mod modern_tests;
#[cfg(test)]
mod negative_tests;
mod page_context;
mod palette;
#[cfg(test)]
mod rollback_tests;
mod shape;
#[cfg(test)]
mod sizing_tests;
mod style;
pub mod svg_hints;
#[cfg(test)]
mod svg_tests;
#[cfg(test)]
mod text_leftover_tests;
#[cfg(test)]
mod transform_tests;
pub mod values;
#[cfg(test)]
mod values_tests;
mod variables;
pub mod webkit_box;
#[cfg(test)]
mod wpt_tests;
mod zoom;

pub use cascade::{
    ContainerSizes, PseudoKey, StringSet, StyledDocument, compute_styles, compute_styles_in,
    compute_styles_with, style_from_declarations,
};
pub use controls::{MeterRegion, meter_value, progress_fraction};
pub use extra::font_face_features;
pub use math::descriptor_number;
pub use style::{ComputedStyle, ComputedValues, CustomProperties};

/// The sRGB color of an absolute color as Chrome draws it (see the computed values of colors),
/// in the light color scheme; `None` for `currentcolor`.
pub fn absolute_color(value: &lightningcss::values::color::CssColor) -> Option<values::Rgba> {
    compute::color(value)
}

/// The built-in stylesheet for HTML documents.
pub const USER_AGENT_CSS: &str = include_str!("ua.css");

/// What the built-in stylesheet adds for documents in quirks mode (no or an old doctype), after
/// [`USER_AGENT_CSS`].
pub const QUIRKS_CSS: &str = include_str!("quirks.css");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::{
        BorderStyle, BreakInside, ColumnFill, ColumnSpan, ContentItem, ContentTarget, DecorationThickness,
        Direction, Display, DisplayInside, LengthPercentage, LengthPercentageAuto, ObjectFit, Rgba,
        UnicodeBidi,
    };
    use fagus_css::{MediaEnvironment, Origin, RuleSet, Stylesheet};
    use fagus_dom::{Document, NodeId};

    fn styles(html: &str, css: &str) -> (Document, StyledDocument) {
        let document = Document::parse_html(html);
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        assert!(ua.diagnostics().is_empty(), "{:#?}", ua.diagnostics());
        let author = Stylesheet::parse(css, Origin::Author, "test.css");
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles(&document, &rules, (environment.width, environment.height));
        (document, styled)
    }

    fn by_id<'a>(document: &Document, styled: &'a StyledDocument, id: &str) -> &'a ComputedStyle {
        let node = document
            .descendants(NodeId::DOCUMENT)
            .find(|&node| {
                document
                    .element(node)
                    .is_some_and(|data| data.id.as_deref() == Some(id))
            })
            .expect("element with id");
        styled.style(node).expect("styled element")
    }

    #[test]
    fn elements_with_the_same_values_share_them() {
        let (document, styled) = styles(
            "<table><tr><td id=a>1<td id=b>2<tr><td id=c>3<td id=d style='color: red'>4</table>",
            "td { padding: 2px }",
        );
        let style = |id| by_id(&document, &styled, id);
        // Siblings and cousins (the cells of other rows) share their values; an inline style does
        // not.
        assert!(style("a").shares_values(style("b")));
        assert!(style("a").shares_values(style("c")));
        assert!(!style("a").shares_values(style("d")));
        // Each element keeps a style of its own, and a change copies the values first.
        assert!(!std::ptr::eq(style("a"), style("b")));
        let mut changed = style("a").clone();
        changed.z_index = Some(1);
        assert!(!changed.shares_values(style("a")));
        assert_eq!(style("a").z_index, None);
        assert!(style("a").shares_values(style("b")));
    }

    #[test]
    fn turns_off_the_native_look_of_controls_with_author_borders_and_radii() {
        let (document, styled) = styles(
            "<input id=plain><input id=radius><button id=button></button><input id=color>",
            "#radius { border-radius: 0 } #button { border-top-left-radius: 2px } #color { color: red }",
        );
        let framed = |id| by_id(&document, &styled, id).author_frame;
        // Chrome 157 draws a control with an author `border-radius` with its CSS border.
        assert!(!framed("plain"));
        assert!(framed("radius"));
        assert!(framed("button"));
        assert!(!framed("color"));
    }

    #[test]
    fn parses_clip_rectangles() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { clip: rect(0, 0, 0, 0) } #b { clip: rect(5px auto 2em auto) }
             #c { clip: rect(1px, 2px, 3px, 4px); clip: auto } #d { clip: rect(1px, 2px) }
             #e { clip: rect(1px, 2px, 3px, 4px); clip: rect(1px, 2%, 3px, 4px) }",
        );
        assert_eq!(by_id(&document, &styled, "a").clip, Some([Some(0.0); 4]));
        assert_eq!(
            by_id(&document, &styled, "b").clip,
            Some([Some(5.0), None, Some(32.0), None])
        );
        assert_eq!(by_id(&document, &styled, "c").clip, None);
        assert_eq!(by_id(&document, &styled, "d").clip, None);
        // Percentages are no values of `clip`: the earlier declaration stays.
        assert_eq!(
            by_id(&document, &styled, "e").clip,
            Some([Some(1.0), Some(2.0), Some(3.0), Some(4.0)])
        );
    }

    #[test]
    fn computes_font_relative_lengths_from_the_metrics_of_the_font() {
        use crate::values::{FontMetrics, FontRequest, FontUnits, Size};
        /// A font "Wide" with a wide "0"; the others take the default metrics.
        struct Wide;
        impl FontMetrics for Wide {
            fn units(&self, request: &FontRequest) -> FontUnits {
                if request.families.first().is_some_and(|family| family == "Wide") {
                    FontUnits {
                        ex: 0.375,
                        ch: 0.625,
                        cap: 0.75,
                        ic: 0.875,
                        ..FontUnits::default()
                    }
                } else {
                    FontUnits::default()
                }
            }
        }
        let document = Document::parse_html("<div id=a><div id=b></div><p id=c></p><p id=d></p></div>");
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "html { font: 20px Wide } #a { font-size: 10px; width: 10ch; height: 10cap }
             #b { font-size: 2ch; width: 10ex; height: 10ic } #c { font-family: serif; width: 10ch }
             #d { width: 1rch; height: 1rex }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles_with(
            &document,
            &rules,
            (environment.width, environment.height),
            &ContainerSizes::default(),
            &Wide,
        );
        let size = |id: &str| {
            let style = by_id(&document, &styled, id);
            (style.width.clone(), style.height.clone())
        };
        let px = |value: f32| Size::Length(LengthPercentage::Px(value));
        assert_eq!(size("a"), (px(62.5), px(75.0)));
        // `font-size` in `ch` refers to the parent's font: 12.5 pixels.
        assert_eq!(size("b"), (px(46.875), px(109.375)));
        // Another font, other metrics.
        assert_eq!(size("c").0, px(50.0));
        // Root-relative units refer to the root's font.
        assert_eq!(size("d"), (px(12.5), px(7.5)));
    }

    #[test]
    fn scales_font_relative_lengths_as_font_size_adjust_scales_the_font() {
        use crate::values::{FontMetrics, FontRequest, FontUnits, Size};
        /// A font whose x-height is a quarter of its em and its "0" half of it.
        struct Small;
        impl FontMetrics for Small {
            fn units(&self, _: &FontRequest) -> FontUnits {
                FontUnits {
                    ex: 0.25,
                    ch: 0.5,
                    ..FontUnits::default()
                }
            }
        }
        let document = Document::parse_html("<div id=a></div><div id=b></div>");
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "html { font: 20px Small } #a { font-size-adjust: 0.5; width: 10ch; height: 2em }
             #b { font-size-adjust: 0.25; width: 10ex }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles_with(
            &document,
            &rules,
            (environment.width, environment.height),
            &ContainerSizes::default(),
            &Small,
        );
        let size = |id: &str| {
            let style = by_id(&document, &styled, id);
            (style.width.clone(), style.height.clone())
        };
        let px = |value: f32| Size::Length(LengthPercentage::Px(value));
        // The font is set twice as large, its "0" with it (WPT
        // `css-fonts/font-size-adjust-order-001`); `em` stays.
        assert_eq!(size("a"), (px(200.0), px(40.0)));
        assert_eq!(size("b").0, px(50.0));
    }

    #[test]
    fn takes_ch_in_upright_vertical_text_for_the_vertical_advance_of_zero() {
        use crate::values::{FontMetrics, FontRequest, FontUnits, Size};
        /// A font whose "0" is half an em wide and that has no vertical metrics: upright glyphs
        /// advance by its ascent and descent, rounded to whole pixels.
        struct Fonts;
        impl FontMetrics for Fonts {
            fn units(&self, _: &FontRequest) -> FontUnits {
                FontUnits {
                    ch: 0.5,
                    ch_upright: None,
                    ascent: 0.93,
                    descent: 0.24,
                    ..FontUnits::default()
                }
            }
        }
        let document = Document::parse_html("<div id=a></div><div id=b></div><div id=c><p id=d></p></div>");
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "div, p { font-size: 16px; height: 9ch }
             #a { writing-mode: vertical-rl; text-orientation: upright }
             #b { writing-mode: vertical-rl } #c { text-orientation: upright }
             #d { writing-mode: vertical-lr }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles_with(
            &document,
            &rules,
            (environment.width, environment.height),
            &ContainerSizes::default(),
            &Fonts,
        );
        let height = |id: &str| by_id(&document, &styled, id).height.clone();
        let px = |value: f32| Size::Length(LengthPercentage::Px(value));
        // 15 and 4 pixels: 19 each.
        assert_eq!(height("a"), px(171.0));
        // Mixed vertical text and horizontal text take the width of the "0".
        assert_eq!(height("b"), px(72.0));
        assert_eq!(height("c"), px(72.0));
        // Its own writing mode and the orientation it inherits.
        assert_eq!(height("d"), px(171.0));
    }

    #[test]
    fn gives_table_rows_and_columns_the_writing_mode_of_their_parent() {
        use crate::values::WritingMode;
        // `writing-mode` does not apply to them; their cells inherit the table's, while the
        // row's own lengths are computed in the writing mode it asks for (ch-units-vrl-001).
        let document = Document::parse_html(
            "<table><colgroup id=g><col id=c></colgroup><tbody id=b><tr id=r><td id=d></td></tr></tbody></table>",
        );
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "tr, tbody, col, colgroup { writing-mode: vertical-rl }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles(&document, &rules, (environment.width, environment.height));
        for id in ["g", "c", "b", "r", "d"] {
            assert_eq!(
                by_id(&document, &styled, id).writing_mode,
                WritingMode::HorizontalTb,
                "{id}"
            );
        }
    }

    #[test]
    fn takes_upright_ch_with_the_larger_ascent_chrome_gives_times() {
        use crate::values::FontUnits;
        // The Times of macOS: an ascent of 3/4 em and a descent of 1/4; Chrome adds 15 % of their
        // 20 pixels to the ascent at 20 pixels, and upright glyphs advance by 23 pixels.
        let times = FontUnits {
            ch_upright: None,
            ascent: 0.75,
            descent: 0.25,
            ascent_hack: true,
            ..FontUnits::default()
        };
        assert!((times.upright_ch(20.0) - 23.0).abs() < 1e-4);
        let other = FontUnits {
            ascent_hack: false,
            ..times
        };
        assert!((other.upright_ch(20.0) - 20.0).abs() < 1e-4);
    }

    #[test]
    fn computes_font_relative_lengths_in_the_language_of_the_element() {
        use crate::values::{FontMetrics, FontRequest, FontUnits, Size};
        /// Fonts whose "0" is wider in Japanese text, as Hiragino's is than Times'.
        struct Japanese;
        impl FontMetrics for Japanese {
            fn units(&self, request: &FontRequest) -> FontUnits {
                FontUnits {
                    ch: if request.lang.as_deref() == Some("ja") {
                        0.625
                    } else {
                        0.5
                    },
                    ..FontUnits::default()
                }
            }
        }
        let document = Document::parse_html(
            "<div id=a lang=ja></div><div id=b></div><div lang=ja><p id=c></p><p id=d lang=en></p></div>",
        );
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "html { font-size: 16px } div, p { width: 10ch }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles_with(
            &document,
            &rules,
            (environment.width, environment.height),
            &ContainerSizes::default(),
            &Japanese,
        );
        let width = |id: &str| by_id(&document, &styled, id).width.clone();
        let px = |value: f32| Size::Length(LengthPercentage::Px(value));
        // Its own `lang`, the inherited one, and one that ends it.
        assert_eq!(width("a"), px(100.0));
        assert_eq!(width("b"), px(80.0));
        assert_eq!(width("c"), px(100.0));
        assert_eq!(width("d"), px(80.0));
    }

    #[test]
    fn line_height_units_refer_to_the_computed_line_height() {
        use crate::values::{FontMetrics, FontRequest, FontUnits, Size};
        /// Fonts whose line spacing is 1.25 em, rounded to whole pixels.
        struct Spacious;
        impl FontMetrics for Spacious {
            fn units(&self, _: &FontRequest) -> FontUnits {
                FontUnits::default()
            }
            fn line_spacing(&self, _: &FontRequest, size: f32) -> Option<f32> {
                Some((size * 1.25).round())
            }
        }
        let document = Document::parse_html(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d><p id=e></p></div>
             <div id=f><p id=g></p></div>",
        );
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            "html { font-size: 20px } #a { height: 2lh; width: 1rlh }
             #b { font-size: 10px; height: 1lh } #c { line-height: 1.5; height: 1lh }
             #d { line-height: 2lh } #e { height: 1lh } #f { font-size: 2lh } #g { height: 1em }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let styled = compute_styles_with(
            &document,
            &rules,
            (environment.width, environment.height),
            &ContainerSizes::default(),
            &Spacious,
        );
        let height = |id: &str| by_id(&document, &styled, id).height.clone();
        let px = |value: f32| Size::Length(LengthPercentage::Px(value));
        // `line-height: normal` is the line spacing of the font: 25 pixels at 20.
        assert_eq!(height("a"), px(50.0));
        assert_eq!(by_id(&document, &styled, "a").width, px(25.0));
        assert_eq!(height("b"), px(13.0));
        assert_eq!(height("c"), px(30.0));
        // `lh` in `line-height` and `font-size` refers to the parent's line height.
        assert_eq!(height("e"), px(50.0));
        assert_eq!(height("g"), px(50.0));
        // Without fonts, 1.2 em.
        let plain = compute_styles(&document, &rules, (environment.width, environment.height));
        assert_eq!(by_id(&document, &plain, "a").height, px(48.0));
    }

    #[test]
    fn computes_shape_outside_and_subgrids() {
        use crate::values::{ClipShape, ShapeBox, ShapeOutside};
        let (document, styled) = styles(
            "<div id=c></div><div id=b></div><div id=i></div><div id=n></div><div id=g></div>",
            "#c { shape-outside: circle(40%) content-box; shape-margin: 5px; }
             #b { shape-outside: border-box; }
             #i { shape-outside: url(a.png); shape-image-threshold: 0.4; }
             #n { shape-outside: circle(); shape-outside: none; }
             #g { display: grid; grid-template-columns: subgrid [a] [b c]; grid-template-rows: subgrid; }",
        );
        let circle = by_id(&document, &styled, "c");
        assert!(matches!(
            circle.shape_outside.as_deref(),
            Some(ShapeOutside::Shape {
                shape: ClipShape::Ellipse { circle: true, .. },
                reference: ShapeBox::ContentBox,
            })
        ));
        assert_eq!(circle.shape_margin, LengthPercentage::Px(5.0));
        assert!(matches!(
            by_id(&document, &styled, "b").shape_outside.as_deref(),
            Some(ShapeOutside::Shape {
                shape: ClipShape::Box,
                reference: ShapeBox::BorderBox,
            })
        ));
        let image = by_id(&document, &styled, "i");
        assert!(matches!(
            image.shape_outside.as_deref(),
            Some(ShapeOutside::Image(_))
        ));
        assert!((image.shape_image_threshold - 0.4).abs() < 1e-6);
        assert_eq!(by_id(&document, &styled, "n").shape_outside, None);
        let grid = by_id(&document, &styled, "g");
        assert!(grid.grid_template_columns.subgrid && grid.grid_template_rows.subgrid);
        let names: Vec<(usize, &str)> = grid
            .grid_template_columns
            .names
            .iter()
            .map(|(line, name)| (*line, &**name))
            .collect();
        assert_eq!(names, [(0, "a"), (1, "b"), (1, "c")]);
    }

    #[test]
    fn computes_subgrids_in_grid_shorthands_and_repeated_names() {
        use crate::values::TrackSize;
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=r></div>",
            "#a { display: grid; grid: subgrid / subgrid; }
             #b { display: grid; grid-template: 50px / subgrid [x]; }
             #r { display: grid; grid-template-columns: subgrid [a] repeat(2, [b]) repeat(auto-fill, [c] []) [d]; }",
        );
        let a = by_id(&document, &styled, "a");
        assert!(a.grid_template_rows.subgrid && a.grid_template_columns.subgrid);
        let b = by_id(&document, &styled, "b");
        assert!(!b.grid_template_rows.subgrid && b.grid_template_columns.subgrid);
        assert_eq!(
            &*b.grid_template_rows.tracks,
            [TrackSize::Length(LengthPercentage::Px(50.0))]
        );
        let r = &by_id(&document, &styled, "r").grid_template_columns;
        let names: Vec<(usize, &str)> = r.names.iter().map(|(line, name)| (*line, &**name)).collect();
        assert_eq!(names, [(0, "a"), (1, "b"), (2, "b")]);
        let repeat = r.auto_repeat.as_ref().expect("an automatic repetition");
        assert_eq!((repeat.at, repeat.tracks.len(), repeat.after_lists), (3, 2, 1));
        assert_eq!(&*repeat.names, [(0, std::sync::Arc::from("c"))]);
    }

    #[test]
    fn computes_decoration_thickness_and_offset() {
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><div id=c><span id=d></span></div>",
            "#a { text-decoration: underline 3px; }
             #b { font-size: 20px; text-decoration: underline dotted; text-decoration-thickness: 10%; }
             #c { font-size: 10px; text-underline-offset: 0.5em; }",
        );
        let a = by_id(&document, &styled, "a");
        assert_eq!(a.text_decoration_thickness, DecorationThickness::Px(3.0));
        assert_eq!(a.decorations[0].thickness, DecorationThickness::Px(3.0));
        let b = by_id(&document, &styled, "b");
        assert_eq!(b.text_decoration_thickness, DecorationThickness::Px(2.0));
        // The offset is inherited, as a length.
        assert_eq!(by_id(&document, &styled, "d").text_underline_offset, Some(5.0));
    }

    #[test]
    fn drops_annotation_positions_chrome_does_not_take() {
        use crate::values::AnnotationSide;
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p><p id=d></p>",
            "p { text-emphasis-position: under; ruby-position: under }
             #a { text-emphasis-position: left; ruby-position: alternate }
             #b { text-emphasis-position: over over; ruby-position: inter-character }
             #c { text-emphasis-position: left over }
             #d { text-emphasis-position: over }",
        );
        let position = |id: &str| {
            let style = by_id(&document, &styled, id);
            (style.text_emphasis_position, style.ruby_position)
        };
        assert_eq!(position("a"), (AnnotationSide::Under, AnnotationSide::Under));
        assert_eq!(position("b"), (AnnotationSide::Under, AnnotationSide::Under));
        assert_eq!(position("c").0, AnnotationSide::Over);
        assert_eq!(position("d").0, AnnotationSide::Over);
    }

    #[test]
    fn computes_emphasis_ruby_and_text_box() {
        use crate::values::{
            AnnotationSide, EmphasisMark, EmphasisShape, RubyAlign, RubyDisplay, TextBoxTrim, TextEdge,
        };
        let (document, styled) = styles(
            "<p id=a><span id=b></span></p><p id=c></p><p id=d></p><p id=e></p>
             <ruby id=f>漢<rt id=g>かん</rt></ruby>",
            "#a { text-emphasis: open sesame red; text-emphasis-position: under right; ruby-position: under;
                  ruby-align: center; text-box: trim-start cap alphabetic }
             #c { text-emphasis-style: 'x'; text-box-trim: trim-end; text-box-edge: ex text }
             #d { text-emphasis: filled; text-box-edge: cap }
             #e { text-box: ex alphabetic }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let sesame = style("a");
        assert_eq!(
            sesame.text_emphasis_style,
            Some(EmphasisMark::Shape {
                shape: EmphasisShape::Sesame,
                filled: false
            })
        );
        assert!(sesame.text_emphasis_color.is_some());
        assert_eq!(sesame.text_emphasis_position, AnnotationSide::Under);
        assert_eq!(
            sesame.text_box_trim,
            TextBoxTrim {
                start: true,
                end: false
            }
        );
        assert_eq!(sesame.text_box_edge, (TextEdge::Cap, TextEdge::Alphabetic));
        // Emphasis, ruby and `text-box-edge` are inherited, `text-box-trim` is not.
        let inner = style("b");
        assert_eq!(inner.text_emphasis_style, sesame.text_emphasis_style);
        assert_eq!(
            (inner.ruby_position, inner.ruby_align),
            (AnnotationSide::Under, RubyAlign::Center)
        );
        assert_eq!(inner.text_box_edge, (TextEdge::Cap, TextEdge::Alphabetic));
        assert_eq!(inner.text_box_trim, TextBoxTrim::default());
        let string = style("c");
        assert_eq!(
            string.text_emphasis_style.as_ref().map(EmphasisMark::text),
            Some("x")
        );
        assert_eq!(
            string.text_box_trim,
            TextBoxTrim {
                start: false,
                end: true
            }
        );
        assert_eq!(string.text_box_edge, (TextEdge::Ex, TextEdge::Text));
        // A fill alone is a circle; one edge must suit both sides, as in Chrome.
        let filled = style("d");
        assert_eq!(
            filled.text_emphasis_style.as_ref().map(EmphasisMark::text),
            Some("\u{25CF}")
        );
        assert_eq!(filled.text_box_edge, (TextEdge::Text, TextEdge::Text));
        // Edges alone trim both sides.
        let edges = style("e");
        assert_eq!(
            edges.text_box_trim,
            TextBoxTrim {
                start: true,
                end: true
            }
        );
        assert_eq!(edges.text_box_edge, (TextEdge::Ex, TextEdge::Alphabetic));
        assert_eq!(style("f").ruby_display, RubyDisplay::Container);
        let annotation = style("g");
        assert_eq!(annotation.ruby_display, RubyDisplay::Text);
        assert_eq!(annotation.font_size, 8.0);
        assert_eq!(annotation.text_emphasis_style, None);
    }

    #[test]
    fn computes_direction_and_logical_sides() {
        let (document, styled) = styles(
            "<ul id=a dir=rtl><li id=b>x</li></ul><p id=c dir=auto>שלום</p><bdo id=d dir=rtl></bdo>
             <p id=e style='direction: rtl; margin-inline-start: 3px; direction: ltr'>a</p>
             <div id=f dir=rtl><span id=g style='margin-left: 1px; margin-inline-start: 2px'></span></div>",
            "",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let a = style("a");
        assert_eq!(
            (a.direction, a.unicode_bidi),
            (Direction::Rtl, UnicodeBidi::Isolate)
        );
        // `padding-inline-start` of the user agent's list is on the right.
        assert_eq!(a.padding.right.resolve(0.0), 40.0);
        assert_eq!(a.padding.left.resolve(0.0), 0.0);
        assert_eq!(style("b").direction, Direction::Rtl);
        assert_eq!(style("c").direction, Direction::Rtl);
        assert_eq!(style("d").unicode_bidi, UnicodeBidi::IsolateOverride);
        // The direction the element ends up with maps its logical properties.
        let e = style("e");
        assert_eq!(e.direction, Direction::Ltr);
        assert_eq!(e.margin.left.resolve_or(0.0, -1.0), 3.0);
        // The later of a logical and a physical declaration of a side wins.
        let g = style("g");
        assert_eq!(
            (g.direction, g.unicode_bidi),
            (Direction::Rtl, UnicodeBidi::Normal)
        );
        assert_eq!(g.margin.left.resolve_or(0.0, -1.0), 1.0);
        assert_eq!(g.margin.right.resolve_or(0.0, -1.0), 2.0);
    }

    #[test]
    fn computes_text_spacing() {
        use values::{TextSpacingTrim, WritingMode};
        let (document, styled) = styles(
            "<div id=a><p id=b>x</p></div><div id=c></div><div id=d></div><div id=e></div>
             <div id=f></div>",
            "#a { font: 16px serif; text-spacing-trim: trim-start; text-autospace: normal }
             #c { text-spacing-trim: space-first; text-spacing-trim: trim-both; text-autospace: auto }
             #d { writing-mode: vertical-lr; -webkit-writing-mode: vertical-rl }
             #e { -webkit-writing-mode: vertical-rl; writing-mode: vertical-lr }
             #f { writing-mode: vertical-lr; -webkit-writing-mode: sideways-rl }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        assert_eq!(style("a").text_spacing_trim, TextSpacingTrim::TrimStart);
        // Inherited.
        assert_eq!(style("b").text_spacing_trim, TextSpacingTrim::TrimStart);
        assert!(style("b").text_autospace);
        // Values Chrome does not take leave the earlier declaration.
        assert_eq!(style("c").text_spacing_trim, TextSpacingTrim::SpaceFirst);
        assert!(!style("c").text_autospace);
        // `-webkit-writing-mode` is an alias, with the three CSS values alone.
        assert_eq!(style("d").writing_mode, WritingMode::VerticalRl);
        assert_eq!(style("e").writing_mode, WritingMode::VerticalLr);
        assert_eq!(style("f").writing_mode, WritingMode::VerticalLr);
    }

    #[test]
    fn resizes_elements_but_not_their_generated_content() {
        // `resize` on `::before` and `::after` gives no resize handle (CSS UI 4 leaves it
        // undefined; web-platform-tests expect none).
        let (document, styled) = styles(
            "<div id=a></div>",
            "#a, #a::before, #a::after { content: ''; overflow: hidden; resize: both }",
        );
        assert!(by_id(&document, &styled, "a").resize);
        let node = node(&document, "a");
        for key in [PseudoKey::Before, PseudoKey::After] {
            assert!(
                !styled.pseudo(node, key).expect("pseudo-element").resize,
                "{key:?}"
            );
        }
    }

    #[test]
    fn computes_the_scroll_bar_properties() {
        use values::{Rgba, ScrollbarWidth};
        let (document, styled) = styles(
            "<div id=a><p id=b>x</p></div><div id=c></div><div id=d></div>",
            "#a { scrollbar-width: thin; scrollbar-color: red rgb(0 0 255); scrollbar-gutter: stable }
             #c { scrollbar-width: none; scrollbar-width: wide; scrollbar-color: red;
                  scrollbar-gutter: both-edges stable; scrollbar-gutter: both-edges }
             #d { scrollbar-color: currentColor auto; scrollbar-color: auto; scrollbar-gutter: stable both-edges;
                  scrollbar-gutter: auto }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        assert_eq!(style("a").scrollbar_width, ScrollbarWidth::Thin);
        let red = Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        let blue = Rgba {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        };
        assert_eq!(style("a").scrollbar_color, Some((Some(red), Some(blue))));
        // The width is not inherited, the colors are.
        assert_eq!(style("b").scrollbar_width, ScrollbarWidth::Auto);
        assert_eq!(style("b").scrollbar_color, Some((Some(red), Some(blue))));
        // Values that are not valid leave the earlier declaration.
        assert_eq!(style("c").scrollbar_width, ScrollbarWidth::None);
        assert_eq!(style("c").scrollbar_color, None);
        assert_eq!(style("d").scrollbar_color, None);
        // `scrollbar-gutter`, not inherited either.
        let stable = |both_edges| values::ScrollbarGutter {
            stable: true,
            both_edges,
        };
        assert_eq!(style("a").scrollbar_gutter, stable(false));
        assert_eq!(style("b").scrollbar_gutter, values::ScrollbarGutter::default());
        assert_eq!(style("c").scrollbar_gutter, stable(true));
        assert_eq!(style("d").scrollbar_gutter, values::ScrollbarGutter::default());
    }

    #[test]
    fn computes_hanging_punctuation() {
        use values::{HangingEnd, HangingPunctuation};
        let (document, styled) = styles(
            "<div id=a><p id=b>x</p><p id=c>y</p></div><div id=d></div><div id=e></div>",
            "#a { hanging-punctuation: last first }
             #c { hanging-punctuation: allow-end; hanging-punctuation: force-end force-end }
             #d { hanging-punctuation: force-end }
             #e { hanging-punctuation: first; hanging-punctuation: initial }",
        );
        let style = |id: &str| by_id(&document, &styled, id).hanging_punctuation;
        let first_last = HangingPunctuation {
            first: true,
            last: true,
            ..HangingPunctuation::default()
        };
        assert_eq!(style("a"), first_last);
        // Inherited.
        assert_eq!(style("b"), first_last);
        // A value that is not valid leaves the earlier declaration.
        assert_eq!(
            style("c"),
            HangingPunctuation {
                end: HangingEnd::Allow,
                ..HangingPunctuation::default()
            }
        );
        assert_eq!(style("d").end, HangingEnd::Force);
        assert!(style("e").is_none());
    }

    #[test]
    fn computes_text_fit_as_chrome_parses_it() {
        use values::{TextFit, TextFitKind, TextFitTarget};
        let (document, styled) = styles(
            "<div id=a><p id=b>x</p></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { text-fit: shrink per-line-all 75% }
             #c { text-fit: grow; text-fit: grow 50% consistent }
             #d { text-fit: grow per-line }
             #e { text-fit: shrink -5% }",
        );
        let style = |id: &str| by_id(&document, &styled, id).text_fit;
        let fit = |kind, target, limit| TextFit { kind, target, limit };
        assert_eq!(
            style("a"),
            fit(TextFitKind::Shrink, TextFitTarget::PerLineAll, Some(0.75))
        );
        // Inherited.
        assert_eq!(style("b"), style("a"));
        // The percentage comes last; a value Chrome does not take leaves the earlier one.
        assert_eq!(
            style("c"),
            fit(TextFitKind::Grow, TextFitTarget::Consistent, None)
        );
        assert_eq!(style("d"), fit(TextFitKind::Grow, TextFitTarget::PerLine, None));
        assert_eq!(style("e"), TextFit::default());
    }

    #[test]
    fn computes_writing_modes_and_their_logical_sides() {
        use values::{Size, TextOrientation, WritingMode};
        let (document, styled) = styles(
            "<div id=a><p id=b>x<span id=c>y</span></p></div>
             <div id=d></div><div id=e dir=rtl></div><div id=f></div>
             <p id=g><span id=h>z</span></p>",
            "#a { writing-mode: vertical-rl; text-orientation: upright }
             #b { margin-block: 3px 5px; padding-inline-start: 7px; inline-size: 40px; min-block-size: 9px }
             #c { text-combine-upright: all }
             #d { writing-mode: vertical-lr; border-block-start: 2px solid; border-start-end-radius: 4px }
             #e { writing-mode: sideways-lr; margin-inline-start: 6px }
             #f { writing-mode: tb-rl }
             #h { writing-mode: vertical-rl }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let vertical = style("a");
        assert_eq!(
            (vertical.writing_mode, vertical.text_orientation),
            (WritingMode::VerticalRl, TextOrientation::Upright)
        );
        // Inherited, with the logical properties of the user agent's paragraph on the right and
        // left, the block-start side being the right.
        let paragraph = style("b");
        assert_eq!(
            (paragraph.writing_mode, paragraph.text_orientation),
            (WritingMode::VerticalRl, TextOrientation::Upright)
        );
        assert_eq!(paragraph.margin.right.resolve_or(0.0, -1.0), 3.0);
        assert_eq!(paragraph.margin.left.resolve_or(0.0, -1.0), 5.0);
        assert_eq!(paragraph.margin.top.resolve_or(0.0, -1.0), 0.0);
        assert_eq!(paragraph.padding.top.resolve(0.0), 7.0);
        assert_eq!(paragraph.height, Size::Length(LengthPercentage::Px(40.0)));
        assert_eq!(paragraph.min_width, Size::Length(LengthPercentage::Px(9.0)));
        let combined = style("c");
        assert!(combined.text_combine_upright && !paragraph.text_combine_upright);
        assert_eq!(combined.display, Display::INLINE);
        let lr = style("d");
        assert_eq!(lr.border_width.left, 2.0);
        assert_eq!(lr.border_radius.bottom_left.0, LengthPercentage::Px(4.0));
        // `sideways-lr` lines run upward: right to left text starts at the top.
        let sideways = style("e");
        assert_eq!(sideways.writing_mode, WritingMode::SidewaysLr);
        assert_eq!(sideways.margin.top.resolve_or(0.0, -1.0), 6.0);
        assert_eq!(style("f").writing_mode, WritingMode::VerticalRl);
        // An inline box in another writing mode becomes an inline block.
        assert!(matches!(
            style("h").display,
            Display::Normal {
                outside: values::DisplayOutside::Inline,
                inside: DisplayInside::FlowRoot,
                ..
            }
        ));
        assert_eq!(style("g").writing_mode, WritingMode::HorizontalTb);
    }

    #[test]
    fn computes_columns() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div>",
            "#a { columns: 12em 3; column-rule: thin dashed red; column-fill: auto }
             #b { columns: 2; column-rule-width: 4px; column-span: all }
             #c { column-width: 0; column-count: auto }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let a = style("a");
        assert_eq!((a.column_count, a.column_width), (Some(3), Some(192.0)));
        assert_eq!(
            (a.column_rule_width, a.column_rule_style),
            (1.0, BorderStyle::Dashed)
        );
        assert!(a.column_rule_color.is_some());
        assert_eq!(a.column_fill, ColumnFill::Auto);
        let b = style("b");
        assert_eq!((b.column_count, b.column_width), (Some(2), None));
        assert_eq!((b.column_rule_width, b.column_span), (4.0, ColumnSpan::All));
        let c = style("c");
        assert_eq!((c.column_count, c.column_width), (None, Some(1.0)));
        assert!(c.is_multicol());
    }

    #[test]
    fn drops_invalid_column_declarations() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { column-count: 4; column-count: -1 }
             #b { column-count: 4; column-count: 2.0; column-count: 0 }
             #c { column-count: 4; columns: 8 auto 6em }
             #d { column-width: 5em; column-width: -2em; column-width: 10%; column-width: 3 }
             #e { columns: 4; columns: 2 3 }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        assert_eq!(style("a").column_count, Some(4));
        assert_eq!(style("b").column_count, Some(4));
        assert_eq!(
            (style("c").column_count, style("c").column_width),
            (Some(4), None)
        );
        assert_eq!(style("d").column_width, Some(80.0));
        assert_eq!(style("e").column_count, Some(4));
    }

    #[test]
    fn drops_column_rules_that_repeat_a_part() {
        let (document, styled) = styles(
            "<div id=a></div>",
            "#a { column-rule: 4px solid green; column-rule: red 5px red solid;
                  column-rule: normal 5px red; column-rule: 5px solid dotted }",
        );
        let a = by_id(&document, &styled, "a");
        assert_eq!(
            (a.column_rule_width, a.column_rule_style),
            (4.0, BorderStyle::Solid)
        );
        let green = a.column_rule_color.expect("a color");
        assert!(green.r < 0.01 && green.g > 0.4 && green.b < 0.01, "{green:?}");
    }

    #[test]
    fn computes_column_height_and_wrap() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div>",
            "#a { columns: 2; column-height: 3em; column-wrap: wrap }
             #b { column-height: 40px; column-wrap: nowrap }
             #c { column-height: 40px; column-height: auto; column-wrap: bogus }
             #d { column-wrap: nowrap; columns: 100px / 50px }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        assert_eq!(
            (style("a").column_height, style("a").column_wrap),
            (Some(48.0), Some(true))
        );
        assert_eq!(
            (style("b").column_height, style("b").column_wrap),
            (Some(40.0), Some(false))
        );
        assert_eq!((style("c").column_height, style("c").column_wrap), (None, None));
        // `column-height` alone makes a multi-column container, as in Blink.
        assert!(style("b").is_multicol());
        assert!(!style("c").is_multicol());
        let d = style("d");
        assert_eq!(
            (d.column_width, d.column_height, d.column_wrap),
            (Some(100.0), Some(50.0), None)
        );
    }

    #[test]
    fn computes_object_fit_and_position() {
        let (document, styled) = styles(
            "<img id=a><img id=b><img id=c><img id=d><img id=e>",
            "#a { object-fit: cover; object-position: left top }
             #b { object-fit: scale-down; object-position: 10px 100% }
             #c { object-position: bottom }
             #d { object-position: right 10px bottom 20% }
             #e { object-fit: squeeze; object-position: 1 2 3 }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let resolved = |id: &str| {
            let (x, y) = &style(id).object_position;
            (x.resolve(100.0), y.resolve(100.0))
        };
        assert_eq!(style("a").object_fit, ObjectFit::Cover);
        assert_eq!(resolved("a"), (0.0, 0.0));
        assert_eq!(style("b").object_fit, ObjectFit::ScaleDown);
        assert_eq!(resolved("b"), (10.0, 100.0));
        assert_eq!(resolved("c"), (50.0, 100.0));
        assert_eq!(resolved("d"), (90.0, 80.0));
        // Invalid values leave the initial ones.
        assert_eq!(style("e").object_fit, ObjectFit::Fill);
        assert_eq!(resolved("e"), (50.0, 50.0));
    }

    #[test]
    fn computes_math_functions_in_object_positions() {
        // As Chrome reads them: a bare comparison of percentages reduced, one in a sum kept for
        // the size, which is negative where the image is larger than the box.
        let css = fagus_css::author_css(
            "#a { object-position: calc(50% - 10px) max(10px, 20%) }
             #b { object-position: min(0%, 100%) calc(min(0%, 100%) + 0px) }",
            false,
        )
        .unwrap_or_default();
        let (document, styled) = styles("<img id=a><img id=b>", &css);
        let resolved = |id: &str, basis: f32| {
            let (x, y) = &by_id(&document, &styled, id).object_position;
            (x.resolve(basis), y.resolve(basis))
        };
        assert_eq!(resolved("a", 100.0), (40.0, 20.0));
        assert_eq!(resolved("b", -100.0), (0.0, -100.0));
    }

    #[test]
    fn rounds_line_heights_like_browsers() {
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e></p>",
            "#a { font: 8pt/1.4 x } #b { font: 8.5pt/1.45 x } #c { font: 7pt/1.5 x }
             #d { font: 10pt/1.45 x } #e { font: 8pt/14.93px x }",
        );
        // As Chrome lays them out (measured): 8pt is 10.67px, rounded to 683/64 first.
        let height = |id: &str| by_id(&document, &styled, id).line_height_px(0.0) * 64.0;
        assert_eq!(height("a"), 956.0);
        assert_eq!(height("b"), 1051.0);
        assert_eq!(height("c"), 895.0);
        assert_eq!(height("d"), 1236.0);
        assert_eq!(height("e"), 956.0);
    }

    #[test]
    fn snaps_border_widths_like_browsers() {
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e></p>",
            "p { border: solid } #a { border-width: 0.1px 0.5pt 0.99px 0 }
             #b { border-width: 1.5px 1.99px 2.5px 3.7px } #c { border-width: 0.75pt 3pt 0.3mm 1em; font-size: 10.5px }
             #d { border: 0.5px none; column-rule: 1.5px solid } #e { border-width: thin medium thick 2px }",
        );
        // As Chrome computes them (measured): less than a pixel is one, more is cut to whole
        // pixels; widths of borders without a style are zero.
        let widths = |id: &str| {
            let style = by_id(&document, &styled, id);
            let sides = style.border_width;
            [
                sides.top,
                sides.right,
                sides.bottom,
                sides.left,
                style.column_rule_width,
            ]
        };
        assert_eq!(widths("a"), [1.0, 1.0, 1.0, 0.0, 3.0]);
        assert_eq!(widths("b"), [1.0, 1.0, 2.0, 3.0, 3.0]);
        assert_eq!(widths("c"), [1.0, 4.0, 1.0, 10.0, 3.0]);
        assert_eq!(widths("d"), [0.0, 0.0, 0.0, 0.0, 1.0]);
        assert_eq!(widths("e"), [1.0, 3.0, 5.0, 2.0, 3.0]);
    }

    #[test]
    fn snaps_outline_offsets_like_border_widths() {
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p><p id=d></p>",
            "#a { outline-offset: 0.3px } #b { outline-offset: 1.7px } #c { outline-offset: -0.3px }
             #d { outline-offset: -2.5px }",
        );
        let offset = |id: &str| by_id(&document, &styled, id).outline_offset;
        // As Chrome computes them (measured): whole pixels away from zero, at least one.
        assert_eq!(["a", "b", "c", "d"].map(offset), [1.0, 1.0, -1.0, -2.0]);
    }

    #[test]
    fn sizes_inline_svg_by_its_attributes() {
        use crate::values::Size;
        let (document, styled) = styles(
            "<svg id=a width=24 height=2em></svg><svg id=b width=50% height='1px;color:red'></svg>\
             <svg id=c class=icon width=24 height=24></svg>",
            "body { font-size: 10px } .icon { width: 20px }",
        );
        let size = |id| {
            let style = by_id(&document, &styled, id);
            (style.width.clone(), style.height.clone())
        };
        let px = |value| Size::Length(LengthPercentage::Px(value));
        assert_eq!(size("a"), (px(24.0), px(20.0)));
        // A value that is no length is ignored, whatever it holds.
        assert_eq!(
            size("b"),
            (Size::Length(LengthPercentage::Percent(0.5)), Size::Auto)
        );
        assert_eq!(
            by_id(&document, &styled, "b").color,
            by_id(&document, &styled, "a").color
        );
        // Style sheets win over the attributes.
        assert_eq!(size("c"), (px(20.0), px(24.0)));
    }

    #[test]
    fn sizes_monospace_text_from_the_fixed_font_size() {
        let (document, styled) = styles(
            "<code id=a>x</code><span id=b style='font-family: monospace, monospace'>x</span>
             <div style='font-size: 20px'><code id=c>x</code></div>
             <div style='font-size: large'><span style='font-size: 2em'><code id=d>x</code></span></div>
             <code id=e style='font-size: 2em'>x</code><code id=f style='font-size: larger'>x</code>
             <div style='font-family: monospace'><span id=g style='font-family: serif'>x</span></div>
             <font id=h face=monospace>x</font><textarea id=i></textarea>",
            "",
        );
        // As Chrome computes them (measured).
        let size = |id| by_id(&document, &styled, id).font_size;
        assert_eq!(size("a"), 13.0);
        assert_eq!(size("b"), 16.0);
        assert_eq!(size("c"), 20.0);
        assert_eq!(size("d"), 29.25);
        assert_eq!(size("e"), 26.0);
        assert!((size("f") - 15.6).abs() < 1e-4);
        assert_eq!(size("g"), 16.0);
        assert_eq!(size("h"), 13.0);
        assert!((size("i") - 13.333_333).abs() < 1e-4);
    }

    #[test]
    fn maps_align_attributes_to_webkit_alignments() {
        use crate::values::TextAlign;
        let (document, styled) = styles(
            "<!DOCTYPE html><div id=d align=center><p id=p align=center>x</p>
               <table id=t><tr><td id=c align=right>x</td></tr></table></div>
             <center id=m><div id=n style='text-align: left'>x</div></center>
             <div id=w style='text-align: -webkit-right'>x</div>",
            "",
        );
        let align = |id| {
            let style = by_id(&document, &styled, id);
            (style.text_align, style.align_blocks)
        };
        // `div`, table cells and `<center>` also align blocks; `p` does not.
        assert_eq!(align("d"), (TextAlign::Center, true));
        assert_eq!(align("p"), (TextAlign::Center, false));
        assert_eq!(align("c"), (TextAlign::Right, true));
        assert_eq!(align("m"), (TextAlign::Center, true));
        assert_eq!(align("w"), (TextAlign::Right, true));
        // A plain `text-align` ends it; a table goes back to `start` (as Blink's style adjuster).
        assert_eq!(align("n"), (TextAlign::Left, false));
        assert_eq!(align("t"), (TextAlign::Start, false));
    }

    #[test]
    fn reads_frame_and_rules_of_tables() {
        use crate::values::BorderCollapse;
        let (document, styled) = styles(
            "<!DOCTYPE html><table id=a border=3 frame=hsides rules=groups><tbody id=g><tr><td id=c>x</td></tr></tbody></table>
             <table id=b frame=box border=4><tr><td>x</td></tr></table>
             <table id=v frame=nonsense border=2><tr><td>x</td></tr></table>",
            "",
        );
        let table = by_id(&document, &styled, "a");
        // `frame` draws its sides solid and 1px wide (it comes after `border`), and hides the
        // others; `rules` collapses the borders.
        assert_eq!(table.border_width.top, 1.0);
        assert_eq!(
            [
                table.border_style.top,
                table.border_style.right,
                table.border_style.bottom
            ],
            [BorderStyle::Solid, BorderStyle::Hidden, BorderStyle::Solid]
        );
        assert_eq!(table.border_collapse, BorderCollapse::Collapse);
        // `rules=groups` rules off the row groups, not the cells.
        let group = by_id(&document, &styled, "g");
        assert_eq!(
            (group.border_width.top, group.border_style.top),
            (1.0, BorderStyle::Solid)
        );
        assert_eq!(by_id(&document, &styled, "c").border_style.top, BorderStyle::None);
        // The later of `border` and `frame` sets the width; an unknown `frame` counts for nothing.
        assert_eq!(by_id(&document, &styled, "b").border_width.top, 4.0);
        let unknown = by_id(&document, &styled, "v");
        assert_eq!(
            (unknown.border_width.top, unknown.border_style.top),
            (2.0, BorderStyle::Outset)
        );
    }

    #[test]
    fn reads_image_alignment_as_chrome() {
        use crate::values::VerticalAlign;
        let (document, styled) = styles(
            "<!DOCTYPE html><img id=m align=middle><img id=a align=absmiddle><img id=b align=bottom>",
            "",
        );
        let align = |id| by_id(&document, &styled, id).vertical_align.clone();
        assert_eq!(align("m"), VerticalAlign::BaselineMiddle);
        assert_eq!(align("a"), VerticalAlign::Middle);
        assert_eq!(align("b"), VerticalAlign::Baseline);
    }

    #[test]
    fn font_family_keywords_are_keywords() {
        // lightningcss reads `font-family: inherit` as a generic family; it inherits.
        let (document, styled) = styles(
            "<div style='font-family: Inter, sans-serif'><p id=p style='font-family: inherit'>x</p>
               <p id=i style='font-family: initial'>x</p></div>",
            "",
        );
        assert_eq!(
            &*by_id(&document, &styled, "p").font_family,
            ["Inter", "sans-serif"]
        );
        assert_ne!(
            by_id(&document, &styled, "i")
                .font_family
                .first()
                .map(String::as_str),
            Some("Inter")
        );
    }

    #[test]
    fn quoted_generic_names_name_families() {
        // A generic family's keyword in quotes is a family name (WPT
        // `css-fonts/quoted-generic-ignored`); it keeps its quotes, which no family has.
        let (document, styled) = styles("<p id=p style='font-family: \"fantasy\", serif'>x</p>", "");
        assert_eq!(
            &*by_id(&document, &styled, "p").font_family,
            ["\"fantasy\"", "serif"]
        );
    }

    #[test]
    fn applies_the_rules_of_quirks_mode() {
        let quirks_styles = |html: &str| {
            let document = Document::parse_html(html);
            let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
            let quirks = Stylesheet::parse(QUIRKS_CSS, Origin::UserAgent, "quirks.css");
            assert!(quirks.diagnostics().is_empty(), "{:#?}", quirks.diagnostics());
            let author = Stylesheet::parse(
                "body { font-size: 12px; font-weight: bold }",
                Origin::Author,
                "test.css",
            );
            let environment = MediaEnvironment::default();
            let rules = RuleSet::new(&[&ua, &quirks, &author], &environment);
            let styled = compute_styles(&document, &rules, (environment.width, environment.height));
            (document, styled)
        };
        let (document, styled) = quirks_styles(
            "<body><table id=t><tr><td>x</td></tr></table><p id=p>x</p><p id=a style='margin-top: 0'>x</p>
             <h1 id=h>x</h1><div id=d>x</div>",
        );
        // Tables do not inherit the font size and weight.
        let table = by_id(&document, &styled, "t");
        assert_eq!((table.font_size, table.font_weight), (16.0, 400.0));
        // The default margins of `p` are quirky at both ends, those of headings at the top;
        // an author's margin is not.
        assert_eq!(by_id(&document, &styled, "p").quirky_margins, (true, true));
        assert_eq!(by_id(&document, &styled, "a").quirky_margins, (false, true));
        assert_eq!(by_id(&document, &styled, "h").quirky_margins, (true, false));
        assert_eq!(by_id(&document, &styled, "d").quirky_margins, (false, false));
        // In standards mode nothing is quirky.
        let (document, styled) = styles("<!DOCTYPE html><p id=p>x</p>", "");
        assert_eq!(by_id(&document, &styled, "p").quirky_margins, (false, false));
    }

    #[test]
    fn reads_style_attributes_with_the_quirks_of_their_mode() {
        use crate::values::Size;
        let html = "<div id=d style='color: ff0000; width: 100; gap: 10'>x</div>";
        // In quirks mode a color may leave out its `#` and a width its unit, not a gap.
        let (document, styled) = styles(html, "");
        let quirky = by_id(&document, &styled, "d");
        assert_eq!(quirky.color, Rgba::from_u8(255, 0, 0, 1.0));
        assert_eq!(quirky.width, Size::Length(LengthPercentage::Px(100.0)));
        assert_eq!(quirky.row_gap, None);
        // In standards mode neither does.
        let (document, styled) = styles(&format!("<!DOCTYPE html>{html}"), "");
        let standard = by_id(&document, &styled, "d");
        assert_eq!(standard.color, Rgba::from_u8(0, 0, 0, 1.0));
        assert_eq!(standard.width, Size::Auto);
    }

    #[test]
    fn applies_presentational_hints() {
        use crate::values::{Float, ListStyleType, Size, TextAlign, VerticalAlign, WhiteSpace};
        let (document, styled) = styles(
            "<table id=t width=50% cellspacing=4 cellpadding=6 border=2 bordercolor=red align=center bgcolor=eeeeee>
               <tr valign=top><td id=a width=0 align=right nowrap>a</td><td id=b class=styled>b</td></tr></table>
             <table id=u border><tr><th id=c>c</th></tr></table>
             <font id=f size=+2 color='#333' face='Courier New, monospace'>x</font>
             <hr id=h size=4 noshade><img id=i align=left hspace=5 border>
             <ol id=o type=I><li id=l>x</li></ol>",
            ".styled { padding: 1px; text-align: left } #u th { border-style: dotted }",
        );
        let rgb = |color: Rgba| [color.r, color.g, color.b].map(|channel| (channel * 255.0).round() as u8);
        // As Chrome reads them (measured): the table's size, spacing, border and background.
        let table = by_id(&document, &styled, "t");
        assert_eq!(table.width, Size::Length(LengthPercentage::Percent(0.5)));
        assert_eq!(table.border_spacing, (4.0, 4.0));
        assert_eq!(table.border_width.top, 2.0);
        assert_eq!(table.border_style.top, BorderStyle::Solid);
        assert_eq!(table.border_color.top.map(rgb), Some([255, 0, 0]));
        assert_eq!(rgb(table.background_color), [0xee, 0xee, 0xee]);
        assert_eq!(table.margin.left, LengthPercentageAuto::Auto);
        // Its cells get its padding and a border in its color; a cell's own attributes apply,
        // a width of zero does not.
        let cell = by_id(&document, &styled, "a");
        assert_eq!(cell.padding.top, LengthPercentage::Px(6.0));
        assert_eq!(
            (cell.border_width.left, cell.border_style.left),
            (1.0, BorderStyle::Solid)
        );
        assert_eq!(cell.border_color.left.map(rgb), Some([255, 0, 0]));
        assert_eq!(
            (cell.text_align, cell.white_space, cell.vertical_align.clone()),
            (TextAlign::Right, WhiteSpace::NO_WRAP, VerticalAlign::Top)
        );
        assert_eq!(cell.width, Size::Auto);
        // Any rule overrides them, even the least specific.
        let styled_cell = by_id(&document, &styled, "b");
        assert_eq!(styled_cell.padding.top, LengthPercentage::Px(1.0));
        assert_eq!(styled_cell.text_align, TextAlign::Left);
        // `border` without a value is one pixel; its cells' borders are inset unless styled.
        assert_eq!(
            by_id(&document, &styled, "u").border_style.top,
            BorderStyle::Outset
        );
        assert_eq!(
            by_id(&document, &styled, "c").border_style.top,
            BorderStyle::Dotted
        );
        let font = by_id(&document, &styled, "f");
        assert_eq!(font.font_size, 24.0);
        assert_eq!(rgb(font.color), [0x33, 0x33, 0x33]);
        assert_eq!(&*font.font_family, ["Courier New", "monospace"]);
        let rule = by_id(&document, &styled, "h");
        assert_eq!(rule.height, Size::Length(LengthPercentage::Px(2.0)));
        assert_eq!(rgb(rule.background_color), [128, 128, 128]);
        let image = by_id(&document, &styled, "i");
        assert_eq!(image.float, Float::Left);
        assert_eq!(
            image.margin.right,
            LengthPercentageAuto::Length(LengthPercentage::Px(5.0))
        );
        assert_eq!(image.border_width.top, 1.0);
        assert_eq!(
            by_id(&document, &styled, "l").list_style_type,
            ListStyleType::UpperRoman
        );
    }

    #[test]
    fn turns_default_ligatures_off_for_text_rendering_optimize_speed() {
        let (document, styled) = styles(
            "<div id=a><p id=b></p><p id=c></p></div><p id=d></p>",
            "#a { text-rendering: optimizeSpeed }
             #c { font-variant-ligatures: common-ligatures contextual }
             #d { text-rendering: optimizeLegibility }",
        );
        let features = |id: &str| by_id(&document, &styled, id).font_features();
        assert_eq!(
            features("b"),
            [(*b"liga", 0), (*b"clig", 0), (*b"calt", 0)],
            "inherited"
        );
        // Ligatures asked for stay, and say so (against those an `@font-face` rule turns off).
        assert_eq!(
            features("c"),
            [(*b"liga", 1), (*b"clig", 1), (*b"calt", 1)],
            "ligatures asked for stay"
        );
        assert_eq!(features("d"), Vec::new());
    }

    #[test]
    fn keeps_the_angle_of_oblique() {
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p>",
            "#a { font-style: oblique } #b { font-style: oblique -12.5deg } #c { font-style: oblique 0.1turn }",
        );
        let style = |id: &str| by_id(&document, &styled, id).font_style;
        assert_eq!(style("a"), values::FontStyle::Oblique(140));
        assert_eq!(style("b"), values::FontStyle::Oblique(-125));
        assert_eq!(style("c"), values::FontStyle::Oblique(360));
    }

    #[test]
    fn computes_font_features() {
        let (document, styled) = styles(
            "<div id=a><p id=b></p><p id=c></p></div><p id=d></p><p id=e></p><p id=f></p>
             <div id=g><p id=h></p></div>",
            "#a { font-variant-numeric: tabular-nums slashed-zero; font-kerning: none }
             #b { font-variant-numeric: proportional-nums }
             #c { font-feature-settings: \"smcp\", \"liga\" 0, 'ss01' on, 'cv11' 2; letter-spacing: 1px }
             #d { font-variant-numeric: tabular-nums; font-variant-numeric: normal;
                  font-variant-numeric: lining-nums oldstyle-nums; font-feature-settings: 'longer' }
             #e { font-variant: small-caps no-common-ligatures discretionary-ligatures diagonal-fractions
                  super historical-forms full-width }
             #f { font-variant-caps: all-small-caps; font-variant-ligatures: none; font-synthesis: weight }
             #g { font-variant-numeric: tabular-nums; font-feature-settings: 'zero'; font-kerning: none }
             #h { font: 12px Inter }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let features = |id: &str| {
            style(id)
                .font_features()
                .iter()
                .map(|(tag, value)| format!("{}={value}", String::from_utf8_lossy(tag)))
                .collect::<Vec<_>>()
        };
        assert_eq!(features("a"), ["kern=0", "tnum=1", "zero=1"]);
        // A numeric value replaces the inherited one as a whole.
        assert_eq!(features("b"), ["kern=0", "pnum=1"]);
        // Letter spacing turns common and contextual ligatures off.
        assert_eq!(
            features("c"),
            ["kern=0", "liga=0", "clig=0", "calt=0", "tnum=1", "zero=1"]
        );
        assert_eq!(
            &*style("c").font_feature_settings,
            [(*b"smcp", 1), (*b"liga", 0), (*b"ss01", 1), (*b"cv11", 2)]
        );
        // Values that contradict themselves or name no feature are dropped.
        assert_eq!(features("d"), Vec::<String>::new());
        assert!(style("d").font_feature_settings.is_empty());
        assert_eq!(
            features("e"),
            [
                "liga=0", "clig=0", "dlig=1", "fwid=1", "frac=1", "hist=1", "sups=1"
            ]
        );
        assert_eq!(style("e").font_variant.caps, values::FontVariantCaps::SmallCaps);
        assert_eq!(features("f"), ["liga=0", "clig=0", "dlig=0", "hlig=0", "calt=0"]);
        assert_eq!(
            style("f").font_variant.caps,
            values::FontVariantCaps::AllSmallCaps
        );
        let synthesis = style("f").font_synthesis;
        assert!(synthesis.weight && !synthesis.style && !synthesis.small_caps);
        // `font` resets the font properties it cannot set.
        assert_eq!(features("h"), Vec::<String>::new());
        assert!(style("h").font_feature_settings.is_empty());
    }

    #[test]
    fn computes_clip_path_outlines() {
        use values::{BoxArea, ClipShape};
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div>",
            "#a { clip-path: path(\"M0 60 C 120 0, 240 110, 400 50 V 120 H 0 Z\") }
             #b { clip-path: circle(); clip-path: path(evenodd, 'M0 0 h10 v10 z') }
             #c { clip-path: circle(); clip-path: path(round, 'M0 0') }",
        );
        let clip = |id: &str| by_id(&document, &styled, id).clip_path.clone();
        let a = clip("a").expect("path");
        assert_eq!(
            (&a.shape, a.reference),
            (
                &ClipShape::Path {
                    even_odd: false,
                    data: "M0 60 C 120 0, 240 110, 400 50 V 120 H 0 Z".into(),
                },
                BoxArea::BorderBox
            )
        );
        assert!(matches!(
            &clip("b").expect("path").shape,
            ClipShape::Path { even_odd: true, data } if &**data == "M0 0 h10 v10 z"
        ));
        // A fill rule that is none is invalid and leaves the shape before it.
        assert!(matches!(
            clip("c").expect("circle").shape,
            ClipShape::Ellipse { .. }
        ));
    }

    #[test]
    fn computes_clip_paths() {
        use values::{BoxArea, ClipShape, LengthPercentage, ShapeRadius};
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { clip-path: polygon(evenodd, 50% 0, 100% 100%, 0 calc(100% - 10px)) }
             #b { clip-path: circle(20px at left top) padding-box }
             #c { clip-path: content-box }
             #d { clip-path: inset(5px 10%) }
             #e { clip-path: url(#shape) }",
        );
        let clip = |id: &str| by_id(&document, &styled, id).clip_path.clone();
        let a = clip("a").expect("polygon");
        let ClipShape::Polygon { even_odd, points } = &a.shape else {
            panic!("a polygon");
        };
        assert!(*even_odd);
        assert_eq!(points.len(), 3);
        assert_eq!(
            points[0],
            (LengthPercentage::Percent(0.5), LengthPercentage::Px(0.0))
        );
        assert_eq!(points[2].1.resolve(50.0), 40.0);
        let b = clip("b").expect("circle");
        assert_eq!(b.reference, BoxArea::PaddingBox);
        let ClipShape::Ellipse {
            radii: (ShapeRadius::Length(radius), _),
            circle: true,
            ..
        } = &b.shape
        else {
            panic!("a circle");
        };
        assert_eq!(radius.fixed(), Some(20.0));
        let c = clip("c").expect("box");
        assert_eq!((&c.shape, c.reference), (&ClipShape::Box, BoxArea::ContentBox));
        assert!(matches!(
            clip("d").map(|clip| clip.shape.clone()),
            Some(ClipShape::Inset { .. })
        ));
        // An SVG clip path is named by its URL.
        assert_eq!(
            clip("e").map(|clip| clip.shape.clone()),
            Some(ClipShape::Url(std::sync::Arc::from("#shape")))
        );
        assert!(by_id(&document, &styled, "a").is_stacking_context());
    }

    #[test]
    fn computes_filters() {
        use values::{Filter, Rgba};
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div>",
            "#a { filter: grayscale(50%) brightness(1.5) hue-rotate(0.25turn) drop-shadow(2px 3px 4px red) blur(1px) }
             #b { filter: url(#f) drop-shadow(1px 1px) opacity(0.5) }
             #c { filter: none }",
        );
        let filter = |id: &str| by_id(&document, &styled, id).filter.to_vec();
        assert_eq!(
            filter("a"),
            vec![
                Filter::Grayscale(0.5),
                Filter::Brightness(1.5),
                Filter::HueRotate(90.0),
                Filter::DropShadow {
                    x: 2.0,
                    y: 3.0,
                    blur: 4.0,
                    color: Some(Rgba::from_u8(255, 0, 0, 1.0)),
                },
                Filter::Blur(1.0),
            ]
        );
        // SVG filters are named by their URL; a shadow without a color has the element's.
        assert_eq!(
            filter("b"),
            vec![
                Filter::Url(std::sync::Arc::from("#f")),
                Filter::DropShadow {
                    x: 1.0,
                    y: 1.0,
                    blur: 0.0,
                    color: None,
                },
                Filter::Opacity(0.5),
            ]
        );
        assert_eq!(filter("c"), []);
        assert!(by_id(&document, &styled, "a").is_stacking_context());
        assert!(!by_id(&document, &styled, "c").is_stacking_context());
        // The color matrices of Filter Effects 1, clamped to the gamut.
        let red = Rgba::from_u8(255, 0, 0, 1.0);
        let gray = Filter::Grayscale(1.0).apply(red);
        assert!((gray.r - 0.2126).abs() < 1e-4 && (gray.g - gray.r).abs() < 1e-4);
        assert_eq!(Filter::Invert(1.0).apply(red), Rgba::from_u8(0, 255, 255, 1.0));
        assert_eq!(
            Filter::Brightness(2.0).apply(Rgba::from_u8(255, 0, 0, 0.5)).r,
            1.0
        );
        assert_eq!(Filter::Opacity(0.5).apply(red).a, 0.5);
        // An SVG color matrix in linear light: half of red's light is not half its sRGB value.
        let mut matrix = [0.0; 20];
        matrix[0] = 0.5;
        matrix[18] = 1.0;
        let half = Filter::ColorMatrix { matrix, linear: true }.apply(red);
        assert!((half.r - 0.7354).abs() < 1e-3 && half.g.abs() < 1e-6 && (half.a - 1.0).abs() < 1e-6);
        assert!(
            (Filter::ColorMatrix {
                matrix,
                linear: false
            }
            .apply(red)
            .r - 0.5)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn computes_transforms_in_depth() {
        use values::{LengthPercentage, TransformFunction};
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div>",
            "#a { transform: rotateY(60deg) translate3d(1px, 2px, 3px) scaleZ(2) perspective(400px) }
             #b { rotate: x 30deg; translate: 1px 2px 3px }
             #c { perspective: 500px; perspective-origin: left top }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        assert_eq!(
            style("a").transform[..],
            [
                TransformFunction::Rotate3d(0.0, 1.0, 0.0, 60.0),
                TransformFunction::Translate(LengthPercentage::Px(1.0), LengthPercentage::Px(2.0)),
                TransformFunction::TranslateZ(3.0),
                // A scale in depth is kept: a plane turned out of the page before scales with it.
                TransformFunction::Matrix3d([
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 1.0
                ]),
                TransformFunction::Perspective(400.0),
            ]
        );
        assert_eq!(style("b").rotate, Some((1.0, 0.0, 0.0, 30.0)));
        assert_eq!(
            style("b").translate,
            Some((LengthPercentage::Px(1.0), LengthPercentage::Px(2.0), 3.0))
        );
        let c = style("c");
        assert_eq!(c.perspective, Some(500.0));
        assert_eq!(
            c.perspective_origin,
            (LengthPercentage::Px(0.0), LengthPercentage::Px(0.0))
        );
        assert!(c.is_stacking_context());
    }

    #[test]
    fn computes_border_images() {
        use values::{BorderImageOutset, BorderImageRepeat, BorderImageWidth, Image, ImageSlice};
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div>",
            "#a { border: 10px solid; border-image: url(frame.png) 30 fill / 1 2em auto 5px / 4px repeat space }
             #b { border-image-source: linear-gradient(red, blue); border-image-slice: 1 }
             #c { border-image: url(frame.png) 30; border: 2px solid }",
        );
        let image = |id: &str| by_id(&document, &styled, id).border_image.clone();
        let a = image("a").expect("a border image");
        assert_eq!(a.source, Image::Url("frame.png".into(), 1.0));
        assert_eq!(a.slice, [ImageSlice::Number(30.0); 4]);
        assert!(a.fill);
        assert_eq!(a.width[0], BorderImageWidth::Number(1.0));
        assert!(matches!(&a.width[1], BorderImageWidth::Length(length) if length.fixed() == Some(32.0)));
        assert_eq!(a.width[2], BorderImageWidth::Auto);
        assert_eq!(a.outset, [BorderImageOutset::Length(4.0); 4]);
        assert_eq!(a.repeat, (BorderImageRepeat::Repeat, BorderImageRepeat::Space));
        let b = image("b").expect("b border image");
        assert!(matches!(b.source, Image::Gradient(_)));
        assert_eq!(b.repeat, (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch));
        // `border` resets the border image.
        assert!(image("c").is_none_or(|image| image.source == Image::None));
    }

    #[test]
    fn computes_masks() {
        use values::{
            BackgroundRepeat, BackgroundSize, BoxArea, Image, LengthPercentage, MaskComposite, MaskMode,
        };
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { -webkit-mask: url(icon.svg) center / contain no-repeat; }
             #b { mask-image: linear-gradient(black, transparent), url(x.png);
                  mask-mode: luminance, alpha; mask-composite: intersect; mask-clip: no-clip }
             #c { mask-size: 10px }
             #d { mask-image: url(a.png); mask-image: none }
             #e { mask: linear-gradient(white, black) luminance subtract }",
        );
        let mask = |id: &str| by_id(&document, &styled, id).mask.clone();
        let a = mask("a").expect("a mask");
        assert_eq!(a.images[..], [Image::Url("icon.svg".into(), 1.0)]);
        assert_eq!(a.sizes[..], [BackgroundSize::Contain]);
        assert_eq!(
            a.repeats[..],
            [(BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat)]
        );
        assert_eq!(
            (a.position_x[0].clone(), a.position_y[0].clone()),
            (LengthPercentage::Percent(0.5), LengthPercentage::Percent(0.5))
        );
        assert_eq!(a.origins[..], [BoxArea::BorderBox]);
        let b = mask("b").expect("b mask");
        assert!(matches!(b.images[0], Image::Gradient(_)));
        assert_eq!(b.modes[..], [MaskMode::Luminance, MaskMode::Alpha]);
        assert_eq!(b.composites[..], [MaskComposite::Intersect]);
        assert_eq!(b.clips[..], [None]);
        // Other mask properties alone mask nothing.
        assert!(mask("c").is_some_and(|mask| mask.is_none()));
        assert!(!by_id(&document, &styled, "c").is_stacking_context());
        assert!(mask("d").is_some_and(|mask| mask.is_none()));
        // The shorthand sets the mode and the composite too.
        let e = mask("e").expect("e mask");
        assert_eq!(
            (e.modes[0], e.composites[0]),
            (MaskMode::Luminance, MaskComposite::Subtract)
        );
        assert!(by_id(&document, &styled, "a").is_stacking_context());
    }

    #[test]
    fn prints_animations_at_time_zero() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div><p id=f></p>",
            "@keyframes fade { from { opacity: 0 } to { opacity: 1 } }
             @keyframes tint { 0% { background-color: rgb(0, 200, 0) } 100% { background-color: rgb(200, 0, 0) } }
             @keyframes grow { to { width: 50px; color: blue } }
             div { width: 10px }
             #a { animation: fade 2s }
             #b { animation: tint 10s linear -5s }
             #c { animation: fade 1s 1s }
             #d { animation: fade 1s 1s both; opacity: 0.5 !important }
             #e { --d: 4s; animation: grow var(--d) linear calc(-1 * var(--d)) forwards }
             #f { animation: fade 1s infinite paused }",
        );
        let style = |id| by_id(&document, &styled, id);
        // A fade-in prints transparent, as it starts.
        assert_eq!(style("a").opacity, 0.0);
        // Halfway, colors are mixed.
        let tint = style("b").background_color;
        assert!((tint.r - 100.0 / 255.0).abs() < 1e-4 && (tint.g - 100.0 / 255.0).abs() < 1e-4);
        // A delayed animation has no effect yet, unless it fills backwards; important
        // declarations win over it.
        assert_eq!(style("c").opacity, 1.0);
        assert_eq!(style("d").opacity, 0.5);
        // Over, filling forwards: the last keyframe's values, `var()` substituted.
        assert_eq!(style("e").width, values::Size::Length(LengthPercentage::Px(50.0)));
        assert_eq!(style("e").color, Rgba::from_u8(0, 0, 255, 1.0));
        assert_eq!(style("f").opacity, 0.0);
    }

    #[test]
    fn styles_footnotes_with_their_calls_and_markers() {
        let (document, styled) = styles(
            "<p id=p>Text<span id=a>Note</span><span id=b>Not</span><span id=c>Kept</span></p>",
            "span { float: footnote; font-size: 10px }
             #a { footnote-display: inline; footnote-policy: line; float-reference: column }
             #c { float-reference: column; float-reference: page }
             #b { position: absolute }
             #c { float: left; float: footnote; float: none }
             #a::footnote-call { color: red }",
        );
        let style = |id| by_id(&document, &styled, id);
        let a = style("a");
        assert!(a.footnote && a.float == values::Float::None);
        assert_eq!(a.display, Display::BLOCK);
        assert_eq!(a.footnote_display, values::FootnoteDisplay::Inline);
        assert_eq!(a.footnote_policy, values::FootnotePolicy::Line);
        assert_eq!(a.float_reference, values::FloatReference::Column);
        assert_eq!(style("c").float_reference, values::FloatReference::Page);
        assert_eq!(style("p").float_reference, values::FloatReference::Page);
        // Absolutely positioned boxes do not float, nor are they footnotes.
        assert!(!style("b").footnote);
        assert!(!style("c").footnote);
        assert!(!style("p").footnote);
        let node = document.find("span").expect("span");
        let call = styled.pseudo(node, PseudoKey::FootnoteCall).expect("call");
        assert_eq!(call.color, Rgba::from_u8(255, 0, 0, 1.0));
        assert_eq!(call.vertical_align, values::VerticalAlign::Super);
        assert!(!call.footnote);
        let marker = styled.pseudo(node, PseudoKey::FootnoteMarker).expect("marker");
        assert!(matches!(&marker.content, values::Content::Items(items) if items.len() == 2));
        let p = document.find("p").expect("p");
        assert!(styled.pseudo(p, PseudoKey::FootnoteCall).is_none());
    }

    #[test]
    fn styles_scroll_markers_groups_and_buttons() {
        use fagus_css::matching::ScrollDirection;
        let (document, styled) = styles(
            "<div id=s><p id=a></p></div><div id=n></div>",
            "#s { overflow: hidden; scroll-marker-group: after }
             #s::scroll-marker-group { display: flex; height: 10px }
             p::scroll-marker { content: '' }
             #n::scroll-button(*) { content: 'x' }
             #n::scroll-button(block-end) { color: green }",
        );
        let node = |id: &str| {
            document
                .descendants(NodeId::DOCUMENT)
                .find(|&node| {
                    document
                        .element(node)
                        .is_some_and(|data| data.id.as_deref() == Some(id))
                })
                .expect("element")
        };
        assert_eq!(
            by_id(&document, &styled, "s").scroll_marker_group,
            values::ScrollMarkerGroup::After
        );
        let group = styled
            .pseudo(node("s"), PseudoKey::ScrollMarkerGroup)
            .expect("group");
        assert_eq!(group.height, values::Size::Length(LengthPercentage::Px(10.0)));
        assert!(styled.pseudo(node("a"), PseudoKey::ScrollMarker).is_some());
        // `*` styles every button, a direction its own.
        let start = styled.pseudo(node("n"), PseudoKey::ScrollButton(ScrollDirection::BlockStart));
        assert!(start.is_some_and(|button| matches!(
            button.display,
            Display::Normal {
                outside: values::DisplayOutside::Inline,
                inside: DisplayInside::FlowRoot,
                ..
            }
        )));
        let end = styled
            .pseudo(node("n"), PseudoKey::ScrollButton(ScrollDirection::BlockEnd))
            .expect("block-end button");
        assert_eq!(end.color, Rgba::from_u8(0, 128, 0, 1.0));
        assert!(
            styled
                .pseudo(node("s"), PseudoKey::ScrollButton(ScrollDirection::BlockEnd))
                .is_none()
        );
    }

    #[test]
    fn finds_the_string_sets_of_elements_in_an_element_with_display_none() {
        fn id(document: &Document, id: &str) -> NodeId {
            document
                .descendants(NodeId::DOCUMENT)
                .find(|&node| {
                    document
                        .element(node)
                        .is_some_and(|data| data.id.as_deref() == Some(id))
                })
                .expect("element")
        }
        // The elements in one with `display: none` have no styles; where the document sets
        // named strings, those of them that do are listed in document order.
        let html = "<div id=a class=none><p id=b><i id=c class=set></i></p><i id=d class=set></i></div>";
        let (document, styled) = styles(html, ".none { display: none } .set { string-set: t content() }");
        let found: Vec<NodeId> = styled
            .hidden_string_sets(id(&document, "a"))
            .iter()
            .map(|(element, _)| *element)
            .collect();
        assert_eq!(found, [id(&document, "c"), id(&document, "d")]);
        assert!(styled.style(id(&document, "b")).is_none());
        assert!(styled.style(id(&document, "c")).is_none());
        assert_eq!(styled.hidden_string_sets(id(&document, "b")), []);
        // Without `string-set` in the document, they are not looked at.
        let (document, styled) = styles(html, ".none { display: none }");
        assert_eq!(styled.hidden_string_sets(id(&document, "a")), []);
    }

    #[test]
    fn styles_the_options_of_selects() {
        let (document, styled) = styles(
            "<select size=4><option id=a class=none>A</option><option id=b>B</option></select>",
            ".none { display: none }",
        );
        assert_eq!(by_id(&document, &styled, "a").display, Display::None);
        assert_ne!(by_id(&document, &styled, "b").display, Display::None);
    }

    #[test]
    fn leaves_properties_that_do_not_animate_to_the_element() {
        let (document, styled) = styles(
            "<div id=a></div>",
            "@keyframes bad { from { contain: none; opacity: 0 } }
             #a { contain: strict; animation: bad 1s paused }",
        );
        let style = by_id(&document, &styled, "a");
        assert!(style.contain_size && style.layout_containment);
        assert_eq!(style.opacity, 0.0);
    }

    #[test]
    fn fixed_and_sticky_boxes_are_stacking_contexts() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div>",
            "#a { position: fixed } #b { position: sticky } #c { position: absolute } \
             #d { position: relative; z-index: 0 }",
        );
        assert!(by_id(&document, &styled, "a").is_stacking_context());
        assert!(by_id(&document, &styled, "b").is_stacking_context());
        // An absolutely positioned box with `z-index: auto` is none.
        assert!(!by_id(&document, &styled, "c").is_stacking_context());
        assert!(by_id(&document, &styled, "d").is_stacking_context());
    }

    #[test]
    fn flex_and_grid_items_with_a_z_index_are_stacking_contexts() {
        let (document, styled) = styles(
            "<div style='display: grid'><i id=a></i><i id=b></i></div>\
             <div style='display: flex'><i id=c></i></div><div><i id=d></i></div>",
            "#a, #c, #d { z-index: 1 }",
        );
        assert!(by_id(&document, &styled, "a").is_stacking_context());
        assert_eq!(by_id(&document, &styled, "a").z_index, Some(1));
        assert!(!by_id(&document, &styled, "b").is_stacking_context());
        assert!(by_id(&document, &styled, "c").is_stacking_context());
        // In block flow `z-index` needs a position.
        assert!(!by_id(&document, &styled, "d").is_stacking_context());
    }

    #[test]
    fn computes_math_functions() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { width: round(17px, 5px); padding-left: round(up, 11px, 5px); margin-right: round(to-zero, -12px, 5px) }
             #b { width: mod(-7px, 5px); margin-left: rem(-7px, 5px); padding-right: hypot(3px, 4px) }
             #c { width: round(down, 55%, 20px); padding-left: abs(10px - 50%) }
             #d { width: calc(round(18px, 0px) + 10px) }
             #e { width: calc(100px * sin(30deg)) }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let px = |size: &values::Size| match size {
            values::Size::Length(length) => length.resolve(100.0),
            _ => f32::NAN,
        };
        assert_eq!(px(&style("a").width), 15.0);
        assert_eq!(style("a").padding.left.resolve(0.0), 15.0);
        assert_eq!(style("a").margin.right.resolve_or(0.0, f32::NAN), -10.0);
        // mod takes the sign of the divisor, rem that of the dividend.
        assert_eq!(px(&style("b").width), 3.0);
        assert_eq!(style("b").margin.left.resolve_or(0.0, f32::NAN), -2.0);
        assert_eq!(style("b").padding.right.resolve(0.0), 5.0);
        // With percentages, at layout: 55 of 100 down to a multiple of 20, |10 - 50|.
        assert_eq!(px(&style("c").width), 40.0);
        assert_eq!(style("c").padding.left.resolve(100.0), 40.0);
        // A step of zero makes the calculation NaN, which comes to 0.
        assert_eq!(px(&style("d").width), 0.0);
        assert!((px(&style("e").width) - 50.0).abs() < 0.01);
    }

    #[test]
    fn computes_white_space_and_text_wrap() {
        use values::{TextWrapStyle, WhiteSpace, WhiteSpaceCollapse};
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><div id=c><p id=d></p></div><p id=e></p>",
            "#a { text-wrap: nowrap }
             #b { white-space: pre; text-wrap-mode: wrap }
             #c { text-wrap: balance; white-space-collapse: preserve-breaks }
             #e { text-wrap: nowrap; text-wrap: pretty wrap }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        assert_eq!(style("a").white_space, WhiteSpace::NO_WRAP);
        assert_eq!(style("b").white_space, WhiteSpace::PRE_WRAP);
        // Inherited, both.
        assert_eq!(
            (style("d").white_space, style("d").text_wrap_style),
            (WhiteSpace::PRE_LINE, TextWrapStyle::Balance)
        );
        assert_eq!(
            style("d").white_space.collapse,
            WhiteSpaceCollapse::PreserveBreaks
        );
        assert_eq!(
            (style("e").white_space, style("e").text_wrap_style),
            (WhiteSpace::NORMAL, TextWrapStyle::Pretty)
        );
    }

    #[test]
    fn computes_line_break() {
        use values::LineBreak;
        let (document, styled) = styles(
            "<div id=a><p id=b></p></div><p id=c></p><div id=d><p id=e></p></div>",
            "#a { line-break: strict } #c { line-break: anywhere } #d { line-break: loose }
             #e { line-break: unset }",
        );
        let style = |id: &str| by_id(&document, &styled, id).line_break;
        // Inherited.
        assert_eq!((style("a"), style("b")), (LineBreak::Strict, LineBreak::Strict));
        assert_eq!(style("c"), LineBreak::Anywhere);
        assert_eq!(style("e"), LineBreak::Loose);
    }

    #[test]
    fn computes_word_break_auto_phrase() {
        use values::WordBreak;
        let (document, styled) = styles(
            "<div id=a><p id=b></p></div><p id=c></p>",
            "#a { word-break: auto-phrase } #c { word-break: keep-all; word-break: auto-phrase phrase }",
        );
        let style = |id: &str| by_id(&document, &styled, id).word_break;
        // Inherited; a value that is not one is dropped.
        assert_eq!(
            (style("a"), style("b")),
            (WordBreak::AutoPhrase, WordBreak::AutoPhrase)
        );
        assert_eq!(style("c"), WordBreak::KeepAll);
    }

    #[test]
    fn computes_flex_wrap_balance_and_line_count() {
        use values::FlexWrap;
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c><p id=d></p></div><div id=e></div>",
            "#a { flex-wrap: balance } #b { flex-wrap: balance wrap-reverse; flex-line-count: 3 }
             #c { flex-wrap: balance; flex-wrap: wrap } #e { flex-wrap: wrap; flex-wrap: balance nowrap;
             flex-line-count: 0 }",
        );
        let style = |id: &str| {
            let style = by_id(&document, &styled, id);
            (style.flex_wrap, style.flex_balance, style.flex_line_count)
        };
        assert_eq!(style("a"), (FlexWrap::Wrap, true, 1));
        assert_eq!(style("b"), (FlexWrap::WrapReverse, true, 3));
        // A later `flex-wrap` replaces it; neither is inherited.
        assert_eq!(style("c"), (FlexWrap::Wrap, false, 1));
        assert_eq!(style("d"), (FlexWrap::NoWrap, false, 1));
        // Values that are not valid are dropped.
        assert_eq!(style("e"), (FlexWrap::Wrap, false, 1));
    }

    #[test]
    fn computes_font_size_adjust() {
        use values::{FontMetric, FontSizeAdjust};
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><div id=c><p id=d></p></div><p id=e></p>",
            "#a { font-size-adjust: 0.5 }
             #b { font-size-adjust: cap-height 0.7 }
             #c { font: bold 12px Brand, sans-serif; font-size-adjust: ch-width from-font }
             #d { font-family: Other }
             #e { font-size-adjust: 0.5; font: 12px Inter }",
        );
        let adjust = |id: &str| by_id(&document, &styled, id).font_size_adjust.clone();
        assert_eq!(adjust("a"), FontSizeAdjust::Aspect(FontMetric::ExHeight, 0.5));
        assert_eq!(adjust("b"), FontSizeAdjust::Aspect(FontMetric::CapHeight, 0.7));
        // `from-font` takes the font of the element that declares it, also in its children.
        let FontSizeAdjust::FromFont(FontMetric::ChWidth, Some(request)) = adjust("d") else {
            panic!("from-font");
        };
        assert_eq!(
            (&*request.families, request.weight),
            (&["Brand".to_owned(), "sans-serif".to_owned()][..], 700.0)
        );
        // `font` resets it.
        assert_eq!(adjust("e"), FontSizeAdjust::None);
    }

    #[test]
    fn keeps_the_names_of_the_ui_generic_families() {
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p>",
            "#a { font-family: ui-sans-serif, system-ui, sans-serif } #b { font-family: ui-serif, ui-monospace, ui-rounded }",
        );
        let a = by_id(&document, &styled, "a");
        assert_eq!(&*a.font_family, ["ui-sans-serif", "system-ui", "sans-serif"]);
        let b = by_id(&document, &styled, "b");
        assert_eq!(&*b.font_family, ["ui-serif", "ui-monospace", "ui-rounded"]);
    }

    #[test]
    fn computes_widths_and_variations_of_fonts() {
        let (document, styled) = styles(
            "<div id=a><p id=b></p></div><p id=c></p><p id=d></p>",
            "#a { font-stretch: condensed; font-variation-settings: 'wght' 650, 'GRAD' 20.5;
                  font-optical-sizing: none }
             #c { font: expanded 12px Inter; font-variation-settings: 'wght' 1, wdth }
             #d { font-stretch: 80%; font-variation-settings: 'wght' 300; font-variation-settings: normal }",
        );
        let a = by_id(&document, &styled, "a");
        assert_eq!(a.font_stretch, 75.0);
        assert_eq!(&*a.font_variation_settings, [(*b"wght", 650.0), (*b"GRAD", 20.5)]);
        assert_eq!(a.font_optical_sizing, values::OpticalSizing::None);
        // All three are inherited.
        let b = by_id(&document, &styled, "b");
        assert_eq!((b.font_stretch, b.font_variation_settings.len()), (75.0, 2));
        assert_eq!(b.font_optical_sizing, values::OpticalSizing::None);
        // The shorthand sets the width; an invalid value is dropped.
        let c = by_id(&document, &styled, "c");
        assert_eq!((c.font_stretch, c.font_variation_settings.len()), (125.0, 0));
        let d = by_id(&document, &styled, "d");
        assert_eq!((d.font_stretch, d.font_variation_settings.len()), (80.0, 0));
        assert_eq!(d.font_optical_sizing, values::OpticalSizing::Auto);
    }

    #[test]
    fn computes_running_elements_and_named_strings() {
        let (document, styled) = styles(
            "<header id=h>Kopf</header><footer id=f>Fuß</footer><h2 id=t>Kapitel</h2><p id=p>x</p>",
            "header { position: running(header); }
             footer { position: absolute; position: running(footer); }
             p { position: running(x); position: relative; }
             h2 { string-set: chapter content(), title \"Teil \" content(before); }",
        );
        let header = by_id(&document, &styled, "h");
        assert_eq!(header.running.as_deref(), Some("header"));
        assert!(header.is_out_of_flow());
        let footer = by_id(&document, &styled, "f");
        assert_eq!(footer.running.as_deref(), Some("footer"));
        assert_eq!(footer.position, values::Position::Static);
        let paragraph = by_id(&document, &styled, "p");
        assert_eq!(paragraph.running, None, "a later position wins");
        assert_eq!(paragraph.position, values::Position::Relative);
        let heading = by_id(&document, &styled, "t");
        assert_eq!(
            heading.string_set,
            [
                (
                    "chapter".to_owned(),
                    vec![ContentItem::Content(ContentTarget::Text)]
                ),
                (
                    "title".to_owned(),
                    vec![
                        ContentItem::String("Teil ".to_owned()),
                        ContentItem::Content(ContentTarget::Before)
                    ]
                ),
            ]
        );
    }

    #[test]
    fn applies_user_agent_styles_and_inheritance() {
        let (document, styled) = styles("<body><h1 id=h>Title <em id=e>x</em></h1><p id=p>y</p>", "");
        let heading = by_id(&document, &styled, "h");
        assert_eq!(heading.display, Display::BLOCK);
        assert_eq!(heading.font_size, 32.0);
        assert_eq!(heading.font_weight, 700.0);
        assert_eq!(
            heading.margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(0.67 * 32.0))
        );
        let emphasis = by_id(&document, &styled, "e");
        assert_eq!(emphasis.font_size, 32.0, "inherited");
        assert_eq!(emphasis.font_weight, 700.0, "inherited");
        assert_eq!(
            by_id(&document, &styled, "p").margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(16.0))
        );
    }

    #[test]
    fn resolves_cascade_order_and_shorthands() {
        let css = "#x { margin: 1px 2px; } .a { margin-top: 5px !important } p { margin-top: 9px }
                   #x { border: 2px solid red; border-left: none } #x { padding-inline: 1em }";
        let (document, styled) = styles("<p id=x class=a style='margin-left: 7px'>", css);
        let style = by_id(&document, &styled, "x");
        assert_eq!(
            style.margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(5.0))
        );
        assert_eq!(
            style.margin.right,
            LengthPercentageAuto::Length(LengthPercentage::Px(2.0))
        );
        assert_eq!(
            style.margin.left,
            LengthPercentageAuto::Length(LengthPercentage::Px(7.0))
        );
        assert_eq!(style.border_width.top, 2.0);
        assert_eq!(style.border_style.top, BorderStyle::Solid);
        assert_eq!(style.border_color.top, Some(Rgba::from_u8(255, 0, 0, 1.0)));
        assert_eq!(
            style.border_width.left, 0.0,
            "border-style none computes the width to 0"
        );
        assert_eq!(style.padding.left, LengthPercentage::Px(16.0));
    }

    #[test]
    fn uses_registered_custom_properties() {
        let css = "@property --tone { syntax: '<color>'; inherits: false; initial-value: #16a34a; }
                   @property --size { syntax: '<length>'; inherits: true; initial-value: 4px; }
                   #outer { --tone: blue; --size: 9px }
                   #outer, #inner { color: var(--tone, red); padding-top: var(--size) }
                   #inner::before { content: var(--label, 'Hinweis') }";
        let (document, styled) = styles("<div id=outer><p id=inner></p></div>", css);
        let outer = by_id(&document, &styled, "outer");
        assert_eq!(outer.color, Rgba::from_u8(0, 0, 255, 1.0));
        let inner = by_id(&document, &styled, "inner");
        assert_eq!(
            inner.color,
            Rgba::from_u8(0x16, 0xa3, 0x4a, 1.0),
            "not inherited: initial value"
        );
        assert_eq!(inner.padding.top, LengthPercentage::Px(9.0), "inherited");
        let node = document
            .descendants(NodeId::DOCUMENT)
            .find(|&node| {
                document
                    .element(node)
                    .is_some_and(|data| data.id.as_deref() == Some("inner"))
            })
            .expect("inner");
        let before = styled.pseudo(node, PseudoKey::Before).expect("::before");
        assert!(
            matches!(&before.content, values::Content::Items(items) if items.len() == 1),
            "var() in a property lightningcss does not parse: {:?}",
            before.content
        );
    }

    #[test]
    fn takes_css_wide_keywords_in_custom_properties() {
        // Tailwind resets its registered properties with `initial`, which makes `var()` take the
        // fallback; `inherit` and `unset` take the parent's value.
        let css = "@property --lead { syntax: '*'; inherits: false; }
                   #outer { --gap: 7px; --lead: 3px }
                   #a { --lead: initial; padding-top: var(--lead, 5px) }
                   #b { --gap: initial; padding-top: var(--gap, 6px) }
                   #c { --gap: inherit; padding-top: var(--gap, 1px) }
                   #d { --gap: unset; --lead: unset; padding-top: var(--gap); padding-left: var(--lead, 2px) }";
        let (document, styled) = styles(
            "<div id=outer><p id=a></p><p id=b></p><p id=c></p><p id=d></p></div>",
            css,
        );
        let top = |id| by_id(&document, &styled, id).padding.top.clone();
        assert_eq!(top("a"), LengthPercentage::Px(5.0));
        assert_eq!(top("b"), LengthPercentage::Px(6.0));
        assert_eq!(top("c"), LengthPercentage::Px(7.0));
        assert_eq!(top("d"), LengthPercentage::Px(7.0));
        assert_eq!(
            by_id(&document, &styled, "d").padding.left,
            LengthPercentage::Px(2.0),
            "a registered property that does not inherit is reset"
        );
    }

    #[test]
    fn centres_header_cells_unless_an_ancestor_aligns_text() {
        use crate::values::TextAlign;
        let (document, styled) = styles(
            "<table><tr><th id=a>a</th></tr></table>
             <table style='text-align: left'><tr><th id=b>b</th></tr></table>
             <table><tr style='text-align: end'><th id=c>c</th></tr></table>
             <div style='text-align: right'><table><tr><th id=d style='text-align: -webkit-match-parent'>d</th></tr></table></div>",
            "",
        );
        let align = |id| by_id(&document, &styled, id).text_align;
        assert_eq!(align("a"), TextAlign::Center);
        assert_eq!(align("b"), TextAlign::Left);
        assert_eq!(align("c"), TextAlign::End);
        assert_eq!(align("d"), TextAlign::Right);
    }

    #[test]
    fn matches_the_root_parent_by_its_own_start() {
        // The root has no parent to match: `match-parent` is `start` there, which its own
        // direction places on the right (text-align-match-parent-root-rtl).
        use crate::values::TextAlign;
        let (document, styled) = styles(
            "<html dir=rtl id=root><body id=body><p id=p style='text-align: match-parent'>p</p></body></html>",
            "html { text-align: match-parent }",
        );
        let align = |id| by_id(&document, &styled, id).text_align;
        assert_eq!(align("root"), TextAlign::Start);
        assert_eq!(align("p"), TextAlign::Right);
    }

    #[test]
    fn applies_scoped_rules_between_their_roots_and_limits() {
        let (document, styled) = styles(
            "<div class=card id=root><p id=in>in</p><div class=content><p id=below>below</p></div>
               <div class=card><p id=nested>nested</p></div></div><p id=out>out</p>
             <div class=far><div class=near><p id=near>near</p></div></div>",
            "@scope (.card) to (.content) { p { color: rgb(255, 0, 0) } :scope { color: rgb(0, 0, 255) } }
             p { color: rgb(0, 128, 0) }
             @scope (.near) { p { color: rgb(1, 1, 1) } }
             @scope (.far) { p { color: rgb(2, 2, 2) } }",
        );
        let color = |id: &str| by_id(&document, &styled, id).color;
        let rgb = |r: u8, g: u8, b: u8| Rgba::from_u8(r, g, b, 1.0);
        // In the scope, a scoped rule beats a later one of the same specificity; below a limit
        // and outside, it does not apply; `:scope` is the root itself.
        assert_eq!(color("in"), rgb(255, 0, 0));
        assert_eq!(color("below"), rgb(0, 128, 0));
        assert_eq!(color("out"), rgb(0, 128, 0));
        assert_eq!(color("root"), rgb(0, 0, 255));
        assert_eq!(color("nested"), rgb(255, 0, 0));
        // Of two scopes, the nearer root wins.
        assert_eq!(color("near"), rgb(1, 1, 1));
    }

    #[test]
    fn picks_the_candidate_of_an_image_set_that_chrome_prints() {
        use crate::values::Image;
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e></p><p id=f></p>",
            r#"#a { background-image: image-set("x2.png" 2x, "x1.png" 1x, "x3.png" 3x) }
               #b { background-image: -webkit-image-set("x2.png" 2x, "x3.png" 3x) }
               #c { background-image: image-set("half.png" 0.5x, "quarter.png" 48dpi) }
               #d { background-image: image-set("x.jxl" type("image/jxl"), "x.png" type("image/png")) }
               #e { background-image: image-set("x.jxl" type("image/jxl"), "x.tif" type("image/tiff")) }
               #f { background-image: image-set("x.avif" type("image/avif"), "x.png" type("image/png")) }"#,
        );
        let image = |id: &str| by_id(&document, &styled, id).background_image[0].clone();
        // The lowest resolution from 1x up, the highest below where there is none.
        assert_eq!(image("a"), Image::Url("x1.png".into(), 1.0));
        assert_eq!(image("b"), Image::Url("x2.png".into(), 2.0));
        assert_eq!(image("c"), Image::Url("half.png".into(), 0.5));
        // Of the types Fagus shows.
        assert_eq!(image("d"), Image::Url("x.png".into(), 1.0));
        assert_eq!(image("e"), Image::Url("x.tif".into(), 1.0));
        assert_eq!(image("f"), Image::Url("x.avif".into(), 1.0));
    }

    #[test]
    fn computes_background_layers_and_gradients() {
        use values::{BackgroundRepeat, BackgroundSize, ColorSpace, GradientKind, Image, LinearDirection};
        let css = "#a { background: red; background: linear-gradient(to right in oklab, #15803d, #22c55e) }
                   #b { background: linear-gradient(#16a34a, #16a34a), red }
                   #c { --direction: to right in oklch; background-image: linear-gradient(var(--direction), red, blue) }
                   #d { background: url(logo.png) no-repeat right 10px top / 20px auto }";
        let (document, styled) = styles("<p id=a></p><p id=b></p><p id=c></p><p id=d></p>", css);
        let gradient = |id: &str| match &by_id(&document, &styled, id).background_image[0] {
            Image::Gradient(gradient) => std::sync::Arc::clone(gradient),
            other => panic!("{id}: no gradient but {other:?}"),
        };
        let a = gradient("a");
        assert_eq!(a.kind, GradientKind::Linear(LinearDirection::Angle(90.0)));
        assert_eq!(
            a.interpolation.map(|method| method.space),
            Some(ColorSpace::Oklab)
        );
        assert_eq!(a.stops.len(), 2);
        assert_eq!(by_id(&document, &styled, "a").background_color, Rgba::TRANSPARENT);
        let b = by_id(&document, &styled, "b");
        assert_eq!(b.background_image.len(), 2, "the last layer holds the color");
        assert_eq!(b.background_image[1], Image::None);
        assert_eq!(b.background_color, Rgba::from_u8(255, 0, 0, 1.0));
        assert_eq!(
            gradient("c").interpolation.map(|method| method.space),
            Some(ColorSpace::Oklch)
        );
        let d = by_id(&document, &styled, "d");
        assert_eq!(d.background_image[0], Image::Url("logo.png".into(), 1.0));
        assert_eq!(
            d.background_repeat[0],
            (BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat)
        );
        assert_eq!(d.background_position_x[0].resolve(200.0), 190.0);
        assert!(matches!(
            &d.background_size[0],
            BackgroundSize::Explicit(
                LengthPercentageAuto::Length(LengthPercentage::Px(20.0)),
                LengthPercentageAuto::Auto
            )
        ));
    }

    #[test]
    fn propagates_text_decorations_to_descendants() {
        let css = "a { text-decoration: underline; color: blue } .badge { display: inline-block }";
        let (document, styled) = styles(
            "<p><a id=link><b id=bold>x</b><span id=badge class=badge>y</span></a></p>",
            css,
        );
        let bold = by_id(&document, &styled, "bold");
        assert_eq!(
            bold.decorations.len(),
            1,
            "the link's underline reaches its children"
        );
        assert_eq!(bold.decorations[0].color, Rgba::from_u8(0, 0, 255, 1.0));
        assert!(
            by_id(&document, &styled, "badge").decorations.is_empty(),
            "but not atomic inlines"
        );
    }

    #[test]
    fn decorations_follow_the_font_of_the_blocks_they_reach() {
        // Chrome draws the underline of a `div` as thick and as far down in a block, table cell
        // or grid item inside it as that box's font makes it, and in an inline box or behind
        // `display: contents` as the nearest such box's.
        let css = "#d { text-decoration: underline; font-size: 16px }
                   #h, #g, #s, #c { font-size: 80px } #i { font-size: 20px }";
        let (document, styled) = styles(
            "<div id=d><div id=h><span id=i>x</span></div><span id=s>y</span>\
             <div style='display: contents' id=c>z</div><div style='display: grid'><p id=g>g</p></div></div>",
            css,
        );
        let size = |id: &str| by_id(&document, &styled, id).decorations[0].font_size;
        assert!((size("h") - 80.0).abs() < 0.01, "a block's own font");
        assert!(
            (size("i") - 80.0).abs() < 0.01,
            "the block's font in an inline box inside it"
        );
        assert!(
            (size("s") - 16.0).abs() < 0.01,
            "the decorating box's font in an inline box"
        );
        assert!((size("c") - 16.0).abs() < 0.01, "not at `display: contents`");
        assert!((size("g") - 80.0).abs() < 0.01, "a grid item's own font");
    }

    #[test]
    fn computes_modern_colors_and_variables() {
        let css = ":root { --accent: oklch(0.488 0.243 264.376); --spacing: 0.25rem }
                   @layer utilities { .p-4 { padding: calc(var(--spacing) * 4) } }
                   #x { color: var(--accent); background-color: color-mix(in oklab, var(--accent) 50%, white) }
                   #y { width: calc(100% - 2 * var(--spacing)); color: var(--missing, rgb(1 2 3)) }";
        let (document, styled) = styles("<div id=x class=p-4></div><div id=y></div>", css);
        let x = by_id(&document, &styled, "x");
        assert!(x.color.b > 0.8 && x.color.r < 0.3, "oklch blue: {:?}", x.color);
        assert!(x.background_color.r > x.color.r, "mixed with white");
        assert_eq!(x.padding.top, LengthPercentage::Px(16.0));
        let y = by_id(&document, &styled, "y");
        assert_eq!(y.color, Rgba::from_u8(1, 2, 3, 1.0));
        match &y.width {
            values::Size::Length(length) => assert_eq!(length.resolve(200.0), 192.0),
            other => panic!("unexpected width {other:?}"),
        }
    }

    #[test]
    fn parses_print_properties() {
        let css = "tr { break-inside: avoid } p { orphans: 3; widows: 4; page-break-before: always }
                   table { border-collapse: collapse; display: table }
                   h1::before { content: 'Kapitel ' counter(chapter) ': ' }";
        let (document, styled) = styles(
            "<table id=t><tr id=r><td>1</td></tr></table><p id=p>x</p><h1 id=h>T</h1>",
            css,
        );
        assert_eq!(by_id(&document, &styled, "r").break_inside, BreakInside::Avoid);
        let paragraph = by_id(&document, &styled, "p");
        assert_eq!((paragraph.orphans, paragraph.widows), (3, 4));
        assert!(paragraph.break_before.is_forced_page_break());
        let table = by_id(&document, &styled, "t");
        assert_eq!(table.border_collapse, values::BorderCollapse::Collapse);
        assert!(matches!(
            table.display,
            Display::Normal {
                inside: DisplayInside::Table,
                ..
            }
        ));
        let heading = document.find("h1").unwrap();
        let before = styled.pseudo(heading, PseudoKey::Before).expect("::before style");
        assert!(matches!(&before.content, values::Content::Items(items) if items.len() == 3));
    }

    #[test]
    fn parses_images_in_content() {
        let (document, styled) = styles(
            "<p></p>",
            "p::before { content: 'a' url(icon.png) url(\"b c.svg\") counter(x) }",
        );
        let paragraph = document.find("p").unwrap();
        let before = styled
            .pseudo(paragraph, PseudoKey::Before)
            .expect("::before style");
        let values::Content::Items(items) = &before.content else {
            panic!("{:?}", before.content);
        };
        assert_eq!(items[1], ContentItem::Image("icon.png".to_owned()));
        assert_eq!(items[2], ContentItem::Image("b c.svg".to_owned()));
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn handles_css_wide_keywords() {
        let css = "#a { color: red; margin-top: 3px } #b { color: initial; margin-top: inherit } #c { color: unset }";
        let (document, styled) = styles("<div id=a><p id=b><span id=c></span></p></div>", css);
        let b = by_id(&document, &styled, "b");
        assert_eq!(b.color, Rgba::BLACK);
        assert_eq!(
            b.margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(3.0))
        );
        assert_eq!(by_id(&document, &styled, "c").color, Rgba::BLACK);
    }

    fn node(document: &Document, id: &str) -> NodeId {
        document
            .descendants(NodeId::DOCUMENT)
            .find(|&node| {
                document
                    .element(node)
                    .is_some_and(|data| data.id.as_deref() == Some(id))
            })
            .expect("element with id")
    }

    #[test]
    fn shares_styles_only_between_elements_that_compute_alike() {
        use crate::values::{LengthPercentage, LengthPercentageAuto, Size, TextAlign};
        let (document, styled) = styles(
            r#"<!doctype html><html style="font-size: 20px">
            <div id="a" style="color: red"><p id="a1" class="x">1</p><p id="a2" class="x" lang="de">2</p>
              <p id="a3" class="x" style="margin-top: 3px">3</p><p id="a4" class="x">4</p></div>
            <div id="b"><p id="b1" class="x">1</p><p id="b2" class="x">2</p></div>
            <div class="x" style="font-size: 10px"><p id="c1" class="x">1</p></div>
            <ul><li><span id="s1" class="x">x</span></li></ul>
            <ul style="color: blue"><li><span id="s2" class="x">x</span></li></ul>
            <table id="t" class="c"></table><div id="d" class="c"></div>"#,
            ".x { padding: 1em; width: 2rem; color: inherit } .c { text-align: -webkit-center }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let red = style("a").color;
        assert_eq!(style("a1").color, red);
        assert_eq!(style("a4").color, red);
        assert_eq!(style("b1").color, Rgba::BLACK);
        assert_eq!(style("s1").color, Rgba::BLACK);
        assert_ne!(style("s2").color, Rgba::BLACK, "inherited through the list item");
        assert_eq!(style("a1").lang, None);
        assert_eq!(style("a2").lang.as_deref(), Some("de"));
        assert_eq!(style("a4").lang, None);
        assert_eq!(
            style("a3").margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(3.0))
        );
        assert_eq!(style("a1").margin.top, style("a4").margin.top);
        assert_ne!(style("a1").margin.top, style("a3").margin.top);
        // `em` from the parent's font, `rem` from the root's.
        assert_eq!(style("b1").padding.top.resolve(0.0), 20.0);
        assert_eq!(style("c1").padding.top.resolve(0.0), 10.0);
        assert_eq!(style("c1").width, Size::Length(LengthPercentage::Px(40.0)));
        // A table goes back to `start`, other elements keep the alignment.
        assert_eq!(style("t").text_align, TextAlign::Start);
        assert_eq!(style("d").text_align, TextAlign::Center);
        // Equal styles, each element its own (layout tells elements apart by their styles).
        let (b1, b2) = (node(&document, "b1"), node(&document, "b2"));
        let (b1, b2) = (styled.style(b1).expect("b1"), styled.style(b2).expect("b2"));
        assert!(!std::sync::Arc::ptr_eq(b1, b2));
        assert_eq!(format!("{b1:?}"), format!("{b2:?}"));
    }

    #[test]
    fn shares_no_styles_between_elements_with_their_own_declarations() {
        use crate::values::{LengthPercentage, LengthPercentageAuto};
        // Inline styles, parsed for each element, and what elements inherit of their `lang`.
        let paragraphs: Vec<String> = (1..=24)
            .map(|number| format!(r#"<p id="p{number}" class="x" style="margin-top: {number}px"></p>"#))
            .collect();
        let paragraphs = paragraphs.concat();
        let (document, styled) = styles(
            &format!(
                r#"<!doctype html>{paragraphs}<p class="x" lang="de"><span id="de" class="y"></span></p>
                <p class="x"><span id="none" class="y"></span></p>"#
            ),
            ".x { padding: 1px } .y { color: green }",
        );
        for number in 1..=24 {
            assert_eq!(
                by_id(&document, &styled, &format!("p{number}")).margin.top,
                LengthPercentageAuto::Length(LengthPercentage::Px(number as f32))
            );
        }
        assert_eq!(by_id(&document, &styled, "de").lang.as_deref(), Some("de"));
        assert_eq!(by_id(&document, &styled, "none").lang, None);
    }

    #[test]
    fn shares_no_styles_between_containers_of_other_sizes() {
        use crate::values::{LengthPercentage, Size};
        let document = Document::parse_html(
            r#"<!doctype html><div id="a" class="box"><p id="a1" class="w"></p></div>
            <div id="b" class="box"><p id="b1" class="w"></p></div>"#,
        );
        let ua = Stylesheet::parse(USER_AGENT_CSS, Origin::UserAgent, "ua.css");
        let author = Stylesheet::parse(
            ".box { container-type: inline-size } .w { width: 50cqw }",
            Origin::Author,
            "test.css",
        );
        let environment = MediaEnvironment::default();
        let rules = RuleSet::new(&[&ua, &author], &environment);
        let mut sizes = ContainerSizes::default();
        for (id, width) in [("a", 200.0), ("b", 400.0)] {
            sizes.insert(
                node(&document, id),
                fagus_css::ContainerSize {
                    width,
                    height: None,
                    font_size: 16.0,
                },
            );
        }
        let styled = compute_styles_in(&document, &rules, (environment.width, environment.height), &sizes);
        let width = |id: &str| by_id(&document, &styled, id).width.clone();
        assert_eq!(width("a1"), Size::Length(LengthPercentage::Px(100.0)));
        assert_eq!(width("b1"), Size::Length(LengthPercentage::Px(200.0)));
    }

    #[test]
    fn shares_custom_properties_that_are_only_restated() {
        use crate::values::{LengthPercentage, LengthPercentageAuto};
        let (document, styled) = styles(
            r#"<!doctype html><div id="o" class="p"><div id="i" class="q"><div id="j" class="q"></div></div></div>"#,
            "* { --tw: 0px; --same: 1px } .p { --tw: 5px } .q { margin-left: var(--tw); margin-right: var(--same) }",
        );
        let style = |id: &str| by_id(&document, &styled, id);
        let px = |value: f32| LengthPercentageAuto::Length(LengthPercentage::Px(value));
        // `*` sets `--tw` back on the inner elements.
        assert_eq!(style("i").margin.left, px(0.0));
        assert_eq!(style("i").margin.right, px(1.0));
        assert_eq!(style("j").margin.left, px(0.0));
        assert!(!std::sync::Arc::ptr_eq(
            &style("o").custom_properties,
            &style("i").custom_properties
        ));
        // `j` restates what `i` has: the properties are shared.
        assert!(std::sync::Arc::ptr_eq(
            &style("i").custom_properties,
            &style("j").custom_properties
        ));
    }

    #[test]
    fn computes_before_and_after_only_with_content() {
        let (document, styled) = styles(
            r#"<!doctype html><p id="a" class="m">a</p><p id="b" class="m n">b</p><p id="c" class="m o">c</p>"#,
            ".m::before, .m::after { color: red; --x: 1 } .n::after { content: 'x' } .o::before { all: inherit }",
        );
        let pseudo = |id: &str, key: PseudoKey| styled.pseudo(node(&document, id), key).is_some();
        assert!(!pseudo("a", PseudoKey::Before) && !pseudo("a", PseudoKey::After));
        assert!(!pseudo("b", PseudoKey::Before) && pseudo("b", PseudoKey::After));
        assert_eq!(
            styled
                .pseudo(node(&document, "b"), PseudoKey::After)
                .map(|style| style.color),
            Some(Rgba::from_u8(255, 0, 0, 1.0))
        );
        assert!(!pseudo("c", PseudoKey::Before));
    }

    #[test]
    fn marks_the_styles_of_before_pseudo_elements_with_their_element() {
        let (document, styled) = styles(
            r#"<!doctype html><p id="a">a<span id="s">s</span></p>"#,
            "#a::before, #a::after { content: ''; position: absolute }",
        );
        let (a, span) = (node(&document, "a"), node(&document, "s"));
        let pseudo = |key| styled.pseudo(a, key).expect("pseudo-element");
        assert_eq!(pseudo(PseudoKey::Before).before_of, Some(a));
        assert_eq!(pseudo(PseudoKey::After).before_of, None);
        // Not inherited.
        assert_eq!(pseudo(PseudoKey::Before).inherit().before_of, None);
        assert_eq!(styled.style(span).expect("span").before_of, None);
    }

    #[test]
    fn inherits_radii_alignment_and_text_decoration_with_inherit() {
        // A bar inside a rounded track takes its corners with `border-radius: inherit`.
        let (document, styled) = styles(
            "<div id=track><i id=bar></i><i id=plain></i></div>",
            "#track { border-radius: 4px 8px; display: flex; flex-direction: column; align-items: center;
                      justify-content: end; flex-wrap: wrap; aspect-ratio: 2; word-break: break-all;
                      overflow-wrap: anywhere; text-decoration: underline dotted red }
             #bar { border-radius: inherit; align-items: inherit; justify-content: inherit; flex-direction: inherit;
                    flex-wrap: inherit; aspect-ratio: inherit; text-decoration: inherit }
             #plain { word-break: unset; overflow-wrap: unset }",
        );
        let track = by_id(&document, &styled, "track");
        let bar = by_id(&document, &styled, "bar");
        assert_eq!(bar.border_radius, track.border_radius);
        assert_ne!(bar.border_radius, ComputedStyle::default().border_radius);
        assert_eq!(bar.align_items, track.align_items);
        assert_eq!(bar.justify_content, track.justify_content);
        assert_eq!(bar.flex_direction, track.flex_direction);
        assert_eq!(bar.flex_wrap, track.flex_wrap);
        assert_eq!(bar.aspect_ratio, Some(2.0));
        assert_eq!(bar.text_decoration_line, track.text_decoration_line);
        assert_eq!(bar.text_decoration_style, track.text_decoration_style);
        assert_eq!(bar.text_decoration_color, track.text_decoration_color);
        // `word-break` and `overflow-wrap` are inherited: `unset` is `inherit`.
        let plain = by_id(&document, &styled, "plain");
        assert_eq!(plain.word_break, track.word_break);
        assert_eq!(plain.overflow_wrap, track.overflow_wrap);
    }

    #[test]
    fn marks_backgrounds_clipped_to_text() {
        let (document, styled) = styles(
            "<h1 id=a>Title</h1><h1 id=b>Title</h1><h1 id=c>Title</h1><p id=d><span id=e>x</span></p>",
            "#a { background-clip: text } #b { -webkit-background-clip: text } #c { background-clip: padding-box, text }
             #d { background-clip: text } #e { background-clip: inherit }",
        );
        for id in ["a", "b", "c", "d", "e"] {
            assert!(by_id(&document, &styled, id).clips_background_to_text(), "{id}");
        }
        let (document, styled) = styles("<p id=a></p>", "#a { background-clip: content-box }");
        assert!(!by_id(&document, &styled, "a").clips_background_to_text());
    }

    #[test]
    fn inherits_the_text_fill_color() {
        let (document, styled) = styles(
            "<p id=a><span id=b></span></p><p id=c></p>",
            "#a { -webkit-text-fill-color: transparent } #c { -webkit-text-fill-color: red;
             -webkit-text-fill-color: currentcolor }",
        );
        assert_eq!(
            by_id(&document, &styled, "a").text_fill_color,
            Some(Rgba::TRANSPARENT)
        );
        assert_eq!(
            by_id(&document, &styled, "b").text_fill_color,
            Some(Rgba::TRANSPARENT)
        );
        assert_eq!(by_id(&document, &styled, "c").text_fill_color, None);
    }

    #[test]
    fn marks_backgrounds_clipped_to_the_border_area() {
        use crate::values::BoxArea;
        let (document, styled) = styles(
            "<p id=a></p><p id=b></p><p id=c></p><p id=d><span id=e></span></p><p id=f></p><p id=g></p>",
            "#a { background-clip: border-area } #b { background-clip: border-area, content-box }
             #c { background-clip: border-area text } #d { background-clip: border-area }
             #e { background-clip: inherit } #f { background-clip: border-area; background: red }
             #g { background-clip: padding-box; background-clip: border-area bogus }",
        );
        for id in ["a", "b", "c", "e"] {
            assert!(
                by_id(&document, &styled, id).clips_background_to_border_area(),
                "{id}"
            );
        }
        let b = by_id(&document, &styled, "b");
        assert_eq!(&*b.background_clip_border_area, &[true, false]);
        assert_eq!(&*b.background_clip, &[BoxArea::BorderBox, BoxArea::ContentBox]);
        assert!(by_id(&document, &styled, "c").clips_background_to_text());
        assert!(!by_id(&document, &styled, "f").clips_background_to_border_area());
        let g = by_id(&document, &styled, "g");
        assert!(!g.clips_background_to_border_area());
        assert_eq!(&*g.background_clip, &[BoxArea::PaddingBox]);
    }

    #[test]
    fn styles_the_track_and_value_of_progress_bars() {
        let (document, styled) = styles(
            "<progress id=a value=1></progress><progress id=b></progress><div id=c></div>",
            "#a { color: navy } #a::-webkit-progress-bar { background-color: #ddd; border-radius: 3px }
             #a::-webkit-progress-value { background-color: rgb(0, 0, 255) }",
        );
        let pseudo = |id: &str, key: PseudoKey| styled.pseudo(node(&document, id), key).cloned();
        let bar = pseudo("a", PseudoKey::ProgressBar).expect("bar");
        let value = pseudo("a", PseudoKey::ProgressValue).expect("value");
        assert_eq!(bar.background_color, Rgba::from_u8(0xdd, 0xdd, 0xdd, 1.0));
        assert_eq!(value.background_color, Rgba::from_u8(0, 0, 255, 1.0));
        // The value inherits from the track, which inherits from the element.
        assert_eq!(value.color, Rgba::from_u8(0, 0, 0x80, 1.0));
        assert_eq!(bar.height, values::Size::Length(LengthPercentage::Percent(1.0)));
        // Without author styles the parts are Chrome's: a gray track and a green value.
        let bar = pseudo("b", PseudoKey::ProgressBar).expect("bar");
        let value = pseudo("b", PseudoKey::ProgressValue).expect("value");
        assert_eq!(bar.background_color, Rgba::from_u8(128, 128, 128, 1.0));
        assert_eq!(value.background_color, Rgba::from_u8(0, 128, 0, 1.0));
        assert!(pseudo("c", PseudoKey::ProgressBar).is_none());
    }

    #[test]
    fn borders_start_medium_wide() {
        let (document, styled) = styles(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>",
            "#a { border-style: solid } #b { border-top-style: dashed } #c { border: 1px solid; border-width: initial }
             #d { border: 1px solid; border-left-width: unset } #e { border-width: 5px }",
        );
        let widths = |id| {
            let style = by_id(&document, &styled, id);
            let sides = style.border_width;
            [sides.top, sides.right, sides.bottom, sides.left]
        };
        assert_eq!(widths("a"), [3.0; 4]);
        assert_eq!(widths("b"), [3.0, 0.0, 0.0, 0.0]);
        assert_eq!(widths("c"), [3.0; 4]);
        assert_eq!(widths("d"), [1.0, 1.0, 1.0, 3.0]);
        // A width without a style is no border.
        assert_eq!(widths("e"), [0.0; 4]);
        // Styles that are not computed from declarations have no borders.
        assert_eq!(ComputedStyle::default().border_width.top, 0.0);
    }

    #[test]
    fn inherits_border_widths_that_the_parent_style_hides() {
        let (document, styled) = styles(
            "<div id=p><div id=a></div><div id=b></div></div><div id=q><div id=c></div></div>",
            "#a { border-style: solid; border-left-width: inherit } #q { border-width: 7px }
             #b { border-style: solid; border-width: inherit } #c { border-style: solid; border-top-width: inherit }",
        );
        let widths = |id| {
            let style = by_id(&document, &styled, id);
            let sides = style.border_width;
            [sides.top, sides.right, sides.bottom, sides.left]
        };
        // The parents have no border style, so no borders, but their widths are inherited as
        // the cascade gives them, as in Chrome: `medium` and 7px.
        assert_eq!(widths("p"), [0.0; 4]);
        assert_eq!(widths("a"), [3.0; 4]);
        assert_eq!(widths("b"), [3.0; 4]);
        assert_eq!(widths("c")[0], 7.0);
    }

    #[test]
    fn applies_css_wide_keywords_of_logical_properties_to_their_physical_sides() {
        let (document, styled) = styles(
            "<div id=p dir=rtl><div id=a dir=ltr></div></div>\
             <div id=q><div id=b></div><div id=c></div></div><div id=r><div id=d dir=rtl></div></div>",
            "#p { margin: 0 30px 0 10px; padding: 1px 2px 3px 4px; border: solid green; border-width: 1px 6px 3px 2px }
             #a { margin-inline-start: inherit; padding-inline: inherit; border-inline-start: inherit }
             #q { width: 200px; height: 50px }
             #b { margin: 5px; margin-inline: initial; padding: 5px; padding-block: unset; border: 5px solid;
                  border-inline-width: initial; inline-size: inherit }
             #c { writing-mode: vertical-rl; inline-size: inherit; block-size: inherit }
             #r { border-radius: 1px 12px 3px 4px }
             #d { border-start-start-radius: inherit; border-end-end-radius: inherit; caption-side: initial }",
        );
        // The physical side the property stands for in the element's direction takes the
        // parent's value of that side (Chrome 157): left in the element, left in the parent.
        let a = by_id(&document, &styled, "a");
        assert_eq!(
            a.margin.left,
            LengthPercentageAuto::Length(LengthPercentage::Px(10.0))
        );
        assert_eq!(
            a.margin.right,
            LengthPercentageAuto::Length(LengthPercentage::ZERO)
        );
        assert_eq!(
            (&a.padding.left, &a.padding.right),
            (&LengthPercentage::Px(4.0), &LengthPercentage::Px(2.0))
        );
        assert_eq!(
            (a.border_width.left, a.border_style.left),
            (2.0, BorderStyle::Solid)
        );
        assert_eq!(a.border_style.right, BorderStyle::None);
        let b = by_id(&document, &styled, "b");
        assert_eq!(
            b.margin.left,
            LengthPercentageAuto::Length(LengthPercentage::ZERO)
        );
        assert_eq!(
            b.margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(5.0))
        );
        assert_eq!(
            (&b.padding.top, &b.padding.left),
            (&LengthPercentage::ZERO, &LengthPercentage::Px(5.0))
        );
        assert_eq!([b.border_width.top, b.border_width.left], [5.0, 3.0]);
        assert_eq!(b.width, values::Size::Length(LengthPercentage::Px(200.0)));
        // In vertical text the inline size is the height.
        let c = by_id(&document, &styled, "c");
        assert_eq!(c.height, values::Size::Length(LengthPercentage::Px(50.0)));
        assert_eq!(c.width, values::Size::Length(LengthPercentage::Px(200.0)));
        // Right to left, the start-start corner is the top right one.
        let d = by_id(&document, &styled, "d");
        assert_eq!(d.border_radius.top_right.0, LengthPercentage::Px(12.0));
        assert_eq!(d.border_radius.bottom_left.0, LengthPercentage::Px(4.0));
        assert_eq!(d.border_radius.top_left.0, LengthPercentage::ZERO);
    }

    #[test]
    fn turns_off_the_native_look_of_text_fields_with_shadows() {
        let (document, styled) = styles(
            "<input id=a><textarea id=b></textarea><select id=c></select><button id=d></button>\
             <input id=e type=checkbox><input id=f style='box-shadow: none'><input id=g type=submit>",
            "* { box-shadow: 0 0 0 2px blue } #f { box-shadow: none }",
        );
        for (id, framed) in [
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", false),
            ("e", false),
            ("f", false),
            ("g", false),
        ] {
            assert_eq!(by_id(&document, &styled, id).author_frame, framed, "{id}");
        }
    }

    #[test]
    fn styles_the_parts_of_meters() {
        let (document, styled) = styles(
            "<meter id=a value=0.6></meter><meter id=b low=0.3 high=0.7 optimum=0.9 value=0.5></meter>\
             <meter id=c low=0.3 high=0.7 optimum=0.9 value=0.1 style='appearance: none'></meter>\
             <meter id=d value=0.5></meter><progress id=e value=0.8></progress>",
            "#d::-webkit-meter-bar { background: rgb(0, 0, 255) }
             #d::-webkit-meter-optimum-value { width: 20% !important }
             #e::-webkit-progress-value { width: 30% !important }",
        );
        let pseudo =
            |id: &str, key: PseudoKey| styled.pseudo(node(&document, id), key).cloned().expect("part");
        // With its native look, a meter has a flat light gray track and a value in the color of
        // its region; the value is as wide as the fraction of the meter's.
        let bar = pseudo("a", PseudoKey::MeterBar);
        assert_eq!(bar.background_color, Rgba::from_u8(0xef, 0xef, 0xef, 1.0));
        assert_eq!(bar.border_width.top, 1.0);
        let value = pseudo("a", PseudoKey::MeterValue);
        assert_eq!(value.background_color, Rgba::from_u8(0x10, 0x7c, 0x10, 1.0));
        assert_eq!(value.width, values::Size::Length(LengthPercentage::Percent(0.6)));
        let value = pseudo("b", PseudoKey::MeterValue);
        assert_eq!(value.background_color, Rgba::from_u8(0xff, 0xb9, 0x00, 1.0));
        // Without it, the parts have gradients and no border.
        let bar = pseudo("c", PseudoKey::MeterBar);
        assert!(bar.background_color.is_transparent() && bar.background_image.len() == 1);
        assert_eq!(bar.border_width.top, 0.0);
        let value = pseudo("c", PseudoKey::MeterValue);
        assert!(value.background_color.is_transparent() && value.background_image.len() == 1);
        // Author styles come after the native ones; `!important` wins over the width the
        // element sets in the value's `style` attribute.
        let bar = pseudo("d", PseudoKey::MeterBar);
        assert_eq!(bar.background_color, Rgba::from_u8(0, 0, 255, 1.0));
        let value = pseudo("d", PseudoKey::MeterValue);
        assert_eq!(value.width, values::Size::Length(LengthPercentage::Percent(0.2)));
        let value = pseudo("e", PseudoKey::ProgressValue);
        assert_eq!(value.width, values::Size::Length(LengthPercentage::Percent(0.3)));
    }

    #[test]
    fn multiplies_lengths_from_the_numbers_as_written() {
        // `.64rem` of a 20px root is Blink's 12.8px, whose `.9375em` is 12px exactly; in single
        // precision it was 12.799999px and 11.999999px, which layout truncated to 11.984375px.
        let (document, styled) = styles(
            r#"<!doctype html><div id="a"></div><div id="b"></div>"#,
            "html { font-size: 125% } #a { font-size: .64rem; padding-top: .9375em }
             #b { width: 25.4mm; padding-top: 10.5pt; margin-top: 2.54cm }",
        );
        let a = by_id(&document, &styled, "a");
        assert_eq!(a.font_size, 12.8);
        assert_eq!(a.padding.top.resolve(0.0), 12.0);
        let b = by_id(&document, &styled, "b");
        assert_eq!(b.width, values::Size::Length(LengthPercentage::Px(96.0)));
        assert_eq!(b.padding.top.resolve(0.0), 14.0);
        assert_eq!(
            b.margin.top,
            LengthPercentageAuto::Length(LengthPercentage::Px(96.0))
        );
        assert_eq!(compute::decimal(0.64), 0.64);
        assert_eq!(compute::decimal(33.333_332), 33.333_332);
        assert_eq!(compute::decimal(1.0e-7), 1.0e-7);
        assert_eq!(compute::decimal(-2_500.0), -2_500.0);
    }
}
