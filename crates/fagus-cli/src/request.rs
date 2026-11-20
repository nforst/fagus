//! A render request of the worker protocol (the header of `R` and its data blocks, see
//! `docs/protocol.md`), rendered to the header of its answer (`P` or `E`). `fagus worker` reads
//! the request from a frame; the C library of `crates/fagus-ffi`, which includes this file, gets
//! it from its caller, so that both take the same options.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A render request (the header of `R`). Its flags are those of the protocol's JSON.
#[allow(clippy::struct_excessive_bools)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    id: u64,
    html: usize,
    /// Whether the markup is XHTML, parsed as XML.
    #[serde(default)]
    xhtml: bool,
    #[serde(default)]
    base: Option<PathBuf>,
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    stylesheets: Vec<Stylesheet>,
    #[serde(default)]
    resources: Vec<Resource>,
    #[serde(default)]
    ask: bool,
    #[serde(default)]
    conformance: Option<String>,
    #[serde(default = "yes")]
    tagged: bool,
    #[serde(default)]
    accessible: bool,
    #[serde(default)]
    forms: bool,
    #[serde(default)]
    attachments: Vec<Attachment>,
    #[serde(default)]
    factur_x: Option<FacturX>,
    #[serde(default)]
    created: Option<String>,
    /// Whether to report where the elements with an `id` ended up.
    #[serde(default)]
    layout: bool,
    #[serde(default)]
    encryption: Option<Encryption>,
    #[serde(default)]
    signature: Option<SignatureRequest>,
    /// The memory budget in bytes (0 for none), if not the worker's.
    #[serde(default)]
    memory_budget: Option<u64>,
    /// The time limit in milliseconds (0 for none), if not the worker's.
    #[serde(default)]
    time_limit: Option<f64>,
}

/// The digital signature of a render request.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignatureRequest {
    /// The data block of a PKCS#12 file.
    #[serde(default)]
    pkcs12: Option<usize>,
    #[serde(default)]
    password: Option<String>,
    /// The data blocks of the certificates of a signer the caller asks (`S`).
    #[serde(default)]
    certificates: Vec<usize>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    contact_info: Option<String>,
    #[serde(default)]
    time: Option<String>,
    /// Whether the caller timestamps the signature (`T`).
    #[serde(default)]
    timestamp: bool,
    /// What a certification signature allows: `no-changes`, `fill-forms` or `annotate`.
    #[serde(default)]
    certify: Option<String>,
    /// The `id` of the element in whose box the signature shows.
    #[serde(default)]
    element: Option<String>,
    /// The area of a page in which the signature shows.
    #[serde(default)]
    area: Option<Area>,
    /// Whether the caller gets the revocation status of the certificates (`V`), for B-LT.
    #[serde(default)]
    revocation: bool,
    /// Whether an archive timestamp follows the validation data (B-LTA), asked for with `T`.
    #[serde(default)]
    archive_timestamp: bool,
}

/// An area of a page: the number of the page (from 1) and the area in points from its top left
/// corner.
#[derive(Deserialize)]
struct Area {
    page: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

/// The default of fields that are on unless the caller turns them off.
fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Stylesheet {
    name: String,
    blob: usize,
}

#[derive(Deserialize)]
struct Resource {
    url: String,
    blob: usize,
}

#[derive(Deserialize)]
struct Attachment {
    name: String,
    mime: String,
    #[serde(default)]
    relationship: Option<String>,
    #[serde(default)]
    description: Option<String>,
    blob: usize,
}

#[derive(Deserialize)]
struct FacturX {
    blob: usize,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Encryption {
    #[serde(default)]
    user_password: Option<String>,
    #[serde(default)]
    owner_password: Option<String>,
    /// Every permission if left out.
    #[serde(default)]
    permissions: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Diagnostic<'a> {
    severity: &'static str,
    code: &'a str,
    message: &'a str,
    source: &'a str,
    line: u32,
    column: u32,
}

fn diagnostics(list: &[fagus::Diagnostic]) -> Vec<Diagnostic<'_>> {
    list.iter()
        .map(|diagnostic| Diagnostic {
            severity: match diagnostic.severity {
                fagus::Severity::Info => "info",
                fagus::Severity::Warning => "warning",
                fagus::Severity::Error => "error",
            },
            code: diagnostic.code,
            message: &diagnostic.message,
            source: &diagnostic.source,
            line: diagnostic.line,
            column: diagnostic.column,
        })
        .collect()
}

/// What the engine may ask the caller while a document renders, beyond resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Question {
    /// To sign a digest (`S`).
    Sign,
    /// To timestamp a digest (`T`).
    Timestamp,
    /// For the revocation status of a certificate (`V`).
    Revocation,
}

