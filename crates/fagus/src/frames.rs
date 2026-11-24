//! Documents in frames: what an `<iframe>` (its `srcdoc`, else its `src`), an `<object>` (its
//! `data`) or an `<embed>` (its `src`) shows when that is an HTML document, a text file or an
//! image, as Chrome prints them (Chrome 157, measured).
//!
//! Each is a document of its own, laid out on a viewport as large as the frame's content box (see
//! [`fagus_layout::layout_viewport`]) and drawn clipped to it. Its style sheets, fonts
//! (`@font-face`) and images are its own; the document around it does not reach into it.
//!
//! A frame's document loads only as the resources of the document around it do, through the
//! same rules: `data:` URLs, the resources handed over, the caller's loader, files inside the
//! root directory; never the network. Its relative URLs resolve against its own URL (a
//! `srcdoc` document's against that of the document around it); a document from a `data:` URL
//! loads only `data:` URLs. Scripts do not run. Frames nest at most [`MAX_DEPTH`] deep, and a
//! render lays out at most [`MAX_FRAMES`] documents in frames with [`MAX_BYTES`] of markup
//! together, so that frames that include themselves end; the frames beyond show nothing.

use std::borrow::Cow;
use std::cell::Cell;
use std::sync::Arc;

use fagus_css::{MediaEnvironment, RuleSet, Stylesheet};
use fagus_layout::{FrameDocument, Images};
use fagus_text::FontDb;
use rustc_hash::FxHashMap;

use crate::{Diagnostic, Document, Inner, container_sizes, warning};

/// How deep frames nest: the documents of frames this deep show no frames of their own.
pub(crate) const MAX_DEPTH: u8 = 8;

/// The most documents in frames that one render lays out, at all depths together.
const MAX_FRAMES: usize = 256;

/// The most markup of documents in frames that one render lays out, in bytes.
const MAX_BYTES: usize = 8 << 20;

/// The largest side of a frame whose document is laid out, in pixels.
const MAX_SIDE: f32 = 100_000.0;

/// Where a document lies in frames.
#[derive(Clone, Debug, Default)]
pub(crate) struct Nesting {
    /// How deep the document lies in frames: 0 for the document rendered.
    pub depth: u8,
    /// The directory of the document's URL, relative to that of the document rendered (`sub/`),
    /// which its relative URLs resolve against; empty for that document.
    pub directory: String,
    /// Whether the document comes from a `data:` URL, which gives it an opaque origin: it loads
    /// nothing but `data:` URLs.
    pub opaque: bool,
}

/// What the documents in frames of one render have taken so far.
#[derive(Debug, Default)]
pub(crate) struct Budget {
    frames: Cell<usize>,
    bytes: Cell<usize>,
    /// Whether a frame was left empty because the budget was spent.
    spent: Cell<bool>,
}

impl Budget {
    /// Takes one document of `bytes` of markup from the budget, if it has room for it.
    fn take(&self, bytes: usize) -> bool {
        let (frames, total) = (self.frames.get() + 1, self.bytes.get().saturating_add(bytes));
        if frames > MAX_FRAMES || total > MAX_BYTES {
            self.spent.set(true);
            return false;
        }
        self.frames.set(frames);
        self.bytes.set(total);
        true
    }
}

/// What a frame's document is, as its type says (a MIME type, from the `type` attribute or a
/// `data:` URL) or else the extension of its URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Html,
    Xhtml,
    /// Plain text, which Chrome shows as a document of its own, preformatted.
    Text,
    /// An image, which Chrome shows as a document holding it at its corner.
    Image,
}

impl Kind {
    /// The kind of a MIME type (its parameters left out), if Fagus shows such documents.
    fn of_type(mime: &str) -> Option<Self> {
        let mime = mime
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match mime.as_str() {
            "text/html" => Some(Self::Html),
            "application/xhtml+xml" => Some(Self::Xhtml),
            "text/plain" | "" => Some(Self::Text),
            _ if mime.starts_with("image/") => Some(Self::Image),
            _ => None,
        }
    }

