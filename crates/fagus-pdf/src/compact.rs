//! Object streams: the many small objects of a PDF (pages, font dictionaries, annotations, the
//! structure tree) compressed together, with a cross-reference stream (PDF 1.7, §7.5.7 and
//! §7.5.8).
//!
//! krilla writes every object on its own, uncompressed, with a cross-reference table; for a
//! tagged PDF the structure elements then make up most of the file. The PDF is rewritten as one
//! revision: streams stay as they are, all other objects go into object streams. Incremental
//! updates (such as the one for Factur-X metadata) are merged, the newest version of each object
//! winning. Anything unexpected leaves the PDF as it was.
//!
//! An encrypted PDF is written in the same pass: the streams and the object streams are
//! encrypted, and with them the strings of the objects inside (ISO 32000-2, §7.5.7); the strings
//! of stream dictionaries are encrypted one by one. The cross-reference stream and the
//! encryption dictionary stay as they are, as the standard requires.

use std::fmt::Write as _;
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::encrypt::Encryptor;
use crate::parse::{self, Object};

/// Objects per object stream.
const PER_STREAM: usize = 128;

/// The PDF with its objects in object streams and a cross-reference stream, or `None` if it is
/// not a PDF this understands. `catalog` holds entries for the document catalog that krilla does
/// not write; those it has already are left as they are, but for dictionaries, which are merged.
/// With an encryptor, the PDF is encrypted.
pub(crate) fn compact(
    pdf: &[u8],
    catalog: &[(&str, &str)],
    mut encryptor: Option<&mut Encryptor>,
) -> Option<Vec<u8>> {
    let references = parse::cross_references(pdf)?;
    let extended: Vec<(&str, &str)>;
    let catalog = match encryptor {
        // AES-256 belongs to PDF 2.0; PDF 1.7 has it as Adobe's extension level 8.
        Some(_) if pdf.starts_with(b"%PDF-1.") => {
            extended = [("/Extensions", "<</ADBE<</BaseVersion/1.7/ExtensionLevel 8>>>>")]
                .into_iter()
                .chain(catalog.iter().copied())
                .collect();
            &extended[..]
        }
        _ => catalog,
    };
    let root = parse::entry(&references.trailer, "/Root")
        .and_then(|root| root.split_whitespace().next()?.parse::<u32>().ok());
    let mut objects: Vec<Object> = Vec::with_capacity(references.offsets.len());
    for (&number, &offset) in &references.offsets {
        objects.push(parse::object(pdf, number, offset)?);
    }
    let header_end = objects.iter().map(|object| object.whole.start).min()?;
    let header = &pdf[..header_end];
    if !header.starts_with(b"%PDF-") {
        return None;
    }

    let mut out = Vec::with_capacity(pdf.len());
    out.extend_from_slice(header);
    let highest = objects.last()?.number;
    // Entries of the cross-reference stream (type, second and third field), by object number.
    let mut entries: Vec<(u8, u64, u16)> = vec![(0, 0, 0); highest as usize + 1];
    entries[0] = (0, 0, u16::MAX);
    for object in objects.iter().filter(|object| object.data.is_some()) {
        entries[object.number as usize] = (1, out.len() as u64, 0);
        if let Some(encryptor) = encryptor.as_deref_mut() {
            encrypted_stream(&mut out, pdf, object, encryptor)?;
        } else {
            out.extend_from_slice(&pdf[object.whole.clone()]);
            out.push(b'\n');
        }
    }
    // The catalog of an encrypted PDF stays outside the object streams, with its strings
    // encrypted one by one: viewers such as macOS's look for it before they ask for the
    // password, which the object streams need.
    let mut catalog_outside = false;
    if let Some(encryptor) = encryptor.as_deref_mut()
        && let Some(object) = objects
            .iter()
            .find(|object| object.data.is_none() && Some(object.number) == root)
    {
        let body = with_entries(parse::trim(&pdf[object.body.clone()]), catalog)?;
        let body = encryptor.encrypt_strings(&body)?;
        entries[object.number as usize] = (1, out.len() as u64, 0);
        writeln!(out, "{} 0 obj", object.number).ok()?;
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\nendobj\n");
        catalog_outside = true;
    }
    let small: Vec<&Object> = objects
        .iter()
        .filter(|object| object.data.is_none() && !(catalog_outside && Some(object.number) == root))
        .collect();
    let mut next = highest + 1;
    // The object streams with their numbers, filled first and compressed side by side.
    let mut streams: Vec<(u32, usize, usize, Vec<u8>)> = Vec::new();
    for group in small.chunks(PER_STREAM) {
        let number = next;
        next += 1;
        let mut index = String::new();
        let mut data = Vec::new();
        for (position, object) in group.iter().enumerate() {
            write!(index, "{} {} ", object.number, data.len()).ok()?;
            let body = parse::trim(&pdf[object.body.clone()]);
            if Some(object.number) == root {
                data.extend_from_slice(&with_entries(body, catalog)?);
            } else {
                data.extend_from_slice(body);
            }
            data.push(b'\n');
            entries[object.number as usize] = (2, u64::from(number), u16::try_from(position).ok()?);
        }
        index.push('\n');
        let mut content = index.into_bytes();
        let first = content.len();
        content.extend_from_slice(&data);
        streams.push((number, group.len(), first, content));
    }
    #[cfg(feature = "parallel")]
    let compressed: Vec<Option<Vec<u8>>> = streams
        .par_iter()
        .map(|(_, _, _, content)| deflate(content))
        .collect();
    #[cfg(not(feature = "parallel"))]
    let compressed: Vec<Option<Vec<u8>>> = streams
        .iter()
        .map(|(_, _, _, content)| deflate(content))
        .collect();
    for ((number, count, first, _), compressed) in streams.iter().zip(compressed) {
        let mut compressed = compressed?;
        if let Some(encryptor) = encryptor.as_deref_mut() {
            compressed = encryptor.encrypt(&compressed)?;
        }
        entries.push((1, out.len() as u64, 0));
        write!(
            out,
            "{number} 0 obj\n<</Type/ObjStm/N {count}/First {first}/Filter/FlateDecode/Length {}>>\nstream\n",
            compressed.len()
        )
        .ok()?;
        out.extend_from_slice(&compressed);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }

    // The encryption dictionary, which no object stream may hold, and which needs the
    // identifier of the trailer.
    let mut encrypt = String::new();
    if let Some(encryptor) = encryptor {
        parse::entry(&references.trailer, "/ID")?;
        entries.push((1, out.len() as u64, 0));
        write!(out, "{next} 0 obj\n{}\nendobj\n", encryptor.dictionary()).ok()?;
        encrypt = format!("/Encrypt {next} 0 R");
        next += 1;
    }

    // The cross-reference stream lists itself too, and carries the trailer's entries.
    let xref = next;
    let xref_offset = out.len() as u64;
    entries.push((1, xref_offset, 0));
    let width = (1..8)
        .find(|bytes| entries.iter().all(|entry| entry.1 < 1_u64 << (8 * bytes)))
        .unwrap_or(8);
    let mut table = Vec::with_capacity(entries.len() * (width + 3));
    for (kind, field, generation) in &entries {
        table.push(*kind);
        table.extend_from_slice(&field.to_be_bytes()[8 - width..]);
        table.extend_from_slice(&generation.to_be_bytes());
    }
    let table = deflate(&table)?;
    let mut trailer = String::new();
    for key in ["/Root", "/Info", "/ID"] {
        if let Some(value) = parse::entry(&references.trailer, key) {
            trailer.push_str(key);
            trailer.push(' ');
            trailer.push_str(value.trim());
        }
    }
    trailer.push_str(&encrypt);
    write!(
        out,
        "{xref} 0 obj\n<</Type/XRef/Size {}/W[1 {width} 2]{trailer}/Filter/FlateDecode/Length {}>>\nstream\n",
        entries.len(),
        table.len()
    )
    .ok()?;
    out.extend_from_slice(&table);
    write!(out, "\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").ok()?;
    Some(out)
}