/// Whom the engine asks while a request renders: for resources (`?` in the protocol) and for
/// signatures, timestamps and revocation data.
pub(crate) trait Caller: Send + Sync {
    /// The resource at `url`, if the caller has it.
    fn resource(&self, id: u64, url: &str) -> Option<Vec<u8>>;

    /// Asks `question` with the header and data blocks of the protocol's message, and returns the
    /// data blocks of the answer.
    fn ask(&self, question: Question, header: Value, data: &[&[u8]]) -> Result<Vec<Vec<u8>>, String>;
}

/// Asks the caller for resources the document needs, while it renders.
struct Asker {
    caller: Arc<dyn Caller>,
    id: u64,
}

impl fagus::ResourceLoader for Asker {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        self.caller.resource(self.id, url)
    }
}

/// The answer to a render request: the header of `P` with the PDF, or that of `E`.
pub(crate) enum Answer {
    Pdf { header: Value, pdf: Vec<u8> },
    Error { header: Value },
}

/// Renders the request of `header` and `blobs` with `engine`. Without a `caller`, a request that
/// needs one (`ask`, a signer of the caller's own, timestamps, revocation data) is answered with
/// an error.
pub(crate) fn render(
    engine: &fagus::Engine,
    header: Value,
    blobs: Vec<Vec<u8>>,
    caller: Option<&Arc<dyn Caller>>,
) -> Answer {
    let id = header.get("id").and_then(Value::as_u64);
    // Tells the caller that the PDF is encrypted, which a worker without encryption would not
    // say: it ignores the field it does not know.
    let encrypted = header
        .get("encryption")
        .is_some_and(|encryption| !encryption.is_null());
    // Likewise that it is signed.
    let signed = header
        .get("signature")
        .is_some_and(|signature| !signature.is_null());
    // And that it made the form controls fields.
    let forms = header.get("forms").and_then(Value::as_bool) == Some(true);
    let reply = document(header, blobs, engine.limits(), caller)
        .map_err(|message| (message, Vec::new()))
        .and_then(|document| {
            engine
                .render_with_diagnostics(&document)
                .map_err(|failure| (failure.error.to_string(), failure.diagnostics))
        });
    match reply {
        Ok(output) => {
            let mut header = json!({
                "encrypted": encrypted,
                "signed": signed,
                "forms": forms,
                "id": id,
                "pages": output.pages,
                "diagnostics": diagnostics(&output.diagnostics),
                "timings": {
                    "parse": output.timings.parse.as_secs_f64() * 1000.0,
                    "style": output.timings.style.as_secs_f64() * 1000.0,
                    "layout": output.timings.layout.as_secs_f64() * 1000.0,
                    "paint": output.timings.paint.as_secs_f64() * 1000.0,
                    "pdf": output.timings.pdf.as_secs_f64() * 1000.0,
                },
            });
            if let Some(layout) = &output.layout {
                header["layout"] = crate::layout_json::layout_json(layout);
            }
            Answer::Pdf {
                header,
                pdf: output.pdf,
            }
        }
        Err((message, found)) => Answer::Error {
            header: json!({ "id": id, "message": message, "diagnostics": diagnostics(&found) }),
        },
    }
}

/// The caller that `what` of the request needs, or why there is none.
fn needs<'a>(caller: Option<&'a Arc<dyn Caller>>, what: &str) -> Result<&'a Arc<dyn Caller>, String> {
    caller.ok_or_else(|| format!("{what} needs a caller that answers questions, and there is none"))
}

