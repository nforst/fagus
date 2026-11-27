//! Math functions nested in math functions, made cheap to parse.
//!
//! lightningcss parses a math function inside another one twice where it does not parse: once
//! as a math function, and once more as the value it stands for, which tries the math function
//! again. The time doubles with each level of nesting, so a value of twenty nested `calc(`
//! that does not parse took seconds, and one of forty would take days.
//!
//! [`flat_math`] writes CSS so that this cannot happen, before it is parsed: a `calc()` directly
//! inside a math function is the same as parentheses (CSS Values 4, §10.1), so it becomes
//! parentheses, which lightningcss parses once. Other math functions (`min()`, `clamp()`, …)
//! cannot be written so; a value with more than [`MAX_MATH_NESTING`] of them nested is made
//! invalid instead, as no real style sheet nests them so deep (Chrome stops at 100 levels).
//! The CSS keeps its length, so that the lines and columns of diagnostics stay right.
//!
//! A `min()`, `max()` or `clamp()` with a unitless zero among dimensions or percentages
//! (`min(0, 100%)`), which lightningcss takes as `0px`, is made invalid in the same way, as
//! browsers reject it (a number is not a length in math functions; CSS Values 4, §10.9).
//!
//! Color functions double the time in the same way: in a value that lightningcss keeps as
//! tokens (a custom property, a value with `var()`, a value that does not parse, the block of an
//! unknown at-rule), it parses `rgb()`, `hsl()` and `light-dark()` once as a color whose parts
//! are tokens, and where that fails once more as any function. A value that does not parse in
//! twenty-two nested `light-dark(` took 13 seconds. Neither parse can be left out, so a color
//! function inside more than [`MAX_COLOR_NESTING`] of them is made invalid, as no real style
//! sheet nests them so deep.

use std::borrow::Cow;

use crate::MATH_FUNCTIONS;

/// How many math functions (other than a `calc()` inside a math function, which counts as
/// parentheses) a value nests at most; deeper values are invalid.
pub const MAX_MATH_NESTING: usize = 8;

/// How many of the color functions that lightningcss parses twice (`rgb()`, `hsl()`,
/// `light-dark()`) a value nests at most; one inside more is invalid.
pub const MAX_COLOR_NESTING: usize = 8;

/// What a block of CSS is, for the math and color functions in it.
#[derive(Clone, Copy, PartialEq)]
enum Block {
    /// A math function whose name starts at that offset; `true` if the name has an escape, as
    /// it may spell a color function too.
    Math(usize, bool),
    /// Parentheses (or a `calc()` written as them) in a math function.
    Group,
    /// A color function that lightningcss parses twice (see [`is_color`]).
    Color,
    /// Anything else: a block, brackets, parentheses outside math functions, another function.
    Other(u8),
}

