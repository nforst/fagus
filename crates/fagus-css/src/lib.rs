//! CSS parsing, selector matching and rule indexing for Fagus.
//!
//! Stylesheets are parsed with lightningcss ([`Stylesheet`]). A [`RuleSet`] flattens the rules of
//! several stylesheets for one document: it evaluates `@media` and `@supports`, resolves layers
//! and nesting, and indexes the rules by the rightmost part of their selectors. Selector matching
//! is implemented here ([`matching`]) because the matcher of `parcel_selectors` does not support
//! `:has()` and `&`.

#![forbid(unsafe_code)]

pub mod background_clip;
mod computed;
mod diagnostics;
mod flow_position;
pub mod gradient;
mod guard;
pub mod hanging;
pub mod matching;
mod math_nesting;
mod media;
mod negative_basis;
mod page_selector;
pub mod page_size;
pub mod paged;
mod pattern;
pub mod properties;
mod quirks;
mod repair;
mod ruleset;
pub mod srcset;
mod stylesheet;
mod supports;
mod validity;

pub use computed::{
    COLOR_FUNCTIONS, MATH_FUNCTIONS, clip_path_function, divides_by_type, has_anchor_functions,
    has_computed_values, is_anchored, is_calc_size, is_computed_function, is_newer_clip_shape, is_typed_attr,
    is_unparsed_color, matches_syntax,
};
pub use diagnostics::{Diagnostic, Severity};
pub use flow_position::{FlowPosition, flow_position};
pub use guard::{guarded, parse_declarations};
pub use lightningcss;
pub use math_nesting::{MAX_COLOR_NESTING, MAX_MATH_NESTING, flat_math};
pub use media::{ContainerSize, MediaEnvironment, asks_size, container_matches};
pub use page_selector::{PageNth, PageSelector, encode_nth_pages};
pub use quirks::author_css;
pub use repair::{has_forbidden_negative, is_dense, repair_block, repair_property};
pub use ruleset::{
    CascadeBand, ContainerQuery, MatchedRule, PageRuleData, Precedence, Rule, RuleSet, Scope, supports,
};
pub use stylesheet::{Origin, Stylesheet, flex_wrap_balance, style_attribute_diagnostics};
pub use validity::checked;

/// How deep blocks, functions and brackets nest in CSS at most. Parsers walk them recursively,
/// so CSS that nests deeper (which no real stylesheet does) is not parsed at all.
pub const MAX_NESTING: usize = 256;

/// Whether blocks, functions or brackets in `css` nest deeper than [`MAX_NESTING`]. Strings,
/// comments and escaped characters do not count.
pub fn nests_too_deep(css: &str) -> bool {
    let bytes = css.as_bytes();
    let mut depth = 0_usize;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => index += 1,
            b'"' | b'\'' => {
                index += 1;
                while let Some(&inner) = bytes.get(index) {
                    match inner {
                        b'\\' => index += 1,
                        b'\n' => break,
                        _ if inner == byte => break,
                        _ => {}
                    }
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = css[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 1);
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING {
                    return true;
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    false
}

/// A parsed selector. All selectors in Fagus are owned (`'static`).
/// Resolves `url` against `base`, the address of the stylesheet or document it is in (RFC 3986,
/// §5.2): absolute URLs, `data:` URLs, URLs from the root and fragments stay as they are; others
/// replace the last segment of the base, `.` and `..` segments resolved.
pub fn join_url(base: &str, url: &str) -> String {
    let url = url.trim();
    let scheme = url.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
            && scheme.as_bytes()[0].is_ascii_alphabetic()
    });
    if url.is_empty() || scheme || url.starts_with('/') || url.starts_with('#') {
        return url.to_owned();
    }
    // The base without its query and fragment, up to its last slash.
    let base = base.split(['?', '#']).next().unwrap_or(base);
    let (prefix, path) = match base.find("://") {
        Some(at) => {
            let rest = &base[at + 3..];
            let host = rest.find('/').unwrap_or(rest.len());
            base.split_at(at + 3 + host)
        }
        None => ("", base),
    };
    let directory = path.rfind('/').map_or("", |slash| &path[..=slash]);
    let mut segments: Vec<&str> = Vec::new();
    let joined = format!("{directory}{url}");
    let (joined, tail) = match joined.find(['?', '#']) {
        Some(at) => (joined[..at].to_owned(), joined[at..].to_owned()),
        None => (joined, String::new()),
    };
    let absolute = joined.starts_with('/');
    let parts: Vec<&str> = joined.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        match *part {
            "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") && !segments.is_empty() {
                    segments.pop();
                } else if !absolute && prefix.is_empty() {
                    segments.push("..");
                }
            }
            "" if index > 0 && index + 1 < parts.len() => {}
            part => segments.push(part),
        }
    }
    let mut path = segments.join("/");
    if parts.last().is_some_and(|last| *last == "." || *last == "..") {
        path.push('/');
    }
    if absolute && !path.starts_with('/') {
        path.insert(0, '/');
    }
    format!("{prefix}{path}{tail}")
}

pub type Selector = lightningcss::selector::Selector<'static>;
/// A parsed selector list.
pub type SelectorList = lightningcss::selector::SelectorList<'static>;
/// A part of a parsed selector.
pub type Component = lightningcss::selector::Component<'static>;
/// A declaration of a property with its parsed value.
pub type Property = lightningcss::properties::Property<'static>;
/// A block of declarations.
pub type DeclarationBlock = lightningcss::declaration::DeclarationBlock<'static>;
