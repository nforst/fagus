//! `fagus merge` and the merge request of the worker protocol (`M`, see `docs/protocol.md`).

use std::path::PathBuf;

use anyhow::{Result, bail};

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
