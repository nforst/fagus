//! The fonts installed on the system, found where Chrome finds them and loaded only when a
//! document uses them.
//!
//! Scanning reads the names and styles of the faces from memory-mapped files (with fontdb), which
//! takes a few milliseconds per hundred files; a face is parsed, and its file read, the first time
//! a family that it belongs to is asked for or fallback needs one of its characters.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use rustc_hash::FxHashMap;

use crate::{Face, Outlines, make_face};

/// The installed fonts: an index of their faces by family, with the faces parsed on first use.
pub(crate) struct SystemFonts {
    index: fontdb::Database,
    entries: Vec<Entry>,
    /// The entries of each family, by the family's name in lower case.
    families: FxHashMap<String, Vec<u32>>,
    /// The data of the files read so far, shared by the faces of a collection.
    files: Mutex<FxHashMap<PathBuf, Arc<Vec<u8>>>>,
    /// The first visible entry with a glyph for a character, as scanning found it.
    coverage: Mutex<FxHashMap<char, Option<u32>>>,
    /// The visible entry with a PostScript or full name (in lower case), as [`Self::unique`]
    /// found it: finding a name the system does not have reads the names of all faces.
    unique: Mutex<FxHashMap<String, Option<u32>>>,
    /// The families of faces passed over because Fagus cannot draw their glyphs, not yet
    /// reported.
    skipped: Mutex<Vec<String>>,
}

/// A face of the index.
struct Entry {
    id: fontdb::ID,
    family: String,
    /// Whether the family is one the system hides from documents (its name starts with a dot,
    /// as macOS names the fonts of its user interface). Only the generic families reach it.
    hidden: bool,
    /// The weight the system gives the face where it differs from the font's own (see
    /// [`system_weight`]), for fallback.
    weight: Option<f32>,
    face: OnceLock<Option<Arc<Face>>>,
}

/// The weight that Core Text gives a face numbered by weight, as Hiragino's are (`HiraginoSans-W3`
/// …), which their `OS/2` tables put a step lower: W3 is the regular face, W6 the bold one. Chrome
/// matches CSS weights against these where it falls back to the face; where a document names
/// its family, against weights like those of the `OS/2` table.
fn system_weight(post_script_name: &str) -> Option<f32> {
    let (_, number) = post_script_name.rsplit_once("-W")?;
    let number: u8 = number.parse().ok()?;
    Some(match number {
        0..=8 => f32::from(number + 1) * 100.0,
        _ => 900.0,
    })
}

impl std::fmt::Debug for SystemFonts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemFonts")
            .field("faces", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// The directories with the system's fonts on macOS, in the order Core Text searches them.
#[cfg(target_os = "macos")]
fn macos_directories() -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = [
        "/System/Library/Fonts",
        // PingFang, the system's Chinese font, lives with the fonts of the user interface.
        "/System/Library/PrivateFrameworks/FontServices.framework/Resources/Reserved",
        "/Library/Fonts",
        "/Network/Library/Fonts",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Fonts downloaded on demand.
    if let Ok(assets) = std::fs::read_dir("/System/Library/AssetsV2") {
        let mut downloaded: Vec<PathBuf> = assets
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("com_apple_MobileAsset_Font"))
            })
            .collect();
        downloaded.sort();
        directories.extend(downloaded);
    }
    if let Some(home) = std::env::var_os("HOME") {
        directories.push(std::path::Path::new(&home).join("Library/Fonts"));
    }
    directories
}

/// The configuration of fontconfig on FreeBSD, whose packages install it under `/usr/local`
/// rather than in `/etc/fonts`, where fontdb looks for it.
#[cfg(target_os = "freebsd")]
const FREEBSD_FONTCONFIG: &str = "/usr/local/etc/fonts";