/// Writes a stream object encrypted: its data, and the strings of its dictionary, whose length
/// changes. `None` if the object is not as expected or encrypting fails.
fn encrypted_stream(out: &mut Vec<u8>, pdf: &[u8], object: &Object, encryptor: &mut Encryptor) -> Option<()> {
    let start = parse::skip_whitespace(pdf, object.body.start);
    let end = parse::dictionary_end(pdf, start)?;
    // The strings are hexadecimal now: the dictionary is text.
    let dictionary = String::from_utf8(encryptor.encrypt_strings(&pdf[start..end])?).ok()?;
    let data = encryptor.encrypt(&pdf[object.data.clone()?])?;
    let length = parse::entry(&dictionary, "/Length")?;
    let at = length.as_ptr() as usize - dictionary.as_ptr() as usize;
    write!(
        out,
        "{} 0 obj\n{} {}{}\nstream\n",
        object.number,
        &dictionary[..at],
        data.len(),
        &dictionary[at + length.len()..]
    )
    .ok()?;
    out.extend_from_slice(&data);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    Some(())
}

/// The PDF as one revision with a cross-reference table, every object on its own, for PDF 1.4
/// (PDF/A-1), which has neither object streams nor cross-reference streams; `None` if it is not
/// a PDF this understands. Incremental updates are merged and `catalog` holds entries for the
/// document catalog, as with [`compact`].
pub(crate) fn classic(pdf: &[u8], catalog: &[(&str, &str)]) -> Option<Vec<u8>> {
    let references = parse::cross_references(pdf)?;
    let root = parse::entry(&references.trailer, "/Root")
        .and_then(|root| root.split_whitespace().next()?.parse::<u32>().ok());
    let mut objects: Vec<Object> = Vec::with_capacity(references.offsets.len());
    for (&number, &offset) in &references.offsets {
        objects.push(parse::object(pdf, number, offset)?);
    }
    let header_end = objects.iter().map(|object| object.whole.start).min()?;
    let header = &pdf[..header_end];
    if !header.starts_with(b"%PDF-") {
        return None;
    }
    let mut out = Vec::with_capacity(pdf.len());
    out.extend_from_slice(header);
    let size = objects.last()?.number as usize + 1;
    let mut offsets: Vec<Option<usize>> = vec![None; size];
    for object in &objects {
        offsets[object.number as usize] = Some(out.len());
        if object.data.is_none() && Some(object.number) == root {
            let body = with_entries(parse::trim(&pdf[object.body.clone()]), catalog)?;
            writeln!(out, "{} 0 obj", object.number).ok()?;
            out.extend_from_slice(&body);
            out.extend_from_slice(b"\nendobj\n");
        } else {
            out.extend_from_slice(&pdf[object.whole.clone()]);
            out.push(b'\n');
        }
    }
    let xref = out.len();
    write!(out, "xref\n0 {size}\n").ok()?;
    for (number, offset) in offsets.iter().enumerate() {
        match offset {
            Some(offset) => write!(out, "{offset:010} 00000 n\r\n").ok()?,
            None if number == 0 => out.extend_from_slice(b"0000000000 65535 f\r\n"),
            None => out.extend_from_slice(b"0000000000 00000 f\r\n"),
        }
    }
    write!(out, "trailer\n<</Size {size}").ok()?;
    for key in ["/Root", "/Info", "/ID"] {
        if let Some(value) = parse::entry(&references.trailer, key) {
            write!(out, "{key} {}", value.trim()).ok()?;
        }
    }
    write!(out, ">>\nstartxref\n{xref}\n%%EOF\n").ok()?;
    Some(out)
}