    /// The kind of the file a URL names, by its extension; `None` for other or no extensions.
    fn of_url(url: &str) -> Option<Self> {
        let path = url.split(['?', '#']).next().unwrap_or_default();
        let name = path.rsplit('/').next().unwrap_or_default();
        let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
        match extension.as_str() {
            "html" | "htm" | "shtml" => Some(Self::Html),
            "xhtml" | "xht" => Some(Self::Xhtml),
            "txt" | "text" => Some(Self::Text),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "bmp" | "ico" => Some(Self::Image),
            _ => None,
        }
    }
}

/// The document a frame shows, ready to be laid out.
pub(crate) struct Source {
    markup: String,
    xhtml: bool,
    nesting: Nesting,
}

/// Whether `data` looks like markup (it starts with `<`, after white space and a byte order
/// mark), as a document without a type is taken to be HTML.
pub(crate) fn is_markup(data: &[u8]) -> bool {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    data.iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|&byte| byte == b'<')
}

/// Whether a URL has a scheme (`data:`, `https:`).
fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut characters = scheme.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
}

/// `url` resolved against `directory` (see [`Nesting::directory`]): relative to the document
/// rendered, as its own URLs are written. URLs with a scheme or a host, paths from the root and
/// references to fragments stay as they are; `.` and `..` are taken out, those that lead above
/// the directory of the document rendered kept (reading files refuses what leads outside the
/// root).
pub(crate) fn resolve<'a>(directory: &str, url: &'a str) -> Cow<'a, str> {
    if directory.is_empty() || url.is_empty() || url.starts_with(['#', '/', '\\']) || has_scheme(url) {
        return Cow::Borrowed(url);
    }
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let (path, rest) = url.split_at(end);
    let joined = format!("{directory}{path}");
    let mut segments: Vec<&str> = Vec::new();
    let mut parts = joined.split('/').peekable();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        match part {
            "." => {
                if last {
                    segments.push("");
                }
            }
            ".." => {
                if segments.last().is_some_and(|&segment| segment != "..") {
                    segments.pop();
                } else {
                    segments.push("..");
                }
                if last {
                    segments.push("");
                }
            }
            "" if !last => {}
            part => segments.push(part),
        }
    }
    Cow::Owned(format!("{}{rest}", segments.join("/")))
}

/// The directory of a URL relative to the document rendered: all of its path up to the last
/// `/`, with that.
fn directory_of(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    path.rfind('/')
        .map_or_else(String::new, |end| path[..=end].to_owned())
}

/// `text` with the characters that are markup in HTML escaped.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            character => out.push(character),
        }
    }
    out
}

/// The document Chrome shows for a resource of kind `kind`: an HTML or XHTML document as it is,
/// plain text preformatted, wrapped where its lines are too long (Blink's `TextDocument`), an
/// image at the corner of a document without margins (Blink's `ImageDocument` in a frame,
/// which neither centers nor shrinks it), at `url`.
fn markup(kind: Kind, data: &[u8], url: &str) -> (String, bool) {
    match kind {
        Kind::Html | Kind::Xhtml => (String::from_utf8_lossy(data).into_owned(), kind == Kind::Xhtml),
        Kind::Text => (
            format!(
                "<html><head><meta name=\"color-scheme\" content=\"light dark\"></head><body>\
                 <pre style=\"word-wrap: break-word; white-space: pre-wrap;\">{}</pre></body></html>",
                escaped(&String::from_utf8_lossy(data))
            ),
            false,
        ),
        Kind::Image => (
            format!(
                "<!DOCTYPE html><html><body style=\"margin: 0px;\">\
                 <img style=\"display: block;\" src=\"{}\"></body></html>",
                escaped(url)
            ),
            false,
        ),
    }
}