/// The existing font directories that the fontconfig configuration in `directory` lists, in its
/// order, each once and none inside one listed before: those of `fonts.conf`, of the files in
/// `conf.d` and of `local.conf`, which `fonts.conf` includes by paths relative to its directory
/// (the parser would take them relative to `/etc/fonts`).
#[cfg(any(target_os = "freebsd", test))]
fn fontconfig_directories(directory: &std::path::Path) -> Vec<PathBuf> {
    let mut files = vec![directory.join("fonts.conf")];
    if let Ok(entries) = std::fs::read_dir(directory.join("conf.d")) {
        let mut included: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "conf"))
            .collect();
        included.sort();
        files.extend(included);
    }
    files.push(directory.join("local.conf"));
    let mut fontconfig = fontconfig_parser::FontConfig::default();
    for file in files {
        // As fontdb does: a file that cannot be read or parsed adds nothing, the others count.
        let _ = fontconfig.merge_config(&file);
    }
    let mut directories: Vec<PathBuf> = Vec::new();
    for listed in fontconfig.dirs {
        let Ok(path) = std::fs::canonicalize(&listed.path) else {
            continue;
        };
        if path.is_dir() && !directories.iter().any(|known| path.starts_with(known)) {
            directories.push(path);
        }
    }
    directories
}

impl SystemFonts {
    /// Scans the fonts of the system: on macOS `/System/Library/Fonts` (with `Supplemental`),
    /// `/Library/Fonts` and `~/Library/Fonts`; on Linux the directories that fontconfig lists
    /// (else `/usr/share/fonts`, `/usr/local/share/fonts` and `~/.local/share/fonts`); on FreeBSD
    /// those that fontconfig lists in `/usr/local/etc/fonts` (else the same as on Linux); on
    /// Windows `%WINDIR%\Fonts` and the fonts installed for the user.
    pub(crate) fn scan() -> Self {
        let mut index = fontdb::Database::new();
        #[cfg(target_os = "macos")]
        for directory in macos_directories() {
            index.load_fonts_dir(directory);
        }
        #[cfg(target_os = "freebsd")]
        {
            // fontdb reads `FONTCONFIG_FILE` and `/etc/fonts` itself.
            let directories = if std::env::var_os("FONTCONFIG_FILE").is_none()
                && !std::path::Path::new("/etc/fonts/fonts.conf").exists()
            {
                fontconfig_directories(std::path::Path::new(FREEBSD_FONTCONFIG))
            } else {
                Vec::new()
            };
            if directories.is_empty() {
                index.load_system_fonts();
            }
            for directory in directories {
                index.load_fonts_dir(directory);
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
        index.load_system_fonts();
        Self::from_index(index, |_| None)
    }

    /// The faces of an index, with the family that `rename` gives a face where it gives one.
    pub(crate) fn from_index(
        index: fontdb::Database,
        rename: impl Fn(&fontdb::FaceInfo) -> Option<String>,
    ) -> Self {
        let mut entries = Vec::new();
        let mut families: FxHashMap<String, Vec<u32>> = FxHashMap::default();
        for info in index.faces() {
            let renamed = rename(info);
            let Some(family) = renamed
                .clone()
                .or_else(|| info.families.first().map(|(name, _)| name.clone()))
            else {
                continue;
            };
            let number = entries.len() as u32;
            // A family is found by its name in every language, as Core Text finds it (ヒラギノ角ゴシック
            // is Hiragino Sans).
            let mut names: Vec<String> = match renamed {
                Some(_) => vec![family.to_lowercase()],
                None => info
                    .families
                    .iter()
                    .map(|(name, _)| name.to_lowercase())
                    .collect(),
            };
            names.dedup();
            for name in names {
                let numbers = families.entry(name).or_default();
                if numbers.last() != Some(&number) {
                    numbers.push(number);
                }
            }
            entries.push(Entry {
                id: info.id,
                hidden: family.starts_with('.') || info.post_script_name.starts_with('.'),
                weight: system_weight(&info.post_script_name),
                family,
                face: OnceLock::new(),
            });
        }
        Self {
            index,
            entries,
            families,
            files: Mutex::default(),
            coverage: Mutex::default(),
            unique: Mutex::default(),
            skipped: Mutex::default(),
        }
    }

    /// The number of faces.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The directories that hold the files of the faces, each once and none inside another.
    pub(crate) fn directories(&self) -> Vec<PathBuf> {
        let mut directories: Vec<PathBuf> = self
            .index
            .faces()
            .filter_map(|info| match &info.source {
                fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                    path.parent().map(std::path::Path::to_path_buf)
                }
                fontdb::Source::Binary(_) => None,
            })
            .collect();
        directories.sort();
        directories.dedup();
        // Sorted by their components, the directories inside one follow it.
        let mut outer: Vec<PathBuf> = Vec::new();
        for directory in directories {
            if !outer.last().is_some_and(|last| directory.starts_with(last)) {
                outer.push(directory);
            }
        }
        outer
    }

    /// The names [`Self::unique`] looked for so far, with the entries it found.
    #[cfg(test)]
    pub(crate) fn names_looked_for(&self) -> FxHashMap<String, Option<u32>> {
        self.unique
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The number of faces parsed so far.
    #[cfg(test)]
    pub(crate) fn loaded(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.face.get().is_some())
            .count()
    }

    /// Whether the system has a family (by its name in any case), hidden ones only where
    /// `hidden` allows them.
    pub(crate) fn has_family(&self, family: &str, hidden: bool) -> bool {
        self.entries_of(family, hidden).next().is_some()
    }

    fn entries_of(&self, family: &str, hidden: bool) -> impl Iterator<Item = u32> + '_ {
        self.families
            .get(&family.to_lowercase())
            .into_iter()
            .flatten()
            .copied()
            .filter(move |number| hidden || !self.entries[*number as usize].hidden)
    }