/// A dictionary with `entries` added where it has none of the key. Where both the dictionary
/// and an entry have a dictionary for a key (as with `/ViewerPreferences`, which krilla writes
/// for PDF/UA), the two are merged the same way; any other value the dictionary has stays as it
/// is.
fn with_entries(dictionary: &[u8], entries: &[(&str, &str)]) -> Option<Vec<u8>> {
    if !dictionary.ends_with(b">>") {
        return Some(dictionary.to_vec());
    }
    let mut text = std::str::from_utf8(dictionary).ok()?.to_owned();
    for (key, value) in entries {
        let Some(existing) = parse::entry(&text, key) else {
            text.truncate(text.len() - 2);
            text.push_str(key);
            text.push_str(value);
            text.push_str(">>");
            continue;
        };
        let inner = existing.trim();
        if value.starts_with("<<") && inner.starts_with("<<") && inner.ends_with(">>") {
            let start = inner.as_ptr() as usize - text.as_ptr() as usize;
            let range = start..start + inner.len();
            let merged = with_entries(inner.as_bytes(), &parse::entries(value)?)?;
            text.replace_range(range, std::str::from_utf8(&merged).ok()?);
        }
    }
    Some(text.into_bytes())
}

fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(data.len() / 3), Compression::new(6));
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;

    use fagus_paint::{Item, PageList, RectF};
    use fagus_style::values::Rgba;
    use fagus_text::FontDb;

    use super::*;
    use crate::parse::{
        cross_references, dictionary_end, entry, find, number_after, object, rfind, skip_whitespace, trim,
    };
    use crate::{Extension, Link, LinkTarget, Metadata, Options};

    fn inflate(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_end(&mut out)
            .expect("deflated data");
        out
    }

    /// The dictionary and the decoded data of a stream object.
    fn stream(pdf: &[u8], number: usize, offset: usize) -> (String, Vec<u8>) {
        decrypted_stream(pdf, number, offset, None)
    }

    /// [`stream`] of an encrypted PDF, with its file key.
    fn decrypted_stream(
        pdf: &[u8],
        number: usize,
        offset: usize,
        key: Option<&[u8; 32]>,
    ) -> (String, Vec<u8>) {
        let object = object(pdf, u32::try_from(number).expect("number"), offset).expect("object");
        let data = object.data.expect("a stream");
        let start = skip_whitespace(pdf, object.body.start);
        let end = dictionary_end(pdf, start).expect("dictionary");
        let dictionary = std::str::from_utf8(&pdf[start..end]).expect("text").to_owned();
        let data = match key {
            Some(key) => crate::encrypt::tests::decrypt(key, &pdf[data]),
            None => pdf[data].to_vec(),
        };
        (dictionary, inflate(&data))
    }

    /// The bodies of all objects of a compacted PDF, read back through its cross-reference
    /// stream and object streams.
    fn read_back(pdf: &[u8]) -> BTreeMap<u32, Vec<u8>> {
        read_back_decrypted(pdf, None)
    }

    /// [`read_back`] of an encrypted PDF, with its file key: the objects in object streams come
    /// decrypted, the streams as they are.
    fn read_back_decrypted(pdf: &[u8], key: Option<&[u8; 32]>) -> BTreeMap<u32, Vec<u8>> {
        let start = rfind(pdf, b"startxref").expect("startxref");
        let xref_offset = number_after(pdf, start + 9).expect("offset").0;
        let xref_number = number_after(pdf, xref_offset).expect("number").0;
        let (dictionary, table) = stream(pdf, xref_number, xref_offset);
        assert!(dictionary.contains("/Type/XRef"));
        for key in ["/Root", "/Info", "/ID"] {
            assert!(entry(&dictionary, key).is_some(), "{key}");
        }
        let widths: Vec<usize> = entry(&dictionary, "/W")
            .expect("W")
            .trim_matches([' ', '[', ']'])
            .split_whitespace()
            .map(|width| width.parse().expect("width"))
            .collect();
        let row_size: usize = widths.iter().sum();
        let rows: Vec<(usize, usize, usize)> = table
            .chunks(row_size)
            .map(|row| {
                let field = |bytes: &[u8]| {
                    bytes
                        .iter()
                        .fold(0, |value, byte| value << 8 | usize::from(*byte))
                };
                (
                    field(&row[..widths[0]]),
                    field(&row[widths[0]..widths[0] + widths[1]]),
                    field(&row[widths[0] + widths[1]..]),
                )
            })
            .collect();
        let object_streams: BTreeMap<usize, Vec<(u32, Vec<u8>)>> = rows
            .iter()
            .filter(|row| row.0 == 2)
            .map(|row| row.1)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|number| {
                let (dictionary, data) = decrypted_stream(pdf, number, rows[number].1, key);
                assert!(dictionary.contains("/Type/ObjStm"));
                let first: usize = entry(&dictionary, "/First")
                    .expect("First")
                    .trim()
                    .parse()
                    .expect("first");
                let index: Vec<usize> = std::str::from_utf8(&data[..first])
                    .expect("index")
                    .split_whitespace()
                    .map(|value| value.parse().expect("number"))
                    .collect();
                let objects = index
                    .chunks(2)
                    .enumerate()
                    .map(|(position, pair)| {
                        let end = index
                            .get(position * 2 + 3)
                            .map_or(data.len(), |next| first + next);
                        let body = trim(&data[first + pair[1]..end]).to_vec();
                        (u32::try_from(pair[0]).expect("number"), body)
                    })
                    .collect();
                (number, objects)
            })
            .collect();
        let mut objects = BTreeMap::new();
        for (number, &(kind, second, third)) in rows.iter().enumerate() {
            let is_object_stream = object_streams.contains_key(&number);
            match kind {
                1 if number != xref_number && !is_object_stream => {
                    let object = object(pdf, u32::try_from(number).expect("number"), second).expect("object");
                    objects.insert(object.number, trim(&pdf[object.body]).to_vec());
                }
                2 => {
                    let (found, body) = object_streams[&second][third].clone();
                    assert_eq!(found as usize, number);
                    objects.insert(found, body);
                }
                _ => {}
            }
        }
        objects
    }

    /// A PDF whose title and link text could fool a careless reader, with or without an
    /// incremental update of its metadata.
    fn sample(update: bool) -> Vec<u8> {
        let rect = RectF {
            x: 10.0,
            y: 10.0,
            width: 50.0,
            height: 20.0,
        };
        let page = PageList {
            width: 200.0,
            height: 100.0,
            items: vec![Item::Rect {
                rect,
                color: Rgba::BLACK,
                radii: None,
            }],
            bleed: 0.0,
            marks: fagus_layout::PageMarks::default(),
            orientation: fagus_layout::PageOrientation::Upright,
        };
        let link = Link {
            rect: [10.0, 10.0, 50.0, 20.0],
            target: LinkTarget::Uri("https://quittio.dev/endobj".to_owned()),
            node: None,
            description: Some("endobj ) ( \\ stream >> << /Length 3 %".to_owned()),
            role: crate::LinkRole::Element,
        };
        let options = Options {
            links: vec![vec![link.clone()], vec![link]],
            xmp: update.then(|| Extension {
                descriptions:
                    "<rdf:Description rdf:about=\"\" xmlns:x=\"urn:x\"><x:a>b</x:a></rdf:Description>"
                        .to_owned(),
                schemas: String::new(),
            }),
            ..Options::default()
        };
        let metadata = Metadata {
            title: Some("Rechnung (endobj) \\".to_owned()),
            ..Metadata::default()
        };
        crate::write_uncompacted(&mut [page.clone(), page], &FontDb::new(), &metadata, &options)
            .expect("written")
            .pdf
    }

    #[test]
    fn keeps_every_object() {
        for update in [false, true] {
            let plain = sample(update);
            let compacted = compact(&plain, &[], None).expect("compacted");
            let original: BTreeMap<u32, Vec<u8>> = cross_references(&plain)
                .expect("cross-reference table")
                .offsets
                .iter()
                .map(|(&number, &offset)| {
                    let object = object(&plain, number, offset).expect("object");
                    (number, trim(&plain[object.body]).to_vec())
                })
                .collect();
            assert_eq!(read_back(&compacted), original);
            assert!(compacted.len() < plain.len());
            if update {
                // The newest metadata wins.
                assert!(
                    original
                        .values()
                        .any(|body| find(body, b"<x:a>b</x:a>").is_some())
                );
            }
        }
    }

    #[test]
    fn adds_entries_to_the_catalog() {
        let plain = sample(false);
        let entries = [
            ("/ViewerPreferences", "<</DisplayDocTitle true>>"),
            ("/Type", "/Other"),
        ];
        let objects = read_back(&compact(&plain, &entries, None).expect("compacted"));
        let catalogs: Vec<String> = objects
            .values()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .filter(|body| body.contains("/Catalog"))
            .collect();
        assert_eq!(catalogs.len(), 1, "{catalogs:?}");
        assert!(
            catalogs[0].ends_with("/ViewerPreferences<</DisplayDocTitle true>>>>"),
            "{}",
            catalogs[0]
        );
        // An entry the catalog has stays as it is.
        assert!(!catalogs[0].contains("/Other"), "{}", catalogs[0]);
    }

    #[test]
    fn merges_dictionaries_the_catalog_has_already() {
        let merged = |dictionary: &str, entries: &[(&str, &str)]| {
            String::from_utf8(with_entries(dictionary.as_bytes(), entries).expect("merged")).expect("text")
        };
        let preferences = [(
            "/ViewerPreferences",
            "<</DisplayDocTitle true/FitWindow true/CenterWindow true>>",
        )];
        // As krilla writes the catalog of a PDF/UA file: the entry is there, and stays first.
        assert_eq!(
            merged(
                "<<\n  /Type /Catalog\n  /ViewerPreferences <<\n    /DisplayDocTitle true\n  >>\n  /Lang (de)\n>>",
                &preferences
            ),
            "<<\n  /Type /Catalog\n  /ViewerPreferences <<\n    /DisplayDocTitle true\n  /FitWindow true/CenterWindow true>>\n  /Lang (de)\n>>"
        );
        // Nothing to add leaves the bytes as they are; a value of the catalog wins.
        let same = "<</ViewerPreferences<</FitWindow false/CenterWindow true/DisplayDocTitle true>>>>";
        assert_eq!(merged(same, &preferences), same);
        // Dictionaries inside, and names that begin alike, are told apart.
        assert_eq!(
            merged(
                "<</ViewerPreferences<</Other<</FitWindow true>>/FitWindowed [1 2]>>/Pages 2 0 R>>",
                &preferences
            ),
            "<</ViewerPreferences<</Other<</FitWindow true>>/FitWindowed [1 2]\
             /DisplayDocTitle true/FitWindow true/CenterWindow true>>/Pages 2 0 R>>"
        );
        // A reference is no dictionary to merge into, and a name is no dictionary to merge.
        let reference = "<</ViewerPreferences 7 0 R/PageMode/UseNone>>";
        assert_eq!(
            merged(reference, &[preferences[0], ("/PageMode", "/UseOutlines")]),
            reference
        );
        assert_eq!(
            merged(
                "<</Type/Catalog>>",
                &[preferences[0], ("/PageMode", "/UseOutlines")]
            ),
            "<</Type/Catalog/ViewerPreferences<</DisplayDocTitle true/FitWindow true/CenterWindow true>>\
             /PageMode/UseOutlines>>"
        );
        assert_eq!(
            parse::entries("<< /A 1/B<</C 2>> /D[/E /F]/G(>>)>>"),
            Some(vec![
                ("/A", " 1"),
                ("/B", "<</C 2>> "),
                ("/D", "[/E /F]"),
                ("/G", "(>>)")
            ])
        );
        assert_eq!(parse::entries("<</A 1"), None);
        assert_eq!(parse::entries("[1 2]"), None);
    }

    #[test]
    fn writes_one_revision_with_a_cross_reference_table() {
        for update in [false, true] {
            let original = sample(update);
            let entries = [("/ViewerPreferences", "<</DisplayDocTitle true>>")];
            let rewritten = classic(&original, &entries).expect("rewritten");
            // One revision, no streams of objects or cross references (PDF 1.4).
            assert_eq!(
                rewritten
                    .windows(9)
                    .filter(|window| window == b"startxref")
                    .count(),
                1
            );
            assert!(find(&rewritten, b"/ObjStm").is_none() && find(&rewritten, b"/XRef").is_none());
            assert!(find(&rewritten, b"/Prev").is_none());
            let references = cross_references(&rewritten).expect("cross-reference table");
            let read = |pdf: &[u8], references: &parse::CrossReferences| -> BTreeMap<u32, Vec<u8>> {
                references
                    .offsets
                    .iter()
                    .map(|(&number, &offset)| {
                        let object = object(pdf, number, offset).expect("object");
                        (number, trim(&pdf[object.body]).to_vec())
                    })
                    .collect()
            };
            let mut expected = read(&original, &cross_references(&original).expect("table"));
            for body in expected.values_mut() {
                if find(body, b"/Type/Catalog").is_some() {
                    *body = with_entries(body, &entries).expect("catalog");
                }
            }
            assert_eq!(read(&rewritten, &references), expected);
            for key in ["/Root", "/Info", "/ID"] {
                assert!(entry(&references.trailer, key).is_some(), "{key}");
            }
            // Every entry of the table is 20 bytes long.
            let table = find(&rewritten, b"\nxref\n").expect("xref") + 6;
            let rows = &rewritten[table..rfind(&rewritten, b"trailer").expect("trailer")];
            let rows = &rows[rows.iter().position(|byte| *byte == b'\n').expect("subsection") + 1..];
            assert_eq!(rows.len() % 20, 0);
            assert!(rows.chunks(20).all(|row| row.ends_with(b"\r\n")));
        }
    }

    /// The strings of an object, decoded (and decrypted with `key`), and the object with each
    /// string replaced by `()` and the value of `/Length` by `0`.
    fn strings(object: &[u8], key: Option<&[u8; 32]>) -> (Vec<Vec<u8>>, String) {
        let mut strings = Vec::new();
        let mut rest = Vec::new();
        let mut position = 0;
        while let Some(&byte) = object.get(position) {
            let end = match byte {
                b'(' => parse::string_end(object, position).expect("string end"),
                b'<' if object.get(position + 1) != Some(&b'<') => {
                    position + find(&object[position..], b">").expect("hexadecimal end")
                }
                b'<' => {
                    rest.extend_from_slice(b"<<");
                    position += 2;
                    continue;
                }
                _ => {
                    rest.push(byte);
                    position += 1;
                    continue;
                }
            };
            let encoded = &object[position..=end];
            let decoded = crate::encrypt::tests::decode(encoded);
            strings.push(match key {
                Some(key) => crate::encrypt::tests::decrypt(key, &decoded),
                None => decoded,
            });
            rest.extend_from_slice(b"()");
            position = end + 1;
        }
        let rest = String::from_utf8(rest).expect("text");
        let length = entry(&rest, "/Length").map_or(String::new(), str::to_owned);
        (
            strings,
            rest.replacen(&format!("/Length{length}"), "/Length 0", 1),
        )
    }

    #[test]
    fn encrypts_every_string_and_stream() {
        use crate::encrypt::tests::{decrypt, file_key};
        use crate::{Encryption, encrypt::Encryptor};

        for update in [false, true] {
            let plain = sample(update);
            let compacted = compact(&plain, &[], None).expect("compacted");
            let encryption = Encryption::new()
                .user_password("user")
                .owner_password("owner")
                .seed([3; 32]);
            let mut encryptor = Encryptor::new(&encryption).expect("encryptor");
            let encrypted = compact(&plain, &[], Some(&mut encryptor)).expect("encrypted");
            // The same seed, the same bytes.
            let mut again = Encryptor::new(&encryption).expect("encryptor");
            assert_eq!(
                compact(&plain, &[], Some(&mut again)).expect("encrypted"),
                encrypted
            );

            // The trailer points to the encryption dictionary, outside the object streams.
            let start = rfind(&encrypted, b"startxref").expect("startxref");
            let xref_offset = number_after(&encrypted, start + 9).expect("offset").0;
            let xref_number = number_after(&encrypted, xref_offset).expect("number").0;
            let (trailer, _) = stream(&encrypted, xref_number, xref_offset);
            let reference: u32 = entry(&trailer, "/Encrypt")
                .expect("Encrypt")
                .split_whitespace()
                .next()
                .expect("number")
                .parse()
                .expect("number");
            assert_eq!(
                entry(&trailer, "/ID").map(str::trim),
                entry(&cross_references(&plain).expect("table").trailer, "/ID").map(str::trim),
                "the identifier is not encrypted"
            );
            let at = find(&encrypted, format!("\n{reference} 0 obj\n").as_bytes()).expect("Encrypt") + 1;
            let encrypt = object(&encrypted, reference, at).expect("object");
            let dictionary = std::str::from_utf8(trim(&encrypted[encrypt.body])).expect("text");
            assert!(
                dictionary.starts_with("<</Filter/Standard/V 5/R 6/"),
                "{dictionary}"
            );
            assert_eq!(file_key(dictionary, "wrong"), None);
            let (key, _) = file_key(dictionary, "owner").expect("owner password");
            assert_eq!(file_key(dictionary, "user"), Some((key, false)));

            // Every object as before; the catalog with Adobe's extension for AES-256 in PDF 1.7.
            let references = cross_references(&plain).expect("table");
            let is_stream = |number: u32| {
                object(&plain, number, references.offsets[&number])
                    .expect("object")
                    .data
                    .is_some()
            };
            let expected = read_back(&compacted);
            let found = read_back_decrypted(&encrypted, Some(&key));
            let mut streams = 0;
            for (number, body) in &expected {
                let found = &found[number];
                if find(body, b"/Type/Catalog").is_some() {
                    // Outside the object streams, with its strings encrypted.
                    let extended = with_entries(
                        body,
                        &[("/Extensions", "<</ADBE<</BaseVersion/1.7/ExtensionLevel 8>>>>")],
                    )
                    .expect("catalog");
                    assert_eq!(strings(found, Some(&key)), strings(&extended, None));
                    continue;
                }
                if !is_stream(*number) {
                    assert_eq!(found, body, "object {number}");
                    continue;
                }
                // A stream: its data and the strings of its dictionary encrypted.
                streams += 1;
                let parse = |body: &[u8]| {
                    let start = skip_whitespace(body, 0);
                    let end = dictionary_end(body, start).expect("dictionary");
                    let data = &body[end..];
                    let data_start = find(data, b"stream").expect("stream") + 7;
                    let data_end = rfind(data, b"endstream").expect("endstream") - 1;
                    (body[start..end].to_vec(), data[data_start..data_end].to_vec())
                };
                let (plain_dictionary, plain_data) = parse(body);
                let (dictionary, data) = parse(found);
                assert_eq!(decrypt(&key, &data), plain_data, "object {number}");
                let (plain_strings, plain_rest) = strings(&plain_dictionary, None);
                let (found_strings, found_rest) = strings(&dictionary, Some(&key));
                assert_eq!(found_strings, plain_strings, "object {number}");
                assert_eq!(found_rest, plain_rest, "object {number}");
                assert_eq!(
                    entry(std::str::from_utf8(&dictionary).expect("text"), "/Length").map(str::trim),
                    Some(data.len().to_string().as_str())
                );
            }
            assert!(streams > 2, "{streams}");
            // Nothing readable is left: not the title, not the text of a link.
            assert!(find(&compacted, b"Rechnung").is_some());
            assert!(find(&encrypted, b"Rechnung").is_none());
            assert!(find(&encrypted, b"quittio.dev").is_none());
        }
    }

    #[test]
    fn leaves_what_it_does_not_understand() {
        assert!(compact(b"%PDF-1.7\nnot really", &[], None).is_none());
        let plain = sample(false);
        let broken = [&plain[..plain.len() - 30], b"startxref\n99999999\n%%EOF\n"].concat();
        assert!(compact(&broken, &[], None).is_none());
        assert!(classic(&broken, &[]).is_none());
    }
}
