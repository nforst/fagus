//! The merge request of the worker protocol (`M`, see `docs/protocol.md`), merged to the header
//! of its answer (`P` or `E`).

use serde::Deserialize;
use serde_json::{Value, json};

use crate::request::Answer;

/// A merge request (the header of `M`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    /// Required; read from the header before the request, for the answer.
    #[allow(dead_code, reason = "read from the header before the request")]
    id: u64,
    inputs: Vec<Input>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    created: Option<String>,
    /// The memory budget in bytes (0 for none), if not the worker's.
    #[serde(default)]
    memory_budget: Option<u64>,
    /// The time limit in milliseconds (0 for none), if not the worker's.
    #[serde(default)]
    time_limit: Option<f64>,
}

/// A PDF of a merge request: its data block and the pages to take of it.
#[derive(Deserialize)]
struct Input {
    blob: usize,
    #[serde(default)]
    pages: Option<String>,
}

/// Merges the PDFs of the request of `header` and `blobs`, within `limits` unless the request
/// sets its own: the header of `P` with the PDF, or that of `E`.
pub(crate) fn answer(header: Value, blobs: Vec<Vec<u8>>, limits: fagus::Limits) -> Answer {
    let id = header.get("id").and_then(Value::as_u64);
    let merged = merge(header, blobs, limits).and_then(|merge| {
        merge.write().map_err(|error| {
            let code = match error {
                fagus::Error::MemoryBudget { .. } => Some("limit.memory"),
                fagus::Error::TimeLimit { .. } => Some("limit.time"),
                _ => None,
            };
            let message = error.to_string();
            let diagnostics: Vec<Value> = code
                .map(|code| {
                    json!({ "severity": "error", "code": code, "message": message, "source": "document", "line": 0, "column": 0 })
                })
                .into_iter()
                .collect();
            (message, diagnostics)
        })
    });
    match merged {
        Ok(output) => Answer::Pdf {
            header: json!({
                "encrypted": false,
                "signed": false,
                "forms": false,
                "id": id,
                "pages": output.pages,
                "diagnostics": [],
                "timings": { "pdf": output.timings.pdf.as_secs_f64() * 1000.0 },
            }),
            pdf: output.pdf,
        },
        Err((message, diagnostics)) => Answer::Error {
            header: json!({ "id": id, "message": message, "diagnostics": diagnostics }),
        },
    }
}

/// The merge of a request.
fn merge(
    header: Value,
    blobs: Vec<Vec<u8>>,
    limits: fagus::Limits,
) -> Result<fagus::Merge, (String, Vec<Value>)> {
    let refuse = |message: String| (message, Vec::new());
    let request: Request =
        serde_json::from_value(header).map_err(|error| refuse(format!("invalid request: {error}")))?;
    // A data block may be added more than once (the same terms after each invoice).
    let blobs: Vec<std::sync::Arc<Vec<u8>>> = blobs.into_iter().map(std::sync::Arc::new).collect();
    let mut merge = fagus::Merge::new();
    for input in request.inputs {
        let data = blobs
            .get(input.blob)
            .cloned()
            .ok_or_else(|| refuse(format!("data block {} is missing", input.blob)))?;
        merge = match input.pages {
            Some(pages) => merge.add_pages(
                data,
                pages
                    .parse::<fagus::PageRanges>()
                    .map_err(|error| refuse(error.to_string()))?,
            ),
            None => merge.add(data),
        };
    }
    if let Some(title) = request.title {
        merge = merge.title(title);
    }
    for author in request.authors {
        merge = merge.author(author);
    }
    if let Some(language) = request.language {
        merge = merge.language(language);
    }
    if let Some(created) = request.created {
        let date =
            crate::request::date(&created).ok_or_else(|| refuse(format!("invalid date `{created}`")))?;
        merge = merge.created(date);
    }
    let mut limits = limits;
    if let Some(bytes) = request.memory_budget {
        limits = limits.memory((bytes > 0).then_some(bytes));
    }
    if let Some(milliseconds) = request.time_limit {
        let limit = std::time::Duration::try_from_secs_f64(milliseconds / 1000.0)
            .map_err(|_| refuse(format!("invalid time limit `{milliseconds}`")))?;
        limits = limits.time((!limit.is_zero()).then_some(limit));
    }
    Ok(merge.limits(limits))
}
