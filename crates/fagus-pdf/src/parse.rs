//! Reading the PDFs krilla writes, for the steps after it: extending the metadata and compacting
//! the objects. Objects are found through the cross-reference tables, and the data of a stream
//! through its length, so that no text inside a string or a stream (a title that says `endobj`)
//! can be taken for the structure of the file.

use std::collections::BTreeMap;
use std::ops::Range;

/// The cross-reference tables of a PDF, newest first.
pub(crate) struct CrossReferences {
    /// The newest offset of every object in use.
    pub(crate) offsets: BTreeMap<u32, usize>,
    /// The newest trailer dictionary, `<<` to `>>`.
    pub(crate) trailer: String,
    /// Where the newest cross-reference table starts.
    pub(crate) start: usize,
}

/// An object of a PDF.
pub(crate) struct Object {
    pub(crate) number: u32,
    /// The bytes between `obj` and `endobj`.
    pub(crate) body: Range<usize>,
    /// From `N 0 obj` to the end of `endobj`.
    pub(crate) whole: Range<usize>,
    /// The data of a stream.
    pub(crate) data: Option<Range<usize>>,
}

/// The cross-reference tables from the end of the file back through `/Prev`. `None` for a file
/// with cross-reference streams, encryption or objects of a generation other than 0.
pub(crate) fn cross_references(pdf: &[u8]) -> Option<CrossReferences> {
    let startxref = rfind(pdf, b"startxref")?;
    let start = number_after(pdf, startxref + 9)?.0;
    let mut position = start;
    let mut offsets = BTreeMap::new();
    let mut newest = None;
    for _ in 0..64 {
        if !pdf.get(position..)?.starts_with(b"xref") {
            return None;
        }
        position += 4;
        // Subsections: the first number and the count, then entries of 20 bytes.
        loop {
            position = skip_whitespace(pdf, position);
            if pdf.get(position..)?.starts_with(b"trailer") {
                break;
            }
            let (first, after) = number_after(pdf, position)?;
            let (count, after) = number_after(pdf, after)?;
            position = skip_whitespace(pdf, after);
            for entry in 0..count {
                let line = pdf.get(position..position + 18)?;
                let offset: usize = std::str::from_utf8(&line[..10]).ok()?.parse().ok()?;
                let generation: u32 = std::str::from_utf8(&line[11..16]).ok()?.parse().ok()?;
                if line[17] == b'n' {
                    if generation != 0 {
                        return None;
                    }
                    offsets
                        .entry(u32::try_from(first + entry).ok()?)
                        .or_insert(offset);
                }
                position = skip_whitespace(pdf, position + 18);
            }
        }
        let dictionary = skip_whitespace(pdf, position + 7);
        let trailer = std::str::from_utf8(&pdf[dictionary..dictionary_end(pdf, dictionary)?]).ok()?;
        if trailer.contains("/Encrypt") || trailer.contains("/XRefStm") {
            return None;
        }
        let previous = entry(trailer, "/Prev");
        newest.get_or_insert_with(|| trailer.to_owned());
        match previous {
            Some(previous) => position = previous.trim().parse().ok()?,
            None => {
                return Some(CrossReferences {
                    offsets,
                    trailer: newest?,
                    start,
                });
            }
        }
    }
    None
}

/// The object `number` at `offset`.
pub(crate) fn object(pdf: &[u8], number: u32, offset: usize) -> Option<Object> {
    let (found, after) = number_after(pdf, offset)?;
    let (generation, after) = number_after(pdf, after)?;
    let after = skip_whitespace(pdf, after);
    if found != number as usize || generation != 0 || !pdf.get(after..)?.starts_with(b"obj") {
        return None;
    }
    let body_start = after + 3;
    let value = skip_whitespace(pdf, body_start);
    // A stream is a dictionary followed by `stream`; its data may contain anything, so its end
    // comes from its length.
    let mut data = None;
    if pdf.get(value..)?.starts_with(b"<<") {
        let dictionary_end = dictionary_end(pdf, value)?;
        let stream = skip_whitespace(pdf, dictionary_end);
        if pdf.get(stream..)?.starts_with(b"stream") {
            let dictionary = std::str::from_utf8(&pdf[value..dictionary_end]).ok()?;
            let length: usize = entry(dictionary, "/Length")?.trim().parse().ok()?;
            let start = stream
                + 6
                + match pdf.get(stream + 6..stream + 8)? {
                    b"\r\n" => 2,
                    [b'\n', _] => 1,
                    _ => return None,
                };
            data = Some(start..start.checked_add(length)?);
        }
    }
    let end = keyword(pdf, data.as_ref().map_or(value, |data| data.end), b"endobj")?;
    Some(Object {
        number,
        body: body_start..end,
        whole: offset..end + 6,
        data,
    })
}

