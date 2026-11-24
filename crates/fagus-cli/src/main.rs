//! The `fagus` command line tool.

mod date;
mod layout_json;
mod memory;
mod request;
mod sandbox;
mod worker;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fagus", version, about = "HTML and CSS to PDF, without a browser")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant, reason = "parsed once per run")]
enum Command {
    /// Renders an HTML file to PDF.
    Render {
        /// The HTML file.
        input: PathBuf,
        /// The PDF file to write.
        #[arg(short, long)]
        output: PathBuf,
        /// Directories with font files (repeatable).
        #[arg(long = "font-dir")]
        font_dirs: Vec<PathBuf>,
        /// Use the fonts installed on the system too, as Chrome does (the output then depends
        /// on the machine).
        #[arg(long)]
        system_fonts: bool,
        /// The language of the texts form controls show of their own (button labels, date
        /// formats) where the document gives them no `lang`, e.g. `de-DE` (default: en-US).
        /// Text without `lang` is then shaped in it, as Chrome shapes it in the language of its
        /// user interface.
        #[arg(long)]
        locale: Option<String>,
        /// Family used for `sans-serif`.
        #[arg(long)]
        sans_serif: Option<String>,
        /// Family used for `serif`.
        #[arg(long)]
        serif: Option<String>,
        /// Family used for `monospace`.
        #[arg(long)]
        monospace: Option<String>,
        /// Directory that the document may read files from (default: the input's directory).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Write PDF/A: 1b, 2b, 2u, 2a, 3b, 3u, 3a, 4, 4e or 4f.
        #[arg(long, value_parser = ["1b", "2b", "2u", "2a", "3b", "3u", "3a", "4", "4e", "4f"])]
        pdfa: Option<String>,
        /// Write a PDF without tags (the logical structure that screen readers read).
        #[arg(long)]
        untagged: bool,
        /// Write PDF/UA-1, the standard for accessible PDF (tagged; needs a title, a language and
        /// alternative text for images).
        #[arg(long)]
        accessible: bool,
        /// Make the form controls fields of an interactive form, which readers fill in.
        #[arg(long)]
        forms: bool,
        /// Write opaque images as JPEGs of this quality, from 1 (smallest) to 100 (best); images
        /// with transparent pixels, and those that are smaller without loss, stay as they are.
        #[arg(long, value_name = "1-100", value_parser = clap::value_parser!(u8).range(1..=100))]
        image_quality: Option<u8>,
        /// Attach a file (repeatable).
        #[arg(long = "attach")]
        attachments: Vec<PathBuf>,
        /// Make an electronic invoice (ZUGFeRD 2 / Factur-X) with this XML.
        #[arg(long)]
        factur_x: Option<PathBuf>,
        /// The profile of the invoice XML (default: the one the XML declares).
        #[arg(long, value_parser = ["minimum", "basicwl", "basic", "en16931", "extended", "xrechnung"])]
        profile: Option<String>,
        /// The `Version` of the Factur-X XMP metadata (default: 1.0).
        #[arg(long)]
        factur_x_version: Option<String>,
        /// The description of the attached invoice XML (default: "Factur-X/ZUGFeRD invoice").
        #[arg(long)]
        factur_x_description: Option<String>,
        /// Fail on any diagnostic (CSS that is ignored, a missing resource, …), without writing
        /// the PDF.
        #[arg(long)]
        strict: bool,
        /// Write where the elements with an `id` ended up (their pages and boxes, and the rows of
        /// their tables) to this file as JSON.
        #[arg(long)]
        layout_json: Option<PathBuf>,
        /// Print the time of each phase.
        #[arg(long)]
        timings: bool,
        /// Render this many times (for benchmarks) and report the median.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// The creation date written into the PDF, e.g. `2026-11-13T12:00:00Z` or
        /// `2026-11-13T14:00:00+02:00` (default: now), so that the same input gives the same file.
        #[arg(long)]
        created: Option<String>,
        #[command(flatten)]
        encryption: EncryptionArgs,
        #[command(flatten)]
        sign: Sign,
        #[command(flatten)]
        limits: LimitArgs,
        #[command(flatten)]
        confinement: Confinement,
    },
    /// Renders documents for another program over standard input and output (docs/protocol.md).
    Worker {
        /// Directories with font files (repeatable).
        #[arg(long = "font-dir")]
        font_dirs: Vec<PathBuf>,
        /// Use the fonts installed on the system too, as Chrome does (the output then depends
        /// on the machine).
        #[arg(long)]
        system_fonts: bool,
        /// The language of the texts form controls show of their own (button labels, date
        /// formats) where the document gives them no `lang`, e.g. `de-DE` (default: en-US).
        /// Text without `lang` is then shaped in it, as Chrome shapes it in the language of its
        /// user interface.
        #[arg(long)]
        locale: Option<String>,
        /// Family used for `sans-serif`.
        #[arg(long)]
        sans_serif: Option<String>,
        /// Family used for `serif`.
        #[arg(long)]
        serif: Option<String>,
        /// Family used for `monospace`.
        #[arg(long)]
        monospace: Option<String>,
        /// On Linux, let the sandbox read only the files in this directory (repeatable), besides
        /// the fonts; the files of documents outside it cannot be read (default: every file).
        #[arg(long = "allow-read", value_name = "DIR")]
        allow_read: Vec<PathBuf>,
        #[command(flatten)]
        limits: LimitArgs,
        #[command(flatten)]
        confinement: Confinement,
    },
}