    /// The faces of a family, parsed now if they were not yet; faces that do not parse are left
    /// out.
    pub(crate) fn family(&self, family: &str, hidden: bool) -> Vec<(u32, &Face)> {
        self.entries_of(family, hidden)
            .filter_map(|number| self.get(number).map(|face| (number, face)))
            .collect()
    }

    /// Face `number`, parsed on first use; `None` if it does not parse or Fagus cannot draw its
    /// glyphs (see [`Self::skip`]).
    pub(crate) fn get(&self, number: u32) -> Option<&Face> {
        let entry = self.entries.get(number as usize)?;
        entry
            .face
            .get_or_init(|| self.load(entry).map(Arc::new))
            .as_deref()
    }

    /// The face of a number that [`Self::family`] or [`Self::covering`] gave out.
    pub(crate) fn loaded_face(&self, number: u32) -> Option<&Face> {
        self.entries.get(number as usize)?.face.get()?.as_deref()
    }

    fn load(&self, entry: &Entry) -> Option<Face> {
        let (data, index) = self.data(entry.id)?;
        if !crate::face_sound(&data, index) {
            self.skip(&entry.family);
            return None;
        }
        let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(&data) as _;
        let font = harfrust::Font::new(blob, index)?;
        let mut face = make_face(font, data, index);
        if face.outlines == Outlines::None {
            self.skip(&entry.family);
            return None;
        }
        face.family.clone_from(&entry.family);
        if face.axes.is_empty() {
            face.cascade_weight = entry.weight;
        }
        Some(face)
    }