/// `css` with each `calc()` directly inside a math function written as parentheses (its name
/// replaced by spaces), each math function that holds more than [`MAX_MATH_NESTING`] levels
/// of math functions made invalid (the first letter of its name replaced by `_`), and each
/// color function inside more than [`MAX_COLOR_NESTING`] of them made invalid in the same way.
/// Strings, comments and URLs stay as they are.
pub fn flat_math(css: &str) -> Cow<'_, str> {
    let bytes = css.as_bytes();
    // Most CSS has no math function in another one; it is not copied.
    if !bytes.contains(&b'(') {
        return Cow::Borrowed(css);
    }
    let mut stack: Vec<Block> = Vec::new();
    // The arguments of the open math functions, innermost last: where the current one starts,
    // and whether one was a unitless zero and one a dimension or percentage.
    let mut arguments: Vec<Arguments> = Vec::new();
    let mut spaces: Vec<std::ops::Range<usize>> = Vec::new();
    let mut invalid: Vec<usize> = Vec::new();
    // How many blocks of `stack` are color functions, or math functions that may spell one.
    let mut colors = 0;
    let mut ident: Option<usize> = None;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => {
                ident.get_or_insert(index);
                index = escape_end(bytes, index);
                continue;
            }
            b'"' | b'\'' => {
                ident = None;
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
                ident = None;
                index = css[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 1);
            }
            b'(' => {
                let name = ident.take().map_or("", |start| &css[start..index]);
                let in_math = matches!(stack.last(), Some(Block::Math(..) | Block::Group));
                if name.eq_ignore_ascii_case("url") && !css[index + 1..].trim_start().starts_with(['"', '\''])
                {
                    // An unquoted URL is one token up to its `)`.
                    index = css[index..].find(')').map_or(bytes.len(), |end| index + end);
                } else if name.is_empty() {
                    stack.push(if in_math { Block::Group } else { Block::Other(b')') });
                } else if in_math && is_calc(name) {
                    spaces.push(index - name.len()..index);
                    stack.push(Block::Group);
                } else if colors >= MAX_COLOR_NESTING && (is_color(name) || has_escape(name)) {
                    // An unknown function now, which lightningcss parses once.
                    invalid.push(index - name.len());
                    stack.push(Block::Other(b')'));
                } else if is_math(name) {
                    let escaped = has_escape(name);
                    colors += usize::from(escaped);
                    stack.push(Block::Math(index - name.len(), escaped));
                    arguments.push(Arguments {
                        name: index - name.len(),
                        start: index + 1,
                        zero: false,
                        typed: false,
                        compares: ["min", "max", "clamp"]
                            .iter()
                            .any(|known| name.eq_ignore_ascii_case(known)),
                    });
                    // Each open math function has its arguments, the outermost first.
                    if arguments.len() > MAX_MATH_NESTING {
                        invalid.push(arguments[0].name);
                    }
                } else if is_color(name) {
                    colors += 1;
                    stack.push(Block::Color);
                } else {
                    stack.push(Block::Other(b')'));
                }
            }
            b'[' | b'{' => {
                ident = None;
                stack.push(Block::Other(if byte == b'[' { b']' } else { b'}' }));
            }
            b',' if matches!(stack.last(), Some(Block::Math(..))) => {
                ident = None;
                if let Some(open) = arguments.last_mut() {
                    open.argument(&css[open.start..index]);
                    open.start = index + 1;
                }
            }
            b')' | b']' | b'}' => {
                ident = None;
                // A closing bracket of another kind than the block's is a token in it.
                let closes = match stack.last() {
                    Some(Block::Math(..) | Block::Group | Block::Color) => byte == b')',
                    Some(Block::Other(closer)) => byte == *closer,
                    None => false,
                };
                if closes {
                    match stack.pop() {
                        Some(Block::Math(start, escaped)) => {
                            colors -= usize::from(escaped);
                            if let Some(mut open) = arguments.pop() {
                                open.argument(&css[open.start..index]);
                                if open.compares && open.zero && open.typed {
                                    invalid.push(start);
                                }
                            }
                        }
                        Some(Block::Color) => colors -= 1,
                        _ => {}
                    }
                }
            }
            _ if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte >= 0x80 => {
                ident.get_or_insert(index);
            }
            _ => ident = None,
        }
        index += 1;
    }
    if spaces.is_empty() && invalid.is_empty() {
        return Cow::Borrowed(css);
    }
    let mut out = bytes.to_vec();
    for range in spaces {
        out[range].fill(b' ');
    }
    for start in invalid {
        out[start] = b'_';
    }
    // Only ASCII bytes of function names were replaced, by ASCII.
    String::from_utf8(out).map_or(Cow::Borrowed(css), Cow::Owned)
}

/// The arguments of a math function seen so far.
struct Arguments {
    /// Where the name of the function starts.
    name: usize,
    /// Where the current argument starts.
    start: usize,
    /// Whether an argument is a unitless zero.
    zero: bool,
    /// Whether an argument has a dimension or a percentage.
    typed: bool,
    /// Whether the function is `min()`, `max()` or `clamp()`.
    compares: bool,
}

impl Arguments {
    /// Notes what an argument (its CSS) is.
    fn argument(&mut self, css: &str) {
        let trimmed = css.trim().trim_start_matches(['+', '-']);
        if !trimmed.is_empty()
            && trimmed.bytes().all(|byte| byte == b'0' || byte == b'.')
            && trimmed.contains('0')
            && trimmed.matches('.').count() <= 1
        {
            self.zero = true;
        } else if has_unit(css) {
            self.typed = true;
        }
    }
}