/// Password protection. Arguments of a process are visible to other users of the machine, so the
/// passwords can come from files or the environment too.
#[derive(clap::Args)]
struct EncryptionArgs {
    /// Encrypt the PDF (AES-256) with this password, needed to open it [default: the
    /// environment variable `FAGUS_USER_PASSWORD`].
    #[arg(long)]
    user_password: Option<String>,
    /// Read the user password from this file (its first line).
    #[arg(long, value_name = "FILE", conflicts_with = "user_password")]
    user_password_file: Option<PathBuf>,
    /// Encrypt the PDF (AES-256) with this password, which lifts the restrictions of
    /// `--permissions` [default: the environment variable `FAGUS_OWNER_PASSWORD`].
    #[arg(long)]
    owner_password: Option<String>,
    /// Read the owner password from this file (its first line).
    #[arg(long, value_name = "FILE", conflicts_with = "owner_password")]
    owner_password_file: Option<PathBuf>,
    /// What an encrypted PDF allows without the owner password, separated by commas: print,
    /// print-low-quality, modify, copy, annotate, fill-forms, assemble, or none (default:
    /// everything).
    #[arg(
        long,
        value_delimiter = ',',
        value_parser = ["print", "print-low-quality", "modify", "copy", "annotate", "fill-forms", "assemble", "none"]
    )]
    permissions: Option<Vec<String>>,
}

impl EncryptionArgs {
    /// The encryption the arguments ask for, if any, with the passwords of the environment
    /// variables where the command line gives none.
    fn encryption(self) -> Result<Option<fagus::Encryption>> {
        self.encryption_with(|name| std::env::var(name).ok())
    }