    /// The data of the file of a face, read once, with the face's index in it.
    fn data(&self, id: fontdb::ID) -> Option<(Arc<Vec<u8>>, u32)> {
        let (source, index) = self.index.face_source(id)?;
        let data = match source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                let mut files = self
                    .files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(data) = files.get(&path) {
                    Arc::clone(data)
                } else {
                    let data = Arc::new(std::fs::read(&path).ok()?);
                    files.insert(path, Arc::clone(&data));
                    data
                }
            }
            fontdb::Source::Binary(_) => Arc::new(self.index.with_face_data(id, |data, _| data.to_vec())?),
        };
        Some((data, index))
    }

    /// The data of the installed face whose PostScript name or full name is `name` (in any
    /// case), as `local()` in `@font-face` finds it in Chrome, with the face's index in it.
    /// Hidden faces are not found.
    pub(crate) fn unique(&self, name: &str) -> Option<(Arc<Vec<u8>>, u32)> {
        let key = name.to_ascii_lowercase();
        let known = self
            .unique
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .copied();
        let number = known.unwrap_or_else(|| {
            let found = self.find_unique(name);
            self.unique
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, found);
            found
        })?;
        self.data(self.entries.get(number as usize)?.id)
    }

    /// The first visible entry whose PostScript name is `name`, else the first whose full name
    /// is (in any case).
    fn find_unique(&self, name: &str) -> Option<u32> {
        let visible = || self.entries.iter().enumerate().filter(|(_, entry)| !entry.hidden);
        let by_post_script = visible().find(|(_, entry)| {
            self.index
                .face(entry.id)
                .is_some_and(|info| info.post_script_name.eq_ignore_ascii_case(name))
        });
        let (number, _) = by_post_script.or_else(|| {
            visible().find(|(_, entry)| {
                self.index
                    .with_face_data(entry.id, |data, index| crate::full_names(data, index, name))
                    .unwrap_or(false)
            })
        })?;
        Some(number as u32)
    }

    /// The first face, in the order of the scan, that is not hidden and has a glyph for
    /// `character`; the faces are not parsed for this, their character maps are read where they
    /// lie. Remembered for each character.
    pub(crate) fn covering(&self, character: char) -> Option<(u32, &Face)> {
        let cached = self
            .coverage
            .lock()
            .ok()
            .and_then(|coverage| coverage.get(&character).copied());
        let number = if let Some(number) = cached {
            number
        } else {
            let found = self.scan_for(character);
            if let Ok(mut coverage) = self.coverage.lock() {
                coverage.insert(character, found);
            }
            found
        }?;
        self.get(number).map(|face| (number, face))
    }

    /// The first face that is not hidden and draws `character`: faces with a glyph for it that
    /// Fagus cannot draw (see [`Self::skip`]) are passed over.
    fn scan_for(&self, character: char) -> Option<u32> {
        use skrifa::MetadataProvider;
        self.entries.iter().enumerate().find_map(|(number, entry)| {
            if entry.hidden {
                return None;
            }
            let has = match entry.face.get() {
                Some(face) => face.as_ref().is_some_and(|face| face.has_glyph(character)),
                None => self
                    .index
                    .with_face_data(entry.id, |data, index| {
                        let Ok(font) = skrifa::FontRef::from_index(data, index) else {
                            return false;
                        };
                        if font.charmap().map(character).is_none() {
                            false
                        } else if Outlines::of(&font) == Outlines::None {
                            self.skip(&entry.family);
                            false
                        } else {
                            true
                        }
                    })
                    .unwrap_or(false),
            };
            has.then_some(number as u32)
        })
    }

    /// Notes that a face of `family` is passed over because Fagus cannot draw its glyphs (it
    /// has no outlines, color glyphs or images that Fagus reads, as Apple's bitmap fonts in
    /// `bdat` tables), so that the next face takes its place in matching and fallback.
    fn skip(&self, family: &str) {
        let mut skipped = self
            .skipped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !skipped.iter().any(|known| known == family) {
            skipped.push(family.to_owned());
        }
    }

    /// The families of the faces passed over since the last call, because Fagus cannot draw
    /// their glyphs.
    pub(crate) fn take_skipped(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .skipped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{fontconfig_directories, system_weight};

    /// A configuration as FreeBSD's packages install it: directories in `fonts.conf`, in the
    /// files of `conf.d` and in `local.conf`, one of them listed twice, one inside another, one
    /// missing.
    #[test]
    fn reads_the_directories_of_a_fontconfig_configuration() {
        let root = std::env::temp_dir().join(format!("fagus-fontconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for directory in ["etc/conf.d", "share/fonts/TTF", "lib/fonts", "local", "other"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let root = std::fs::canonicalize(&root).unwrap();
        let config = |directories: &[&str]| {
            let directories = directories
                .iter()
                .map(|path| format!("<dir>{}</dir>", root.join(path).display()))
                .collect::<Vec<_>>()
                .concat();
            format!(
                "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
                 <fontconfig>{directories}<include ignore_missing=\"yes\">conf.d</include></fontconfig>"
            )
        };
        let write = |file: &str, directories: &[&str]| {
            std::fs::write(root.join("etc").join(file), config(directories)).unwrap();
        };
        write("fonts.conf", &["share/fonts", "missing"]);
        write("conf.d/20-more.conf", &["share/fonts/TTF", "share/fonts"]);
        write("conf.d/10-more.conf", &["lib/fonts"]);
        write("conf.d/README", &["other"]);
        write("local.conf", &["local"]);
        let directories = fontconfig_directories(&root.join("etc"));
        let none = fontconfig_directories(&root.join("none"));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            directories,
            [
                root.join("share/fonts"),
                root.join("lib/fonts"),
                root.join("local")
            ]
        );
        assert_eq!(none, Vec::<std::path::PathBuf>::new());
    }

    #[test]
    fn reads_weights_from_numbered_names() {
        assert_eq!(system_weight("HiraginoSans-W3"), Some(400.0));
        assert_eq!(system_weight("HiraKakuProN-W6"), Some(700.0));
        assert_eq!(system_weight("HiraginoSans-W9"), Some(900.0));
        assert_eq!(system_weight("Helvetica-Bold"), None);
        assert_eq!(system_weight("Inter-Weird"), None);
    }
}