/// Whether CSS has a number with a unit or a percent sign after it (`10px`, `50%`), not a
/// number in a name (`--x2`) or with an exponent (`1e3`).
fn has_unit(css: &str) -> bool {
    let bytes = css.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let starts_number = (bytes[index].is_ascii_digit() || bytes[index] == b'.')
            && index
                .checked_sub(1)
                .is_none_or(|previous| !(bytes[previous].is_ascii_alphanumeric() || bytes[previous] == b'_'));
        if !starts_number {
            index += 1;
            continue;
        }
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            index += 1;
        }
        match bytes.get(index) {
            Some(b'%') => return true,
            Some(b'e' | b'E')
                if bytes
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_digit() || *next == b'+' || *next == b'-') => {}
            Some(next) if next.is_ascii_alphabetic() => return true,
            _ => {}
        }
        index += 1;
    }
    false
}

/// The offset after the escape at `index` (a backslash and a character, or up to six hex digits
/// and a white space).
fn escape_end(bytes: &[u8], index: usize) -> usize {
    let mut end = index + 1;
    let hex = bytes[end.min(bytes.len())..]
        .iter()
        .take(6)
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if hex == 0 {
        return (end + 1).min(bytes.len());
    }
    end += hex;
    if bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
        end += 1;
    }
    end
}

/// Whether a function name is `calc` (or a prefixed `calc`). A name with an escape counts as
/// another math function: it is not rewritten, but counts towards the nesting.
fn is_calc(name: &str) -> bool {
    ["calc", "-webkit-calc", "-moz-calc"]
        .iter()
        .any(|calc| name.eq_ignore_ascii_case(calc))
}

/// Whether a function name is that of a math function, or has an escape (which may spell one).
fn is_math(name: &str) -> bool {
    has_escape(name) || is_calc(name) || MATH_FUNCTIONS.iter().any(|math| name.eq_ignore_ascii_case(math))
}

/// Whether a function name is that of a color function that lightningcss parses twice where
/// it does not parse: as a color whose parts are tokens, then as any function.
fn is_color(name: &str) -> bool {
    ["rgb", "hsl", "light-dark"]
        .iter()
        .any(|color| name.eq_ignore_ascii_case(color))
}