    fn encryption_with(
        self,
        environment: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<fagus::Encryption>> {
        let read = |path: Option<PathBuf>| -> Result<Option<String>> {
            let Some(path) = path else { return Ok(None) };
            let text =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            Ok(Some(text.lines().next().unwrap_or_default().to_owned()))
        };
        let user = read(self.user_password_file)?
            .or(self.user_password)
            .or_else(|| environment("FAGUS_USER_PASSWORD"))
            .filter(|password| !password.is_empty());
        let owner = read(self.owner_password_file)?
            .or(self.owner_password)
            .or_else(|| environment("FAGUS_OWNER_PASSWORD"))
            .filter(|password| !password.is_empty());
        if user.is_none() && owner.is_none() {
            if self.permissions.is_some() {
                bail!("--permissions needs --user-password or --owner-password");
            }
            return Ok(None);
        }
        let mut encryption = fagus::Encryption::new();
        if let Some(user) = user {
            encryption = encryption.user_password(user);
        }
        if let Some(owner) = owner {
            encryption = encryption.owner_password(owner);
        }
        if let Some(names) = self.permissions {
            if names.len() > 1 && names.iter().any(|name| name == "none") {
                bail!("--permissions none cannot go with other permissions");
            }
            encryption =
                encryption.permissions(names.iter().filter_map(|name| fagus::Permission::from_name(name)));
        }
        Ok(Some(encryption))
    }
}

/// The memory budget and the time limit of each document (docs/cli.md#limits).
#[derive(clap::Args)]
struct LimitArgs {
    /// The memory budget of a document in MiB: one that needs more fails with `limit.memory`
    /// (default: 2048; 0 for none).
    #[arg(long, value_name = "MIB")]
    memory_budget: Option<u64>,
    /// The time limit of a document in seconds: one that takes longer fails with `limit.time`
    /// (default: none).
    #[arg(long, value_name = "SECONDS", value_parser = seconds)]
    time_limit: Option<std::time::Duration>,
}

impl LimitArgs {
    fn limits(&self) -> fagus::Limits {
        let mut limits = fagus::Limits::new().time(self.time_limit);
        if let Some(mib) = self.memory_budget {
            limits = limits.memory((mib > 0).then(|| mib.saturating_mul(1 << 20)));
        }
        limits
    }
}

/// A positive number of seconds, such as `30` or `2.5`.
fn seconds(value: &str) -> Result<std::time::Duration, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|seconds| *seconds > 0.0)
        .and_then(|seconds| std::time::Duration::try_from_secs_f64(seconds).ok())
        .ok_or_else(|| format!("`{value}` is not a positive number of seconds"))
}

/// The sandbox and the limits of a process (docs/cli.md#sandbox).
#[derive(clap::Args)]
struct Confinement {
    /// Render outside the sandbox that keeps the process on Linux from the network, from
    /// starting programs and from files it does not need (there is none on other systems).
    #[arg(long)]
    no_sandbox: bool,
    /// On Linux, end the process when its memory (address space) would exceed this many MiB.
    #[arg(long, value_name = "MIB", value_parser = clap::value_parser!(u64).range(1..))]
    memory_limit: Option<u64>,
    /// On Linux, end the process after this many seconds of processor time.
    #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    cpu_limit: Option<u64>,
}

impl Confinement {
    /// Applies the limits.
    fn limit(&self) -> Result<()> {
        sandbox::limit(self.memory_limit, self.cpu_limit).context("limiting the process")
    }

    /// Enters the sandbox, unless it is turned off, then starts the threads that write PDFs, so
    /// that they are inside it.
    fn enter(&self, policy: &sandbox::Policy) {
        if self.no_sandbox {
            sandbox::debug("sandbox turned off");
        } else {
            sandbox::enter(policy);
        }
        start_threads();
    }
}

/// The files the worker may read in its sandbox: the files of documents are anywhere the caller
/// says, unless it names directories; then those, the system fonts, and the status of the process
/// for the memory that each answer reports (memory.rs).
fn worker_reads(allow_read: Vec<PathBuf>, fonts: impl IntoIterator<Item = PathBuf>) -> Option<Vec<PathBuf>> {
    (!allow_read.is_empty()).then(|| {
        let mut read = allow_read;
        read.extend(fonts);
        read.push(PathBuf::from("/proc/self/status"));
        read
    })
}

/// The file the PDF goes to. It is opened before rendering, so that the sandbox need not let the
/// process write files.
struct Output {
    file: std::fs::File,
    path: PathBuf,
    /// Whether it was created for the PDF.
    created: bool,
}

