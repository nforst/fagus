//! Where a document's resources come from. Fagus reads nothing on its own: `data:` URLs, the
//! resources a caller hands over, a caller's loader, and files inside the document's base
//! directory are all there is. There is no network access.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

/// Loads resources (stylesheets, images, fonts) for a document. Implement it to fetch URLs from
/// a database, a cache or the network, under your own rules.
pub trait ResourceLoader: Send + Sync {
    /// The data at `url`, exactly as written in the document (relative URLs included), or `None`
    /// if it is not available. The documents of frames (`<iframe src>`, `<object data>`) ask for
    /// their resources by their URLs resolved against the frame's URL, relative to the document
    /// (`sub/style.css` for `style.css` in `sub/page.html`). For a URL with a fragment
    /// (`icons.svg#check`), a loader that returns `None` is asked again without the fragment.
    /// A render asks once for each URL, whatever the answer.
    fn load(&self, url: &str) -> Option<Vec<u8>>;

    /// The data at each of `urls`, in their order: what [`Self::load`] answers for each. A render
    /// asks with it for the resources it knows it needs before it uses them, at most
    /// [`BATCH`](crate::BATCH) at a time: the linked style sheets of a document, then the fonts
    /// of its `@font-face` rules, then its images. Implement it to fetch them concurrently. The
    /// default calls [`Self::load`] for one after the other, so a loader without it sees the URLs
    /// it saw before. An answer shorter than `urls` leaves the rest not found; what is not found
    /// with a fragment is asked for again without it, as [`Self::load`] is. Resources that only
    /// show while others are read (`@import`, a second source of a font whose first did not
    /// load, the sprites of inline SVG, the documents of `<iframe>`s) are asked for with
    /// [`Self::load`].
    fn load_many(&self, urls: &[&str]) -> Vec<Option<Vec<u8>>> {
        urls.iter()
            .map(|url| {
                let data = self.load(url);
                // (The time limit of a render counts while its loader works.)
                fagus_dom::budget::check_time_now();
                data
            })
            .collect()
    }
}

/// The most URLs a render asks a [`ResourceLoader`] for at once (see
/// [`ResourceLoader::load_many`]); a document that needs more asks again.
pub const BATCH: usize = 64;

/// The data of a `data:` URL (RFC 2397), base64 or percent-encoded.
pub(crate) fn data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:").or_else(|| {
        url.get(..5)
            .filter(|scheme| scheme.eq_ignore_ascii_case("data:"))
            .map(|_| &url[5..])
    })?;
    let (header, data) = rest.split_once(',')?;
    if header
        .rsplit(';')
        .next()
        .is_some_and(|last| last.trim().eq_ignore_ascii_case("base64"))
    {
        base64(&percent_decode(data))
    } else {
        Some(percent_decode(data))
    }
}

fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = input.get(index + 1..index + 3)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            output.push(value);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    output
}