/// The URL whose document a frame shows (`src`, or `data` of an `<object>`), if it names one:
/// not `about:blank` or a script.
fn frame_url(element: &fagus_dom::ElementData) -> Option<&str> {
    let url = if element.is("object") {
        element.attribute("data")
    } else {
        element.attribute("src")
    }?
    .trim();
    (!url.is_empty() && !starts_with(url, "about:") && !starts_with(url, "javascript:")).then_some(url)
}

/// Whether `text` starts with `prefix`, in any case.
pub(crate) fn starts_with(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

impl Inner {
    /// The document the frame `element` of the document `input` shows, if it shows one that
    /// loads: the `srcdoc` of an `<iframe>`, else the resource its URL names, of the kind its
    /// `type` (of an `<object>` or `<embed>`), its `data:` URL or its extension says, else HTML
    /// if it looks like markup.
    pub(crate) fn frame_source(&self, input: &Document, element: &fagus_dom::ElementData) -> Option<Source> {
        let depth = input.nesting.depth.saturating_add(1);
        if element.is("iframe")
            && let Some(srcdoc) = element.attribute("srcdoc")
        {
            // An `srcdoc` document is never in quirks mode (HTML, §13.2.6.4.1).
            return Some(Source {
                markup: format!("<!DOCTYPE html>{srcdoc}"),
                xhtml: false,
                nesting: Nesting {
                    depth,
                    ..input.nesting.clone()
                },
            });
        }
        let url = frame_url(element)?;
        let declared = element
            .attribute("type")
            .filter(|_| !element.is("iframe"))
            .filter(|kind| !kind.trim().is_empty());
        let data = self.load(input, url)?;
        let data_url = starts_with(url, "data:");
        let kind = if data_url {
            // (Its type is what comes before the parameters, `base64` among them.)
            Kind::of_type(url[5..].split_once(',')?.0)?
        } else if let Some(declared) = declared {
            Kind::of_type(declared)?
        } else {
            match Kind::of_url(url) {
                Some(kind) => kind,
                None if is_markup(&data) => Kind::Html,
                None => return None,
            }
        };
        let opaque = input.nesting.opaque || data_url;
        let (markup, xhtml) = markup(kind, &data, url);
        // An image document shows the image at the URL the frame names, from where the frame's
        // own document is; other documents resolve their URLs against theirs.
        let directory = if kind == Kind::Image || opaque {
            input.nesting.directory.clone()
        } else {
            directory_of(&resolve(&input.nesting.directory, url))
        };
        Some(Source {
            markup,
            xhtml,
            nesting: Nesting {
                depth,
                directory,
                opaque,
            },
        })
    }

    /// Makes the frames of `document` (of `input`) that show a document frames of `images`
    /// (see [`Images::insert_frame`]): an `<iframe>` with a `srcdoc` or a URL, an `<object>` or
    /// `<embed>` whose resource is a document that loads (one whose image loads shows that, one
    /// without a document its fallback content). Frames as deep as [`MAX_DEPTH`] and those of SVG
    /// images show none, nor frames past the first [`MAX_FRAMES`] of a document, which the
    /// budget would leave empty.
    pub(crate) fn register_frames(
        &self,
        input: &Document,
        document: &fagus_dom::Document,
        images: &mut Images,
    ) {
        if input.nesting.depth >= MAX_DEPTH || input.svg_depth > 0 {
            return;
        }
        let mut count = 0;
        for node in document.descendants(fagus_dom::NodeId::DOCUMENT) {
            // (One more than the budget holds, which then tells that frames are left empty.)
            if count > MAX_FRAMES {
                break;
            }
            let Some(element) = document.element(node) else {
                continue;
            };
            let shows = if element.is("iframe") {
                element.attribute("srcdoc").is_some() || frame_url(element).is_some()
            } else if element.is("object") || element.is("embed") {
                fagus_layout::embedded_image_url(element).is_none_or(|url| images.get(url).is_none())
                    && self.frame_source(input, element).is_some()
            } else {
                false
            };
            if shows {
                images.insert_frame(node);
                count += 1;
            }
        }
    }

    /// Lays out the documents of the frames of `document` (of `input`) in the boxes `pages`
    /// give the frames, and keeps them in `images` (see [`Images::insert_frame_document`]). The
    /// faces of their `@font-face` rules are added to `fonts`, which draws them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lay_out_frames(
        &self,
        input: &Document,
        document: &fagus_dom::Document,
        pages: &[fagus_layout::Page],
        images: &mut Images,
        fonts: &mut Cow<'_, FontDb>,
        budget: &Budget,
    ) {
        fn walk(
            fragment: &fagus_layout::Fragment,
            images: &Images,
            sizes: &mut FxHashMap<usize, (f32, f32)>,
        ) {
            if let fagus_layout::FragmentKind::Image(image) = &fragment.kind
                && images.is_frame(&image.data)
            {
                sizes
                    .entry(Arc::as_ptr(&image.data) as usize)
                    .or_insert((fragment.rect.width.to_px(), fragment.rect.height.to_px()));
            }
            for child in &fragment.children {
                walk(child, images, sizes);
            }
        }
        let mut sizes = FxHashMap::default();
        for fragment in pages.iter().flat_map(|page| &page.fragments) {
            walk(fragment, images, &mut sizes);
        }
        let mut frames: Vec<_> = images
            .frames()
            .map(|(node, image)| (node, Arc::clone(&image.data)))
            .collect();
        frames.sort_unstable_by_key(|&(node, _)| node);
        for (node, data) in frames {
            // The viewport is the content box snapped to whole pixels.
            let Some(&(width, height)) = sizes.get(&(Arc::as_ptr(&data) as usize)) else {
                continue;
            };
            let (width, height) = (width.round(), height.round());
            if !(width > 0.0 && height > 0.0 && width <= MAX_SIDE && height <= MAX_SIDE) {
                continue;
            }
            let Some(source) = document
                .element(node)
                .and_then(|element| self.frame_source(input, element))
            else {
                continue;
            };
            if !budget.take(source.markup.len()) {
                continue;
            }
            let frame = self.frame_document(input, source, (width, height), fonts, budget);
            images.insert_frame_document(&data, Arc::new(frame));
        }
    }

    /// Lays out the document `source` of a frame of the document `parent` on a viewport `size`
    /// large (see [`fagus_layout::layout_viewport`]), with its own frames.
    fn frame_document(
        &self,
        parent: &Document,
        source: Source,
        (width, height): (f32, f32),
        fonts: &mut Cow<'_, FontDb>,
        budget: &Budget,
    ) -> FrameDocument {
        let input = Document {
            xhtml: source.xhtml,
            base: parent.base.clone(),
            root: parent.root.clone(),
            resources: parent.resources.clone(),
            loader: parent.loader.clone(),
            nesting: source.nesting,
            loaded: Arc::clone(&parent.loaded),
            ..Document::html(source.markup)
        };
        let document = if input.xhtml {
            fagus_dom::Document::parse_xhtml(&input.html)
        } else {
            fagus_dom::Document::parse_html(&input.html)
        };
        let mut quiet: Vec<Diagnostic> = Vec::new();
        let sheets = self.document_sheets(&input, &document, &mut quiet);
        let references: Vec<&Stylesheet> = sheets.iter().map(AsRef::as_ref).collect();
        let rules = RuleSet::new(&references, &MediaEnvironment { width, height });
        // The faces of the documents around it are not its own: they stay, unloaded, so that
        // the ids of those added after them are the same here and in `fonts`.
        let hidden = fonts.with_unloaded(|_| false);
        let base: &FontDb = hidden.as_ref().unwrap_or(&**fonts);
        let known = base.face_count();
        let mut own = self.fonts_with_faces(base, &input, &rules, &mut quiet);
        let viewport = (width, height);
        let style = |sizes: &fagus_style::ContainerSizes, own: &FontDb| {
            fagus_style::compute_styles_with(
                &document,
                &rules,
                viewport,
                sizes,
                &fagus_layout::StyleFonts(own),
            )
        };
        let mut styled = style(&fagus_style::ContainerSizes::default(), &own);
        let mut images = self.document_images(&input, (&document, &rules), &styled, &own, &mut quiet);
        let lay_out = |styled: &fagus_style::StyledDocument, images: &Images, own: &FontDb| {
            fagus_layout::layout_viewport(
                &document,
                styled,
                &rules,
                own,
                images,
                self.locale.as_deref(),
                viewport,
            )
        };
        let mut page = lay_out(&styled, &images, &own);
        // `@container` rules apply as the sizes of their containers in that layout say.
        if rules.has_container_queries()
            || styled
                .all()
                .any(|style| style.container_type != fagus_style::values::ContainerType::Normal)
        {
            styled = style(&container_sizes(std::slice::from_ref(&page)), &own);
            images = self.document_images(&input, (&document, &rules), &styled, &own, &mut quiet);
            page = lay_out(&styled, &images, &own);
        }
        drop(styled);
        if images.frames().next().is_some() {
            self.lay_out_frames(
                &input,
                &document,
                std::slice::from_ref(&page),
                &mut images,
                &mut own,
                budget,
            );
        }
        let added: Vec<Arc<fagus_text::Face>> = own.faces_after(known).to_vec();
        drop(own);
        drop(hidden);
        if !added.is_empty() {
            let fonts = fonts.to_mut();
            for face in added {
                fonts.add_document_face(face);
            }
        }
        // Kept with the document around it until it is painted.
        fagus_dom::budget::charge(
            fagus_dom::budget::Kind::Layout,
            fagus_layout::held_by(std::slice::from_ref(&page)),
        );
        let responsive_height =
            fagus_layout::is_responsive(&document).then(|| fagus_layout::content_height(&page));
        FrameDocument {
            page,
            images,
            responsive_height,
        }
    }
}