impl Output {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        let (file, created) = match options.clone().create_new(true).open(path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (options.open(path)?, false),
            Err(error) => return Err(error),
        };
        Ok(Self {
            file,
            path: path.to_owned(),
            created,
        })
    }

    /// The directory in which the file is removed again if rendering fails.
    fn directory(&self) -> Option<PathBuf> {
        let parent = self.path.parent()?;
        self.created.then(|| {
            if parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                parent.to_owned()
            }
        })
    }

    /// Replaces the content of the file with the PDF.
    fn write(mut self, pdf: &[u8]) -> std::io::Result<()> {
        // Not a pipe or a device, such as /dev/stdout.
        if self.file.metadata()?.is_file() {
            self.file.set_len(0)?;
        }
        self.file.write_all(pdf)?;
        self.file.flush()
    }

    /// Removes the file if it was created for the PDF; a file that was there keeps its content.
    fn discard(self) {
        if self.created {
            drop(self.file);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Options of the digital signature.
#[derive(clap::Args)]
#[allow(
    clippy::struct_field_names,
    reason = "the fields are named like their options"
)]
struct Sign {
    /// Sign the PDF digitally with the key and certificates of this PKCS#12 file (.p12, .pfx).
    /// The password comes from the environment variable `FAGUS_SIGN_PASSWORD` or from
    /// `--sign-password-file`.
    #[arg(long, value_name = "FILE")]
    sign_p12: Option<PathBuf>,
    /// A file whose first line is the password of the PKCS#12 file.
    #[arg(long, value_name = "FILE", requires = "sign_p12")]
    sign_password_file: Option<PathBuf>,
    /// The name of the signer, as the signature shows it.
    #[arg(long, requires = "sign_p12")]
    sign_name: Option<String>,
    /// Why the document is signed.
    #[arg(long, requires = "sign_p12")]
    sign_reason: Option<String>,
    /// Where the document is signed.
    #[arg(long, requires = "sign_p12")]
    sign_location: Option<String>,
    /// How to reach the signer.
    #[arg(long, requires = "sign_p12")]
    sign_contact: Option<String>,
    /// Certify the PDF: the signature says what later changes it allows.
    #[arg(long, value_name = "CHANGES", requires = "sign_p12", value_parser = ["no-changes", "fill-forms", "annotate"])]
    sign_certify: Option<String>,
    /// Make the signature visible in the box of the element with this id: the element's
    /// content becomes the appearance of the signature field.
    #[arg(long, value_name = "ID", requires = "sign_p12")]
    sign_element: Option<String>,
}

impl Sign {
    /// The signature these options ask for, if any.
    fn signature(self) -> Result<Option<fagus::Signature>> {
        let Some(path) = self.sign_p12 else {
            return Ok(None);
        };
        let password = match &self.sign_password_file {
            Some(file) => std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned(),
            None => std::env::var("FAGUS_SIGN_PASSWORD")
                .context("--sign-p12 needs the password in FAGUS_SIGN_PASSWORD or --sign-password-file")?,
        };
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut signature = fagus::Signature::pkcs12(&data, &password)?;
        if let Some(name) = self.sign_name {
            signature = signature.name(name);
        }
        if let Some(reason) = self.sign_reason {
            signature = signature.reason(reason);
        }
        if let Some(location) = self.sign_location {
            signature = signature.location(location);
        }
        if let Some(contact) = self.sign_contact {
            signature = signature.contact_info(contact);
        }
        if let Some(certification) = self
            .sign_certify
            .as_deref()
            .and_then(fagus::Certification::from_name)
        {
            signature = signature.certify(certification);
        }
        if let Some(id) = self.sign_element {
            signature = signature.visible_on(id);
        }
        Ok(Some(signature))
    }
}

/// The message of `--strict` when the document has `count` diagnostics.
fn strict_failure(count: usize) -> String {
    let noun = if count == 1 { "diagnostic" } else { "diagnostics" };
    format!("{count} {noun} in strict mode")
}

