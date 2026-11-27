//! `fagus merge` and the merge request of the worker protocol (`M`, see `docs/protocol.md`).

use std::path::PathBuf;

use anyhow::{Result, bail};
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

/// The pages to take of each input of `fagus merge`: `inputs` are the positions of the inputs
/// among the arguments, `pages` those of the values of `--pages` with the values. Each `--pages`
/// belongs to the input before it.
pub(crate) fn selections(
    inputs: &[usize],
    pages: &[(usize, String)],
) -> Result<Vec<Option<fagus::PageRanges>>> {
    let mut selections = vec![None; inputs.len()];
    for (position, text) in pages {
        let Some(input) = inputs.iter().rposition(|input| input < position) else {
            bail!(
                "--pages {text} comes before any PDF file; it belongs after the file whose pages it selects"
            );
        };
        if selections[input].is_some() {
            bail!(
                "PDF file {} has --pages twice; give its pages in one, like `1-3,5`",
                input + 1
            );
        }
        selections[input] = Some(text.parse::<fagus::PageRanges>()?);
    }
    Ok(selections)
}

/// The error of a merge, with the file it is about named in place of its index.
pub(crate) fn named(error: fagus::Error, paths: &[PathBuf]) -> anyhow::Error {
    let path = |input: usize| {
        paths
            .get(input)
            .map_or_else(String::new, |path| path.display().to_string())
    };
    match error {
        fagus::Error::Pdf(fagus::PdfError::Unreadable { input, reason }) => {
            anyhow::anyhow!("{} cannot be read: {reason}", path(input))
        }
        fagus::Error::Pdf(fagus::PdfError::Encrypted { input }) => {
            anyhow::anyhow!(
                "{} is encrypted; only PDFs without encryption can be merged",
                path(input)
            )
        }
        fagus::Error::Pdf(fagus::PdfError::NoPage { input, page, pages }) => {
            let noun = if pages == 1 { "page" } else { "pages" };
            anyhow::anyhow!("{} has {pages} {noun}, not page {page}", path(input))
        }
        error => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_each_file_the_pages_after_it() {
        let pages = |list: &[(usize, &str)]| -> Vec<(usize, String)> {
            list.iter()
                .map(|(position, text)| (*position, (*text).to_owned()))
                .collect()
        };
        // fagus merge a.pdf --pages 1-2 b.pdf c.pdf --pages 3- -o out.pdf
        let selections = selections(&[2, 5, 6], &pages(&[(4, "1-2"), (8, "3-")])).expect("selections");
        let texts: Vec<Option<String>> = selections
            .iter()
            .map(|pages| pages.as_ref().map(ToString::to_string))
            .collect();
        assert_eq!(texts, [Some("1-2".to_owned()), None, Some("3-".to_owned())]);
        assert_eq!(
            super::selections(&[3], &pages(&[(2, "1")]))
                .expect_err("before")
                .to_string(),
            "--pages 1 comes before any PDF file; it belongs after the file whose pages it selects"
        );
        assert_eq!(
            super::selections(&[2], &pages(&[(4, "1"), (6, "2")]))
                .expect_err("twice")
                .to_string(),
            "PDF file 1 has --pages twice; give its pages in one, like `1-3,5`"
        );
        assert_eq!(
            super::selections(&[2], &pages(&[(4, "0")]))
                .expect_err("invalid")
                .to_string(),
            "invalid pages `0`: pages count from 1"
        );
    }

    #[test]
    fn names_the_file_it_cannot_merge() {
        let paths = [PathBuf::from("a.pdf"), PathBuf::from(r"C:\Belege\b.pdf")];
        let named = |error: fagus::PdfError| named(fagus::Error::Pdf(error), &paths).to_string();
        assert_eq!(
            named(fagus::PdfError::Encrypted { input: 1 }),
            r"C:\Belege\b.pdf is encrypted; only PDFs without encryption can be merged"
        );
        assert_eq!(
            named(fagus::PdfError::NoPage {
                input: 0,
                page: 4,
                pages: 1
            }),
            "a.pdf has 1 page, not page 4"
        );
        assert_eq!(
            named(fagus::PdfError::Unreadable {
                input: 0,
                reason: "it has no pages".to_owned()
            }),
            "a.pdf cannot be read: it has no pages"
        );
        assert_eq!(
            named(fagus::PdfError::NothingToMerge),
            "there are no PDF files to merge"
        );
    }
}