/// Decodes base64 (standard or URL alphabet), ignoring white space; padding is optional.
fn base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for &byte in input {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

/// Reads the file that a URL names, if it lies inside `root`: a relative URL resolves against
/// `base`, a path from the root (`/images/logo.png`) against `root`, as on a web server. URLs
/// with a scheme or a host (`//`) and anything that leads outside `root` (`..`, symbolic links)
/// are refused, also where percent-encoding hides them: an absolute path, a drive or a network
/// share (`%5C%5Chost%5Cshare` on Windows) is refused before the file system is asked about it.
#[cfg(test)]
pub(crate) fn read_inside(base: &Path, root: &Path, url: &str) -> Option<Vec<u8>> {
    std::fs::read(file_inside(base, root, url)?).ok()
}

/// The file that a URL names, if it lies inside `root` (see [`read_inside`]).
pub(crate) fn file_inside(base: &Path, root: &Path, url: &str) -> Option<PathBuf> {
    let path = url.split(['?', '#']).next()?;
    if path.is_empty() || path.contains(':') || path.starts_with("//") || path.starts_with('\\') {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let decoded = String::from_utf8(percent_decode(path)).ok()?;
    let relative = decoded.strip_prefix('/').unwrap_or(&decoded);
    if relative.contains('\0')
        || Path::new(relative).has_root()
        || Path::new(relative)
            .components()
            .any(|component| matches!(component, std::path::Component::Prefix(_)))
    {
        return None;
    }
    let file = match decoded.strip_prefix('/') {
        Some(from_root) => root.join(from_root),
        None => base.join(&decoded),
    }
    .canonicalize()
    .ok()?;
    if !file.starts_with(&root) || !file.is_file() {
        return None;
    }
    Some(file)
}

/// The resources one render has loaded, each content once: what a document loads under many
/// URLs (`logo.png?1`, `logo.png?2`, …, or the same bytes from the caller under several names)
/// is held once, with one address, so that it is decoded and embedded once too, each file is
/// read once, and the caller's loader is asked once for each URL, whatever it answers. The bytes
/// are charged to the budget of the render (see [`fagus_dom::budget`]) once.
#[derive(Debug, Default)]
pub(crate) struct Loaded {
    held: Mutex<Held>,
    files: Mutex<FxHashMap<PathBuf, Arc<Vec<u8>>>>,
    asked: Mutex<FxHashMap<String, Option<Arc<Vec<u8>>>>>,
}

/// The data loaded, by its hash and length, and the addresses of the data held.
#[derive(Debug, Default)]
struct Held {
    by_content: FxHashMap<(u64, usize), Vec<Arc<Vec<u8>>>>,
    addresses: FxHashSet<usize>,
}

impl Loaded {
    /// `data`, or the same content loaded before.
    pub(crate) fn held_once(&self, data: Arc<Vec<u8>>) -> Arc<Vec<u8>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        // Held already (a file read before, a resource handed over).
        if held.addresses.contains(&(Arc::as_ptr(&data) as usize)) {
            return data;
        }
        let mut hasher = FxHasher::default();
        data.hash(&mut hasher);
        let same = held.by_content.entry((hasher.finish(), data.len())).or_default();
        if let Some(known) = same.iter().find(|known| ***known == *data) {
            return Arc::clone(known);
        }
        same.push(Arc::clone(&data));
        held.addresses.insert(Arc::as_ptr(&data) as usize);
        drop(held);
        fagus_dom::budget::charge(fagus_dom::budget::Kind::Resources, data.len() as u64);
        data
    }

    /// What the caller's loader answered for `url`, asked with `ask` once in a render: a
    /// document is laid out again for container queries and to shrink it to the page, and names
    /// a URL in many places.
    pub(crate) fn asked(&self, url: &str, ask: impl FnOnce() -> Option<Vec<u8>>) -> Option<Arc<Vec<u8>>> {
        if let Some(answer) = self.asked.lock().unwrap_or_else(PoisonError::into_inner).get(url) {
            return answer.clone();
        }
        // (Not while the answers are locked: the loader may take its time.)
        let answer = ask().map(Arc::new);
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url.to_owned(), answer.clone());
        answer
    }

    /// Whether the caller's loader was asked for `url` in this render.
    pub(crate) fn was_asked(&self, url: &str) -> bool {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(url)
    }

    /// Keeps what the caller's loader answered for `urls`, asked for together: `answers` in
    /// their order, not found where there are fewer. Returns whether each was found.
    pub(crate) fn answered(&self, urls: &[&str], answers: Vec<Option<Vec<u8>>>) -> Vec<bool> {
        let mut answers = answers.into_iter();
        urls.iter()
            .map(|url| {
                // Held, and charged to the budget, as it arrives.
                let answer = answers
                    .next()
                    .flatten()
                    .map(|data| self.held_once(Arc::new(data)));
                let found = answer.is_some();
                self.asked
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry((*url).to_owned())
                    .or_insert(answer);
                found
            })
            .collect()
    }

    /// The data of `file`, read once.
    pub(crate) fn file(&self, file: PathBuf) -> Option<Arc<Vec<u8>>> {
        if let Some(data) = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&file)
        {
            return Some(Arc::clone(data));
        }
        // A file larger than what is left of the budget is not read.
        if let Ok(metadata) = std::fs::metadata(&file) {
            fagus_dom::budget::ensure_room(fagus_dom::budget::Kind::Resources, metadata.len());
        }
        let data = self.held_once(Arc::new(std::fs::read(&file).ok()?));
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(file, Arc::clone(&data));
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_data_urls() {
        assert_eq!(
            data_url("data:text/plain;base64,SGFsbG8=").as_deref(),
            Some(&b"Hallo"[..])
        );
        assert_eq!(
            data_url("data:,Hallo%20Welt").as_deref(),
            Some(&b"Hallo Welt"[..])
        );
        assert_eq!(
            data_url("DATA:image/png;base64,AAEC").as_deref(),
            Some(&[0, 1, 2][..])
        );
        assert_eq!(data_url("https://example.com/a.png"), None);
        assert_eq!(data_url("data:;base64,@@"), None);
    }

    #[test]
    fn stays_inside_the_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
        let base = root.join("corpus");
        assert!(read_inside(&root, &root, "fonts/Inter-LICENSE.txt").is_some());
        assert!(read_inside(&root, &root, "fonts/Inter-LICENSE.txt?v=2").is_some());
        assert!(
            read_inside(&base, &root, "../fonts/Inter-LICENSE.txt").is_some(),
            "up, but inside the root"
        );
        assert!(
            read_inside(&base, &base, "../fonts/Inter-LICENSE.txt").is_none(),
            "outside the root"
        );
        assert!(read_inside(&root, &root, "../Cargo.toml").is_none(), "outside");
        assert!(
            read_inside(&base, &root, "/fonts/Inter-LICENSE.txt").is_some(),
            "from the root"
        );
        assert!(
            read_inside(&root, &root, "/etc/hosts").is_none(),
            "not inside the root"
        );
        assert!(
            read_inside(&root, &root, "//example.com/a.png").is_none(),
            "a host"
        );
        assert!(
            read_inside(&root, &root, "/../Cargo.toml").is_none(),
            "above the root"
        );
        assert!(read_inside(&root, &root, "file:///etc/hosts").is_none(), "scheme");
        assert!(read_inside(&root, &root, "fonts").is_none(), "directory");
        assert!(
            read_inside(&root, &root, "/%2Fetc/hosts").is_none(),
            "an absolute path, encoded"
        );
        assert!(
            read_inside(&root, &root, "/%2Ffonts/Inter-LICENSE.txt").is_none(),
            "an encoded slash makes no path from the root"
        );
        assert!(read_inside(&root, &root, "fonts/%00").is_none(), "NUL");
        #[cfg(windows)]
        for url in [
            "%5C%5Chost%5Cshare%5Cfile.png",
            "/%5C%5Chost%5Cshare%5Cfile.png",
            "%5Cfonts%5CInter-LICENSE.txt",
            "C%3A%5CWindows%5Cwin.ini",
            "/C%3A/Windows/win.ini",
        ] {
            assert!(read_inside(&root, &root, url).is_none(), "{url}");
        }
    }
}