/// Whether a function name has an escape, and so may spell any function. A name that starts
/// with a character outside ASCII spells none: that character stays.
fn has_escape(name: &str) -> bool {
    name.contains('\\') && name.as_bytes().first().is_some_and(u8::is_ascii)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_calc_in_math_functions_as_parentheses() {
        assert_eq!(flat_math("width: calc(1px + 2px)"), "width: calc(1px + 2px)");
        assert_eq!(
            flat_math("width: calc(2 * calc(1px + CALC(2px)))"),
            "width: calc(2 *     (1px +     (2px)))"
        );
        assert_eq!(
            flat_math("width: min(calc(1px), max(-webkit-calc(2px)))"),
            "width: min(    (1px), max(            (2px)))"
        );
        // Not directly in a math function: the fallback of `var()`, a URL, a string.
        let kept = "a: calc(var(--x, calc(1px))) url(calc(calc(x))) 'calc(calc(1px))' x calc(1px)";
        assert_eq!(flat_math(kept), kept);
    }

    #[test]
    fn makes_comparisons_of_unitless_zeros_and_lengths_invalid() {
        assert_eq!(flat_math("height: min(0, 100%)"), "height: _in(0, 100%)");
        assert_eq!(flat_math("a: max(10px, 0)"), "a: _ax(10px, 0)");
        assert_eq!(
            flat_math("a: clamp(0, 1vw + 2px, 3rem)"),
            "a: _lamp(0, 1vw + 2px, 3rem)"
        );
        // Numbers alone, zeros with units, exponents and names stay.
        for kept in [
            "a: max(0, 1.5)",
            "a: min(0px, 100%)",
            "a: max(0, 1e3)",
            "a: max(0, var(--x2))",
        ] {
            assert_eq!(flat_math(kept), kept);
        }
        // The inner function alone.
        assert_eq!(flat_math("a: calc(2 * max(0, 5px))"), "a: calc(2 * _ax(0, 5px))");
    }

    #[test]
    fn makes_values_of_too_many_nested_math_functions_invalid() {
        let fine = format!("width: {}1px{}", "min(".repeat(8), ")".repeat(8));
        assert_eq!(flat_math(&fine), fine);
        let deep = format!("a {{ width: {}1px{} }}", "min(".repeat(9), ")".repeat(9));
        assert!(flat_math(&deep).starts_with("a { width: _in(min("));
        // Escaped names may spell math functions.
        let escaped = format!("width: {}", "c\\61 lc(min(".repeat(5));
        assert!(flat_math(&escaped).starts_with("width: _\\61 lc(min("));
    }

    #[test]
    fn parses_deeply_nested_math_functions_that_do_not_parse_quickly() {
        let start = std::time::Instant::now();
        for css in [
            format!("width: {}x{}", "calc(".repeat(40), ")".repeat(40)),
            "width: calc(".repeat(40),
            format!("width: {}", "min(".repeat(40)),
        ] {
            assert!(
                crate::parse_declarations(&flat_math(&css))
                    .is_none_or(|parsed| { parsed.declarations.declarations.is_empty() })
            );
        }
        assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
    }

    #[test]
    fn makes_color_functions_nested_too_deep_invalid() {
        let fine = format!("--c: {}red{}", "light-dark(".repeat(8), ")".repeat(8));
        assert_eq!(flat_math(&fine), fine);
        let deep = format!("--c: {}", "rgb(1 2 3 / hsl(1 2% 3% / Light-Dark(".repeat(3));
        assert_eq!(
            flat_math(&deep),
            format!(
                "--c: {}rgb(1 2 3 / hsl(1 2% 3% / _ight-Dark(",
                "rgb(1 2 3 / hsl(1 2% 3% / Light-Dark(".repeat(2)
            )
        );
        // Other functions between them do not count, nor do the colors that lightningcss
        // parses once, nor closed ones.
        let kept = format!(
            "--c: {} {} {}",
            "light-dark(rgb(1 2 3), x, foo(".repeat(7),
            "color-mix(in srgb, rgba(".repeat(20),
            "hsl(1 2% 3%) ".repeat(20)
        );
        assert_eq!(flat_math(&kept), kept);
        // Escaped names may spell color functions.
        let escaped = format!("--c: {}", "r\\67 b(".repeat(9));
        assert!(flat_math(&escaped).ends_with("_\\67 b("));
    }

    #[test]
    fn parses_deeply_nested_color_functions_that_do_not_parse_quickly() {
        // Found by the fuzz target `css`: twice the time for each level, 13 seconds for 22 levels.
        let start = std::time::Instant::now();
        for value in [
            "light-dark(".repeat(40),
            format!("{}]", "light-dark(a, ".repeat(40)),
            format!("{}]", "rgb(1 2 3 / ".repeat(40)),
            format!("{}]", "hsl(1 2% 3% / ".repeat(40)),
            format!("{}]", "lch(60% light-dark(w: calc(900 ".repeat(40)),
        ] {
            let _ = crate::parse_declarations(&format!("--c: {value}"));
            let _ = crate::parse_declarations(&format!("color: {value}"));
            for css in [format!(":root {{ --c: {value} }}"), format!("@x {{ {value} }}")] {
                let _ = crate::Stylesheet::parse(&css, crate::Origin::Author, "a.css");
            }
        }
        assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
    }

    #[test]
    fn counts_hundreds_of_thousands_of_nested_math_functions_in_linear_time() {
        // Each math function counted those open around it: quadratic, a second for 100,000.
        let css = format!("a {{ width: {}", "min(".repeat(400_000));
        let start = std::time::Instant::now();
        assert!(flat_math(&css).starts_with("a { width: _in(min("));
        assert!(start.elapsed().as_secs() < 1, "took {:?}", start.elapsed());
    }
}