/// A MIME type from a file name's extension.
fn mime_type(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("xml") => "text/xml",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// How many threads compress the PDF while it is written, unless `RAYON_NUM_THREADS` says
/// otherwise. More than one gains a few percent on large documents (the page contents are
/// compressed while the next pages are drawn); more than two gain nothing, but cost processor
/// time and threads, which add up in many workers side by side.
#[cfg(feature = "parallel")]
const PDF_THREADS: usize = 2;

/// Starts the threads that write PDFs.
fn start_threads() {
    #[cfg(feature = "parallel")]
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        // Fails only where the pool exists already, which then serves as it is.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(PDF_THREADS)
            .build_global();
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Render {
            input,
            output,
            font_dirs,
            system_fonts,
            locale,
            sans_serif,
            serif,
            monospace,
            root,
            pdfa,
            untagged,
            accessible,
            forms,
            image_quality,
            attachments,
            factur_x,
            profile,
            factur_x_version,
            factur_x_description,
            strict,
            layout_json,
            timings,
            repeat,
            created,
            encryption,
            sign,
            limits,
            confinement,
        } => {
            confinement.limit()?;
            let html =
                std::fs::read_to_string(&input).with_context(|| format!("reading {}", input.display()))?;
            let mut engine = fagus::Engine::new();
            engine.set_limits(limits.limits());
            for directory in &font_dirs {
                engine
                    .add_font_dir(directory)
                    .with_context(|| format!("reading fonts in {}", directory.display()))?;
            }
            if system_fonts {
                engine.add_system_fonts();
            }
            engine.set_locale(locale);
            if sans_serif.is_some() || serif.is_some() || monospace.is_some() {
                engine.set_generic_families(sans_serif, serif, monospace);
            }
            // A bare file name has an empty parent: its directory is the current one.
            let base = input
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map_or_else(|| PathBuf::from("."), PathBuf::from);
            // As browsers do for files, the extension says whether the markup is XHTML.
            let xhtml = input
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("xhtml") || extension.eq_ignore_ascii_case("xht")
                });
            let document = if xhtml {
                fagus::Document::xhtml(html)
            } else {
                fagus::Document::html(html)
            };
            // The directory the document reads its files from.
            let readable = root.clone().unwrap_or_else(|| base.clone());
            let mut document = document.base_directory(base);
            if let Some(root) = root {
                document = document.root(root);
            }
            if let Some(created) = created {
                let Some(date) = request::date(&created) else {
                    bail!(
                        "invalid date `{created}`, expected e.g. 2026-11-13T12:00:00Z or 2026-11-13T14:00:00+02:00"
                    );
                };
                document = document.created(date);
            }
            if let Some(level) = pdfa.as_deref().and_then(fagus::Conformance::from_level) {
                document = document.conformance(level);
            }
            document = document.tagged(!untagged).forms(forms);
            if let Some(quality) = image_quality {
                document = document.image_quality(quality);
            }
            if accessible {
                document = document.accessible();
            }
            for path in &attachments {
                let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                let name = path.file_name().map_or_else(
                    || "attachment".to_owned(),
                    |name| name.to_string_lossy().into_owned(),
                );
                document = document.attach(
                    fagus::Attachment::new(name.clone(), mime_type(&name), data)
                        .description(name)
                        .relationship(fagus::Relationship::Supplement),
                );
            }
            if let Some(path) = factur_x {
                let xml = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                document = match profile.as_deref().and_then(request::profile) {
                    None => document.factur_x(xml),
                    Some(profile) => document.factur_x_profile(xml, profile),
                };
                if let Some(version) = factur_x_version {
                    document = document.factur_x_version(version);
                }
                if let Some(description) = factur_x_description {
                    document = document.factur_x_description(description);
                }
            }
            if layout_json.is_some() {
                document = document.report_layout(true);
            }
            // Before the sandbox: the password files may be anywhere.
            if let Some(encryption) = encryption.encryption()? {
                document = document.encrypt(encryption);
            }
            // Before the sandbox: the PKCS#12 file and its password file may be anywhere.
            if let Some(signature) = sign.signature()? {
                document = document.sign(signature);
            }
            // Both outputs are opened before the sandbox, which does not let the process create
            // files.
            let path = output;
            let output = Output::open(&path).with_context(|| format!("writing {}", path.display()))?;
            let layout_output = match layout_json.as_deref().map(Output::open).transpose() {
                Ok(layout_output) => layout_output,
                Err(error) => {
                    output.discard();
                    let layout_path = layout_json.as_deref().unwrap_or(&path);
                    return Err(error).with_context(|| format!("writing {}", layout_path.display()));
                }
            };
            let mut read = vec![readable];
            read.extend(engine.system_font_directories());
            confinement.enter(&sandbox::Policy {
                read: Some(read),
                remove: output
                    .directory()
                    .into_iter()
                    .chain(layout_output.as_ref().and_then(Output::directory))
                    .collect(),
            });
            let mut durations = Vec::with_capacity(repeat);
            let mut result = None;
            for _ in 0..repeat.max(1) {
                let start = std::time::Instant::now();
                match engine.render_with_diagnostics(&document) {
                    Ok(rendered) => result = Some(rendered),
                    Err(failure) => {
                        // The diagnostics found before the failure often explain it.
                        for diagnostic in &failure.diagnostics {
                            eprintln!("{diagnostic}");
                        }
                        output.discard();
                        if let Some(layout_output) = layout_output {
                            layout_output.discard();
                        }
                        return Err(failure.error.into());
                    }
                }
                durations.push(start.elapsed());
            }
            let rendered = result.expect("rendered at least once");
            for diagnostic in &rendered.diagnostics {
                eprintln!("{diagnostic}");
            }
            if strict && !rendered.diagnostics.is_empty() {
                output.discard();
                if let Some(layout_output) = layout_output {
                    layout_output.discard();
                }
                bail!("{}", strict_failure(rendered.diagnostics.len()));
            }
            output
                .write(&rendered.pdf)
                .with_context(|| format!("writing {}", path.display()))?;
            if let (Some(layout_output), Some(layout)) = (layout_output, &rendered.layout) {
                let mut json = serde_json::to_string_pretty(&layout_json::layout_json(layout))?;
                json.push('\n');
                let layout_path = layout_output.path.clone();
                layout_output
                    .write(json.as_bytes())
                    .with_context(|| format!("writing {}", layout_path.display()))?;
            }
            if timings {
                durations.sort();
                let t = rendered.timings;
                eprintln!(
                    "{} pages, median {:.2} ms (parse {:.2}, style {:.2}, layout {:.2}, paint {:.2}, pdf {:.2})",
                    rendered.pages,
                    durations[durations.len() / 2].as_secs_f64() * 1000.0,
                    t.parse.as_secs_f64() * 1000.0,
                    t.style.as_secs_f64() * 1000.0,
                    t.layout.as_secs_f64() * 1000.0,
                    t.paint.as_secs_f64() * 1000.0,
                    t.pdf.as_secs_f64() * 1000.0,
                );
            }
            Ok(())
        }
        Command::Worker {
            font_dirs,
            system_fonts,
            locale,
            sans_serif,
            serif,
            monospace,
            allow_read,
            limits,
            confinement,
        } => {
            confinement.limit()?;
            let mut engine = fagus::Engine::new();
            engine.set_limits(limits.limits());
            for directory in &font_dirs {
                engine
                    .add_font_dir(directory)
                    .with_context(|| format!("reading fonts in {}", directory.display()))?;
            }
            if system_fonts {
                engine.add_system_fonts();
            }
            engine.set_locale(locale);
            if sans_serif.is_some() || serif.is_some() || monospace.is_some() {
                engine.set_generic_families(sans_serif, serif, monospace);
            }
            let read = worker_reads(allow_read, engine.system_font_directories());
            confinement.enter(&sandbox::Policy {
                read,
                remove: Vec::new(),
            });
            let output = std::io::BufWriter::new(std::io::stdout());
            worker::serve(engine, std::io::stdin(), output).context("serving the caller")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_limits_of_documents() {
        let limits = |arguments: &[&str]| {
            let cli = Cli::try_parse_from(["fagus", "worker"].iter().chain(arguments)).expect("parsed");
            let Command::Worker { limits, .. } = cli.command else {
                panic!("not worker");
            };
            limits.limits()
        };
        assert_eq!(limits(&[]), fagus::Limits::new());
        let given = limits(&["--memory-budget", "512", "--time-limit", "2.5"]);
        assert_eq!(given.memory_budget(), Some(512 << 20));
        assert_eq!(given.time_limit(), Some(std::time::Duration::from_millis(2500)));
        assert_eq!(limits(&["--memory-budget", "0"]).memory_budget(), None);
        assert!(Cli::try_parse_from(["fagus", "worker", "--time-limit", "0"]).is_err());
        assert!(Cli::try_parse_from(["fagus", "worker", "--time-limit", "-1"]).is_err());
    }

    #[test]
    fn counts_diagnostics_in_strict_mode() {
        assert_eq!(strict_failure(1), "1 diagnostic in strict mode");
        assert_eq!(strict_failure(2), "2 diagnostics in strict mode");
    }

    #[test]
    fn says_what_strict_mode_fails_on() {
        use clap::CommandFactory;
        let command = Cli::command();
        let render = command.find_subcommand("render").expect("render");
        let strict = render
            .get_arguments()
            .find(|argument| argument.get_id() == "strict")
            .and_then(|argument| argument.get_help())
            .expect("help")
            .to_string();
        assert!(strict.starts_with("Fail on any diagnostic"), "{strict}");
    }

    #[test]
    fn takes_the_version_and_description_of_invoices() {
        let cli = Cli::try_parse_from([
            "fagus",
            "render",
            "invoice.html",
            "-o",
            "invoice.pdf",
            "--factur-x",
            "xrechnung.xml",
            "--factur-x-version",
            "3.0",
            "--factur-x-description",
            "XRechnung Invoice",
        ])
        .expect("parsed");
        let Command::Render {
            factur_x_version,
            factur_x_description,
            ..
        } = cli.command
        else {
            panic!("not render");
        };
        assert_eq!(factur_x_version.as_deref(), Some("3.0"));
        assert_eq!(factur_x_description.as_deref(), Some("XRechnung Invoice"));
    }

    fn encryption(arguments: &[&str]) -> Result<Option<fagus::Encryption>> {
        encryption_in(arguments, &[])
    }

    fn encryption_in(arguments: &[&str], environment: &[(&str, &str)]) -> Result<Option<fagus::Encryption>> {
        let cli = Cli::try_parse_from(
            ["fagus", "render", "a.html", "-o", "a.pdf"]
                .iter()
                .chain(arguments),
        )?;
        let Command::Render { encryption, .. } = cli.command else {
            panic!("not render");
        };
        encryption.encryption_with(|name| {
            environment
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn takes_passwords_from_arguments_and_files() {
        let file = std::env::temp_dir().join(format!("fagus-password-{}", std::process::id()));
        std::fs::write(&file, "aus der Datei\r\nnext line\n").expect("written");
        let path = file.to_str().expect("a path");
        let debug = |arguments: &[&str]| format!("{:?}", encryption(arguments).expect("parsed"));
        assert_eq!(debug(&[]), "None");
        assert!(debug(&["--user-password", "geheim"]).contains("user_password: Some"));
        assert!(debug(&["--owner-password-file", path]).contains("owner_password: Some"));
        assert!(
            debug(&["--owner-password", "x", "--permissions", "print,copy"])
                .contains("permissions: Some([Print, Copy])")
        );
        assert!(debug(&["--owner-password", "x", "--permissions", "none"]).contains("permissions: Some([])"));
        // An empty password is none.
        assert_eq!(debug(&["--user-password", ""]), "None");
        assert!(encryption(&["--permissions", "print"]).is_err());
        assert!(encryption(&["--user-password", "x", "--permissions", "none,print"]).is_err());
        assert!(encryption(&["--user-password", "x", "--user-password-file", path]).is_err());
        assert!(encryption(&["--user-password", "x", "--permissions", "everything"]).is_err());
        assert!(encryption(&["--user-password-file", "/no/such/file"]).is_err());
        // The environment stands in for options that are not given.
        let from = |arguments: &[&str], environment: &[(&str, &str)]| {
            format!("{:?}", encryption_in(arguments, environment).expect("parsed"))
        };
        assert!(from(&[], &[("FAGUS_OWNER_PASSWORD", "o")]).contains("owner_password: Some"));
        assert!(
            from(&["--user-password-file", path], &[("FAGUS_USER_PASSWORD", "u")])
                .contains("user_password: Some")
        );
        assert_eq!(from(&[], &[("FAGUS_USER_PASSWORD", "")]), "None");
        std::fs::remove_file(&file).expect("removed");
    }

    #[test]
    fn writes_the_layout_as_json() {
        let cli = Cli::try_parse_from([
            "fagus",
            "render",
            "a.html",
            "-o",
            "a.pdf",
            "--layout-json",
            "a.json",
        ])
        .expect("parsed");
        let Command::Render { layout_json, .. } = cli.command else {
            panic!("not render");
        };
        assert_eq!(layout_json, Some(PathBuf::from("a.json")));
        let mut engine = fagus::Engine::new();
        engine
            .add_font_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fonts"))
            .expect("fonts");
        let output = engine
            .render(
                &fagus::Document::html(
                    "<!DOCTYPE html><style>@page { size: 400px 300px; margin: 20px } body { margin: 0 }
                     td, th { padding: 0 } table { border-spacing: 0 }</style>
                     <table id=t><thead><tr><th style='height: 20px'>A</th></tr></thead>
                     <tbody><tr id=r><td style='height: 30px'>1</td></tr></tbody></table>",
                )
                .report_layout(true),
            )
            .expect("rendered");
        let json = layout_json::layout_json(&output.layout.expect("layout"));
        let box_of = |page: u32, y: f64, height: f64| serde_json::json!({ "page": page, "x": 15.0, "y": y, "width": json["elements"][0]["boxes"][0]["width"], "height": height });
        assert_eq!(
            json["pages"],
            serde_json::json!([{ "width": 300.0, "height": 225.0 }])
        );
        assert_eq!(json["elements"][0]["id"], "t");
        assert_eq!(json["elements"][0]["tag"], "table");
        assert_eq!(
            json["elements"][0]["boxes"],
            serde_json::json!([box_of(1, 15.0, 37.5)])
        );
        assert_eq!(
            json["elements"][0]["rows"],
            serde_json::json!([
                { "section": "header", "index": 0, "boxes": [box_of(1, 15.0, 15.0)] },
                { "section": "body", "index": 0, "id": "r", "boxes": [box_of(1, 30.0, 22.5)] },
            ])
        );
        assert_eq!(
            json["elements"][1]["boxes"],
            serde_json::json!([box_of(1, 30.0, 22.5)])
        );
        assert!(json["elements"][1].get("rows").is_none());
    }

    #[test]
    fn takes_the_options_of_signatures() {
        let cli = Cli::try_parse_from([
            "fagus",
            "render",
            "invoice.html",
            "-o",
            "invoice.pdf",
            "--sign-p12",
            "signing.p12",
            "--sign-reason",
            "Rechnung",
            "--sign-certify",
            "fill-forms",
        ])
        .expect("parsed");
        let Command::Render { sign, .. } = cli.command else {
            panic!("not render");
        };
        assert_eq!(sign.sign_p12.as_deref(), Some(Path::new("signing.p12")));
        assert_eq!(sign.sign_reason.as_deref(), Some("Rechnung"));
        assert_eq!(sign.sign_certify.as_deref(), Some("fill-forms"));
        assert!(
            Cli::try_parse_from([
                "fagus",
                "render",
                "a.html",
                "-o",
                "a.pdf",
                "--sign-p12",
                "a.p12",
                "--sign-certify",
                "everything"
            ])
            .is_err()
        );
        // What the signature says needs a signature.
        assert!(
            Cli::try_parse_from(["fagus", "render", "a.html", "-o", "a.pdf", "--sign-reason", "x"]).is_err()
        );
    }

    #[test]
    fn renders_with_every_generic_family() {
        let cli = Cli::try_parse_from([
            "fagus",
            "render",
            "letter.html",
            "-o",
            "letter.pdf",
            "--sans-serif",
            "Inter",
            "--serif",
            "Source Serif 4",
            "--monospace",
            "JetBrains Mono",
        ])
        .expect("parsed");
        let Command::Render {
            sans_serif,
            serif,
            monospace,
            ..
        } = cli.command
        else {
            panic!("not render");
        };
        assert_eq!(sans_serif.as_deref(), Some("Inter"));
        assert_eq!(serif.as_deref(), Some("Source Serif 4"));
        assert_eq!(monospace.as_deref(), Some("JetBrains Mono"));
    }
}