/// Where `word` first stands as a keyword from `start` on: outside strings and comments, and
/// not part of a name or another word.
pub(crate) fn keyword(pdf: &[u8], start: usize, word: &[u8]) -> Option<usize> {
    let regular = |byte: u8| !b" \t\r\n\x0c\0()<>[]{}/%".contains(&byte);
    let mut position = start;
    while let Some(&byte) = pdf.get(position) {
        match byte {
            b'(' => position = string_end(pdf, position)?,
            b'%' => {
                while pdf
                    .get(position)
                    .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
                {
                    position += 1;
                }
            }
            b'<' if pdf.get(position + 1) != Some(&b'<') => {
                position += find(pdf.get(position..)?, b">")?;
            }
            _ if pdf[position..].starts_with(word)
                && (position == 0 || !regular(pdf[position - 1]) && pdf[position - 1] != b'/')
                && pdf
                    .get(position + word.len())
                    .is_none_or(|&after| !regular(after)) =>
            {
                return Some(position);
            }
            _ => {}
        }
        position += 1;
    }
    None
}

/// The position of the `)` that ends the literal string starting at `start`: parentheses
/// balance, backslashes escape.
pub(crate) fn string_end(pdf: &[u8], start: usize) -> Option<usize> {
    let mut nesting = 0_usize;
    let mut position = start;
    loop {
        match *pdf.get(position)? {
            b'\\' => position += 1,
            b'(' => nesting += 1,
            b')' => {
                nesting -= 1;
                if nesting == 0 {
                    return Some(position);
                }
            }
            _ => {}
        }
        position += 1;
    }
}

/// The position after the dictionary starting at `start` (with `<<`).
pub(crate) fn dictionary_end(pdf: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut position = start;
    while let Some(&byte) = pdf.get(position) {
        match byte {
            b'(' => position = string_end(pdf, position)?,
            b'%' => {
                while pdf
                    .get(position)
                    .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
                {
                    position += 1;
                }
            }
            b'<' if pdf.get(position + 1) == Some(&b'<') => {
                depth += 1;
                position += 1;
            }
            b'>' if pdf.get(position + 1) == Some(&b'>') => {
                depth = depth.checked_sub(1)?;
                position += 1;
                if depth == 0 {
                    return Some(position + 1);
                }
            }
            // A hexadecimal string.
            b'<' => position += find(pdf.get(position..)?, b">")?,
            _ => {}
        }
        position += 1;
    }
    None
}