/// Gives the `<iframe>` elements of `document` with `frame-sizing: content-height` whose
/// documents let them (see [`FrameDocument::responsive_height`]) the heights of those documents
/// as their natural heights; whether any changed, which the document is laid out again for.
pub(crate) fn fit_responsive_frames(
    document: &fagus_dom::Document,
    styled: &fagus_style::StyledDocument,
    images: &mut Images,
) -> bool {
    let mut heights: Vec<(fagus_dom::NodeId, f32)> = images
        .frames()
        .filter(|&(node, _)| {
            document.element(node).is_some_and(|data| data.is("iframe"))
                && styled
                    .style(node)
                    .is_some_and(|style| style.frame_sizing_content_height)
        })
        .filter_map(|(node, image)| {
            let height = images.frame_document(&image.data)?.responsive_height?;
            ((height - image.height).abs() > 1e-3).then_some((node, height))
        })
        .collect();
    heights.sort_unstable_by_key(|&(node, _)| node);
    for &(node, height) in &heights {
        images.resize_frame(node, height.min(MAX_SIDE));
    }
    !heights.is_empty()
}

/// The warning for frames left empty because the documents of frames took the budget.
pub(crate) fn spent_warning(budget: &Budget) -> Option<Diagnostic> {
    budget.spent.get().then(|| {
        warning(
            "frame.limit",
            format!(
                "the documents in frames reach the limit of {MAX_FRAMES} documents or {} MiB; \
                 the frames after show nothing",
                MAX_BYTES >> 20
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{Engine, ResourceLoader};

    /// A loader of the resources it holds by URL, which records the URLs asked for.
    #[derive(Default)]
    struct Files {
        files: Vec<(&'static str, &'static str)>,
        asked: Mutex<Vec<String>>,
    }

    impl ResourceLoader for Files {
        fn load(&self, url: &str) -> Option<Vec<u8>> {
            self.asked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(url.to_owned());
            self.files
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, data)| data.as_bytes().to_vec())
        }
    }

    /// The decompressed streams of the PDF of `document`, and its diagnostics' codes.
    fn render(document: Document) -> (String, Vec<&'static str>) {
        let mut engine = Engine::new();
        engine
            .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
            .expect("fonts");
        let output = engine.render(&document.tagged(false)).expect("rendered");
        let mut streams = Vec::new();
        let mut rest = &output.pdf[..];
        while let Some(start) = rest.windows(7).position(|window| window == b"stream\n") {
            rest = &rest[start + 7..];
            let end = rest
                .windows(9)
                .position(|window| window == b"endstream")
                .unwrap_or(rest.len());
            let _ =
                std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&rest[..end]), &mut streams);
            rest = &rest[(end + 9).min(rest.len())..];
        }
        let codes = output
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        (String::from_utf8_lossy(&streams).into_owned(), codes)
    }

    const BLUE: &str = "0 0 1 rg";
    const RED: &str = "1 0 0 rg";
    const GREEN: &str = "0 1 0 rg";

    #[test]
    fn draws_the_documents_of_frames_with_their_own_style() {
        // The rules of the document around do not reach into the frame, whose media queries see
        // the frame's content box.
        let (content, _) = render(Document::html(
            "<style>div { background: rgb(255 0 0) !important } body { margin: 0 }</style>\
             <iframe style='width: 100px; height: 50px; border: 0' srcdoc=\"<style>\
             p { height: 10px; margin: 0; background: rgb(0 0 255) } \
             @media (width: 100px) and (height: 50px) { p + p { background: rgb(0 255 0) } }\
             </style><p></p><p></p>\"></iframe>",
        ));
        assert!(content.contains(BLUE) && content.contains(GREEN), "{content}");
        assert!(!content.contains(RED), "{content}");
        // Clipped to the content box: 100 × 50 pixels, at 0.75 points a pixel, from the corner
        // of the page area.
        assert!(content.contains("102.74999 776.67 l"), "{content}");
    }

    #[test]
    fn resolves_the_urls_of_a_frame_against_its_own() {
        let files = Arc::new(Files {
            files: vec![
                ("sub/frame.html", "<link rel=stylesheet href='s.css'><p>x</p>"),
                ("sub/s.css", "p { background: rgb(0 0 255) }"),
            ],
            ..Files::default()
        });
        let (content, _) = render(
            Document::html("<iframe src='sub/frame.html'></iframe><object data='sub/frame.html'></object>")
                .loader(Arc::clone(&files) as Arc<dyn ResourceLoader>),
        );
        assert!(content.contains(BLUE), "{content}");
        let asked = files.asked.lock().expect("asked").clone();
        assert!(asked.contains(&"sub/s.css".to_owned()), "{asked:?}");
        assert!(!asked.contains(&"s.css".to_owned()), "{asked:?}");
    }

    #[test]
    fn loads_nothing_into_documents_of_data_urls() {
        let files = Arc::new(Files::default());
        let (content, _) = render(
            Document::html(
                "<iframe src=\"data:text/html,<link rel=stylesheet href=s.css>\
                 <p style='height: 9px; background: rgb(0 0 255)'></p>\"></iframe>",
            )
            .loader(Arc::clone(&files) as Arc<dyn ResourceLoader>),
        );
        assert!(content.contains(BLUE), "{content}");
        assert!(files.asked.lock().expect("asked").is_empty());
    }

    #[test]
    fn shows_the_fallback_of_an_object_whose_document_does_not_load() {
        let fallback = "<object data='missing.html'><p style='width: 9px; height: 9px; background: rgb(0 0 255)'></p></object>";
        assert!(render(Document::html(fallback)).0.contains(BLUE));
        // With the document, not the fallback.
        let files = Arc::new(Files {
            files: vec![(
                "doc.html",
                "<p style='height: 9px; background: rgb(0 255 0)'></p>",
            )],
            ..Files::default()
        });
        let shown = fallback.replace("missing", "doc");
        let (content, _) = render(Document::html(shown).loader(files));
        assert!(content.contains(GREEN) && !content.contains(BLUE), "{content}");
    }

    #[test]
    fn reads_no_frame_outside_the_root() {
        let corpus = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus");
        let frame = |src: &str| {
            render(
                Document::html(format!("<iframe src='{src}'></iframe>"))
                    .base_directory(corpus)
                    .root(corpus),
            )
            .0
        };
        // A text file inside the root shows as text; one outside it, or on the network, not.
        let text = |content: &str| content.contains(" Tf");
        assert!(text(&frame("../corpus/backgrounds.html")));
        assert!(!text(&frame("../fonts/Inter-LICENSE.txt")));
        assert!(!text(&frame("https://example.com/")));
    }

    #[test]
    fn ends_frames_that_include_themselves() {
        let files = Arc::new(Files {
            files: vec![(
                "self.html",
                "<iframe src='self.html'></iframe><object data='self.html'></object><embed src='self.html'>",
            )],
            ..Files::default()
        });
        let (_, codes) = render(Document::html("<iframe src='self.html'></iframe>").loader(files));
        assert!(codes.contains(&"frame.limit"), "{codes:?}");
    }

    #[test]
    fn resolves_urls_against_the_directory_of_the_frame() {
        assert_eq!(resolve("", "a.png"), "a.png");
        assert_eq!(resolve("sub/", "a.png"), "sub/a.png");
        assert_eq!(resolve("sub/", "../fonts/a.ttf?v=1#x"), "fonts/a.ttf?v=1#x");
        assert_eq!(resolve("sub/deep/", "./../b/./c.css"), "sub/b/c.css");
        assert_eq!(resolve("sub/", "../../outside.png"), "../outside.png");
        assert_eq!(resolve("../up/", "a.png"), "../up/a.png");
        assert_eq!(resolve("sub/", "/from/root.png"), "/from/root.png");
        assert_eq!(
            resolve("sub/", "data:image/png;base64,AA"),
            "data:image/png;base64,AA"
        );
        assert_eq!(
            resolve("sub/", "https://example.com/a.png"),
            "https://example.com/a.png"
        );
        assert_eq!(resolve("sub/", "#target"), "#target");
        assert_eq!(directory_of("sub/deep/page.html?x=/y"), "sub/deep/");
        assert_eq!(directory_of("page.html"), "");
    }

    #[test]
    fn knows_documents_by_type_and_extension() {
        assert_eq!(Kind::of_type("text/html; charset=utf-8"), Some(Kind::Html));
        assert_eq!(Kind::of_type(""), Some(Kind::Text));
        assert_eq!(Kind::of_type("image/svg+xml"), Some(Kind::Image));
        assert_eq!(Kind::of_type("application/pdf"), None);
        assert_eq!(Kind::of_url("a/b.HTM?x"), Some(Kind::Html));
        assert_eq!(Kind::of_url("b.xht"), Some(Kind::Xhtml));
        assert_eq!(Kind::of_url("b"), None);
        assert!(is_markup(b"\xEF\xBB\xBF  <p>"));
        assert!(!is_markup(b"text"));
        assert!(has_scheme("DATA:,x") && !has_scheme("a/b:c") && !has_scheme("1x:y"));
    }

    #[test]
    fn spends_its_budget_on_documents_and_bytes() {
        let budget = Budget::default();
        assert!(budget.take(MAX_BYTES));
        assert!(!budget.take(1), "no bytes left");
        assert!(spent_warning(&budget).is_some());
        let budget = Budget::default();
        for _ in 0..MAX_FRAMES {
            assert!(budget.take(0));
        }
        assert!(!budget.take(0), "no documents left");
    }
}