/// The document of a render request.
fn document(
    header: Value,
    blobs: Vec<Vec<u8>>,
    limits: fagus::Limits,
    caller: Option<&Arc<dyn Caller>>,
) -> Result<fagus::Document, String> {
    let request: Request =
        serde_json::from_value(header).map_err(|error| format!("invalid request: {error}"))?;
    let mut blobs: Vec<Option<Vec<u8>>> = blobs.into_iter().map(Some).collect();
    let mut take = |index: usize| {
        blobs
            .get_mut(index)
            .and_then(Option::take)
            .ok_or_else(|| format!("data block {index} is missing"))
    };
    let html = String::from_utf8(take(request.html)?).map_err(|_| "the HTML is not UTF-8".to_owned())?;
    let mut document = if request.xhtml {
        fagus::Document::xhtml(html)
    } else {
        fagus::Document::html(html)
    };
    if let Some(base) = request.base {
        document = document.base_directory(base);
    }
    if let Some(root) = request.root {
        document = document.root(root);
    }
    for stylesheet in request.stylesheets {
        let css = String::from_utf8(take(stylesheet.blob)?)
            .map_err(|_| format!("the stylesheet `{}` is not UTF-8", stylesheet.name))?;
        document = document.stylesheet(stylesheet.name, css);
    }
    for resource in request.resources {
        document = document.resource(resource.url, take(resource.blob)?);
    }
    if request.ask {
        document = document.loader(Arc::new(Asker {
            caller: Arc::clone(needs(caller, "`ask`")?),
            id: request.id,
        }));
    }
    if let Some(level) = request.conformance {
        let level =
            fagus::Conformance::from_level(&level).ok_or_else(|| format!("unknown PDF/A level `{level}`"))?;
        document = document.conformance(level);
    }
    document = document.tagged(request.tagged).forms(request.forms);
    if request.accessible {
        document = document.accessible();
    }
    for attachment in request.attachments {
        // PDF/A-3 and PDF/UA need a description: the file name, as the command line gives.
        let description = attachment
            .description
            .filter(|description| !description.is_empty())
            .unwrap_or_else(|| attachment.name.clone());
        let relationship = match attachment.relationship.as_deref().unwrap_or("unspecified") {
            "source" => fagus::Relationship::Source,
            "data" => fagus::Relationship::Data,
            "alternative" => fagus::Relationship::Alternative,
            "supplement" => fagus::Relationship::Supplement,
            "unspecified" => fagus::Relationship::Unspecified,
            other => return Err(format!("unknown relationship `{other}`")),
        };
        document = document.attach(
            fagus::Attachment::new(attachment.name, attachment.mime, take(attachment.blob)?)
                .description(description)
                .relationship(relationship),
        );
    }
    if let Some(invoice) = request.factur_x {
        let xml = take(invoice.blob)?;
        document = match invoice.profile.as_deref() {
            None => document.factur_x(xml),
            Some(profile) => document.factur_x_profile(
                xml,
                self::profile(profile).ok_or_else(|| format!("unknown profile `{profile}`"))?,
            ),
        };
        if let Some(version) = invoice.version {
            document = document.factur_x_version(version);
        }
        if let Some(description) = invoice.description {
            document = document.factur_x_description(description);
        }
    }
    if let Some(created) = request.created {
        document = document.created(date(&created).ok_or_else(|| format!("invalid date `{created}`"))?);
    }
    if request.layout {
        document = document.report_layout(true);
    }
    if request.memory_budget.is_some() || request.time_limit.is_some() {
        let mut limits = limits;
        if let Some(bytes) = request.memory_budget {
            limits = limits.memory((bytes > 0).then_some(bytes));
        }
        if let Some(milliseconds) = request.time_limit {
            let limit = std::time::Duration::try_from_secs_f64(milliseconds / 1000.0)
                .map_err(|_| format!("invalid time limit `{milliseconds}`"))?;
            limits = limits.time((!limit.is_zero()).then_some(limit));
        }
        document = document.limits(limits);
    }
    if let Some(encryption) = request.encryption {
        let mut protection = fagus::Encryption::new();
        if let Some(password) = encryption.user_password {
            protection = protection.user_password(password);
        }
        if let Some(password) = encryption.owner_password {
            protection = protection.owner_password(password);
        }
        if let Some(names) = encryption.permissions {
            let permissions = names
                .iter()
                .map(|name| {
                    fagus::Permission::from_name(name).ok_or_else(|| format!("unknown permission `{name}`"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            protection = protection.permissions(permissions);
        }
        document = document.encrypt(protection);
    }
    if let Some(request_signature) = request.signature {
        document = document.sign(signature(caller, request.id, request_signature, &mut take)?);
    }
    Ok(document)
}

/// The signature of a render request.
fn signature(
    caller: Option<&Arc<dyn Caller>>,
    id: u64,
    request: SignatureRequest,
    take: &mut impl FnMut(usize) -> Result<Vec<u8>, String>,
) -> Result<fagus::Signature, String> {
    let mut signature = match (request.pkcs12, request.certificates.is_empty()) {
        (Some(blob), true) => {
            fagus::Signature::pkcs12(&take(blob)?, request.password.as_deref().unwrap_or_default())
                .map_err(|error| error.to_string())?
        }
        (None, false) => {
            let certificates = request
                .certificates
                .iter()
                .map(|&blob| take(blob))
                .collect::<Result<Vec<_>, _>>()?;
            let caller = Arc::clone(needs(caller, "a signature with `certificates`")?);
            fagus::Signature::with_signer(certificates, move |digest: &[u8], algorithm: fagus::Algorithm| {
                first(caller.ask(
                    Question::Sign,
                    json!({ "id": id, "algorithm": algorithm.name() }),
                    &[digest],
                ))
            })
        }
        _ => return Err("a signature needs either `pkcs12` or `certificates`".to_owned()),
    };
    if request.timestamp {
        let caller = Arc::clone(needs(caller, "a signature with `timestamp`")?);
        signature = signature.timestamp(move |digest: &[u8]| {
            first(caller.ask(Question::Timestamp, json!({ "id": id }), &[digest]))
        });
    }
    if let Some(name) = request.name {
        signature = signature.name(name);
    }
    if let Some(reason) = request.reason {
        signature = signature.reason(reason);
    }
    if let Some(location) = request.location {
        signature = signature.location(location);
    }
    if let Some(contact) = request.contact_info {
        signature = signature.contact_info(contact);
    }
    if let Some(time) = request.time {
        signature = signature.time(date(&time).ok_or_else(|| format!("invalid date `{time}`"))?);
    }
    if let Some(name) = request.certify {
        signature = signature.certify(
            fagus::Certification::from_name(&name)
                .ok_or_else(|| format!("unknown certification `{name}`"))?,
        );
    }
    if request.revocation {
        let caller = Arc::clone(needs(caller, "a signature with `revocation`")?);
        signature = signature.revocation(move |question: &fagus::RevocationRequest| {
            caller.ask(
                Question::Revocation,
                json!({ "id": id, "ocspUrls": question.ocsp_urls, "crlUrls": question.crl_urls }),
                &[&question.certificate, &question.issuer, &question.ocsp_request],
            )
        });
    }
    if request.archive_timestamp {
        signature = signature.archive_timestamp();
    }
    match (request.element, request.area) {
        (Some(_), Some(_)) => return Err("a signature has either `element` or `area`".to_owned()),
        (Some(id), None) => signature = signature.visible_on(id),
        (None, Some(area)) => {
            signature = signature.visible_at(area.page, area.x, area.y, area.width, area.height);
        }
        (None, None) => {}
    }
    Ok(signature)
}

/// The first data block of an answer, which signatures and timestamps have.
fn first(answer: Result<Vec<Vec<u8>>, String>) -> Result<Vec<u8>, String> {
    answer?
        .into_iter()
        .next()
        .ok_or_else(|| "the caller's answer has no data".to_owned())
}

/// A date of the form `2026-11-02T12:00:00Z`, or with an offset (`+01:00`), in UTC.
pub(crate) fn date(text: &str) -> Option<fagus::Date> {
    crate::date::parse(text)
}

/// An invoice profile by its name on the command line and in the protocol.
pub(crate) fn profile(name: &str) -> Option<fagus::FacturXProfile> {
    Some(match name {
        "minimum" => fagus::FacturXProfile::Minimum,
        "basicwl" => fagus::FacturXProfile::BasicWl,
        "basic" => fagus::FacturXProfile::Basic,
        "en16931" => fagus::FacturXProfile::En16931,
        "extended" => fagus::FacturXProfile::Extended,
        "xrechnung" => fagus::FacturXProfile::XRechnung,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dates() {
        let date = date("2026-11-02T12:30:05Z").expect("date");
        assert_eq!(
            (
                date.year,
                date.month,
                date.day,
                date.hour,
                date.minute,
                date.second
            ),
            (2026, 11, 2, 12, 30, 5)
        );
        assert!(super::date("2026-13-02T12:30:05Z").is_none());
        assert!(super::date("gestern").is_none());
    }

    #[test]
    fn needs_a_caller_to_ask() {
        let engine = fagus::Engine::new();
        let refused = |header: Value, blobs: Vec<Vec<u8>>| match render(&engine, header, blobs, None) {
            Answer::Error { header } => header["message"].as_str().unwrap_or_default().to_owned(),
            Answer::Pdf { .. } => String::new(),
        };
        assert_eq!(
            refused(
                json!({ "id": 1, "html": 0, "ask": true }),
                vec![b"<p>x</p>".to_vec()]
            ),
            "`ask` needs a caller that answers questions, and there is none"
        );
        assert_eq!(
            refused(
                json!({ "id": 2, "html": 0, "signature": { "certificates": [1] } }),
                vec![b"<p>x</p>".to_vec(), b"certificate".to_vec()]
            ),
            "a signature with `certificates` needs a caller that answers questions, and there is none"
        );
        // Without a question to ask, no caller is needed.
        let Answer::Pdf { header, pdf } = render(
            &engine,
            json!({ "id": 3, "html": 0, "ask": false }),
            vec![b"<p>x</p>".to_vec()],
            None,
        ) else {
            panic!("not rendered");
        };
        assert_eq!(header["id"], 3);
        assert!(pdf.starts_with(b"%PDF-"));
    }
}