/// The value of `key` in the text of a dictionary (`<<` to `>>`): everything from the key to the
/// next key at its top level. Keys and values alternate, and a value may be a name too.
pub(crate) fn entry<'a>(dictionary: &'a str, key: &str) -> Option<&'a str> {
    let bytes = dictionary.as_bytes();
    let mut depth = 0_usize;
    let mut position = 0;
    let mut expecting_key = true;
    let mut found: Option<usize> = None;
    while let Some(&byte) = bytes.get(position) {
        match byte {
            b'(' => position = string_end(bytes, position)?,
            b'<' if bytes.get(position + 1) == Some(&b'<') => {
                depth += 1;
                position += 1;
            }
            b'>' if bytes.get(position + 1) == Some(&b'>') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return found.map(|start| &dictionary[start..position]);
                }
                position += 1;
            }
            // A hexadecimal string.
            b'<' => position += find(bytes.get(position..)?, b">")?,
            b'[' => depth += 1,
            b']' => depth = depth.checked_sub(1)?,
            b'/' if depth == 1 => {
                let name_end = bytes[position + 1..]
                    .iter()
                    .position(|byte| b" \t\r\n\x0c\0/[]<>()%".contains(byte))
                    .map_or(bytes.len(), |end| position + 1 + end);
                if expecting_key {
                    if let Some(start) = found {
                        return Some(&dictionary[start..position]);
                    }
                    if &dictionary[position..name_end] == key {
                        found = Some(name_end);
                    }
                }
                expecting_key = !expecting_key;
                position = name_end;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0' => {
                position += 1;
                continue;
            }
            _ => {}
        }
        // Anything else at the top level is (part of) a value: the next name is a key. Values
        // inside are done when the depth is back at the top.
        if depth == 1 {
            expecting_key = true;
        }
        position += 1;
    }
    None
}

/// The keys and values of the text of a dictionary (`<<` to `>>`), in their order; `None` if it
/// is not one.
pub(crate) fn entries(dictionary: &str) -> Option<Vec<(&str, &str)>> {
    let bytes = dictionary.as_bytes();
    let mut position = skip_whitespace(bytes, 0);
    if !bytes.get(position..)?.starts_with(b"<<") {
        return None;
    }
    position += 2;
    let mut entries = Vec::new();
    loop {
        position = skip_whitespace(bytes, position);
        if bytes.get(position..)?.starts_with(b">>") {
            return Some(entries);
        }
        if bytes.get(position) != Some(&b'/') {
            return None;
        }
        let key_end = bytes[position + 1..]
            .iter()
            .position(|byte| b" \t\r\n\x0c\0/[]<>()%".contains(byte))
            .map_or(bytes.len(), |end| position + 1 + end);
        let key = &dictionary[position..key_end];
        // The first entry of the key: a dictionary has each key once.
        let value = entry(dictionary, key)?;
        let value_start = value.as_ptr() as usize - dictionary.as_ptr() as usize;
        if value_start != key_end {
            return None;
        }
        entries.push((key, value));
        position = value_start + value.len();
    }
}

/// The number at or after `position` (after white space), and the position after it.
pub(crate) fn number_after(pdf: &[u8], position: usize) -> Option<(usize, usize)> {
    let start = skip_whitespace(pdf, position);
    let length = pdf
        .get(start..)?
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let number = std::str::from_utf8(&pdf[start..start + length])
        .ok()?
        .parse()
        .ok()?;
    Some((number, start + length))
}

pub(crate) fn skip_whitespace(pdf: &[u8], mut position: usize) -> usize {
    while pdf
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0'))
    {
        position += 1;
    }
    position
}

/// Bytes without the white space around them.
pub(crate) fn trim(bytes: &[u8]) -> &[u8] {
    let start = skip_whitespace(bytes, 0);
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0'))
        .map_or(start, |end| end + 1);
    &bytes[start..end.max(start)]
}

pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

pub(crate) fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dictionary_entries() {
        let dictionary =
            "<</Type/Annot/Contents(a /Rect \\) (b) [c)/Rect[1 2 3 4]/A<</URI(x)>>/Length1 9/Length 3>>";
        assert_eq!(entry(dictionary, "/Rect"), Some("[1 2 3 4]"));
        assert_eq!(entry(dictionary, "/Length"), Some(" 3"));
        assert_eq!(entry(dictionary, "/A"), Some("<</URI(x)>>"));
        assert_eq!(entry(dictionary, "/URI"), None, "not at the top level");
        assert_eq!(entry(dictionary, "/Type"), Some("/Annot"));
    }

    #[test]
    fn finds_keywords_outside_strings_and_names() {
        let pdf = b"<</T(endobj)/endobj 1/H<656e646f626a>>> %endobj\nendobj";
        assert_eq!(keyword(pdf, 0, b"endobj"), Some(pdf.len() - 6));
    }
}
