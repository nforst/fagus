# Upgrading

What to change in code, configuration and templates when updating to a new major release. Minor
releases need no changes (see [docs/stability.md](docs/stability.md)); PDFs may still look
different after one, closer to Chrome.

## Next major

Breaking changes planned for the next major release are collected here as soon as they are
decided, each with the deprecation in a minor release that prepares it, so that you can act on
them early; when the major release comes, they become its section. Nothing is planned for 2.0 yet.

## From 0.x to 1.0

This section collects the changes since 0.1.0 that can need action, so that an update from any
0.x release is covered in one place. The [changelog](CHANGELOG.md) has
every change.

- **`element()` with other values in `content`.** A margin box with `content: element(a) element(b)`
  or `content: element(title) " – " counter(page)` is invalid, as CSS Generated Content for Paged
  Media 3 specifies; the declaration is ignored with a `css.invalid-value` diagnostic. Before, the
  box showed the first running element only. Put the pieces into one running element (the page
  number with `counter(page)` in `::after`), or use the neighbouring margin boxes. *(1.0.0)*
- **`Output::layout` in Rust.** `fagus::Output` has a new field, `layout` (`None` unless the
  document asked with `report_layout(true)`). *(1.0.0)*
- **`#[non_exhaustive]` types in Rust.** The public enums and the structs with public fields of
  the crate `fagus` that may grow are `#[non_exhaustive]`, so that later minor releases can add to
  them ([Rust library](docs/rust.md#stability)). Struct literals of them no longer compile outside
  the crate:
  - `Attachment { name, mime_type, description, relationship, data }` becomes
    `Attachment::new(name, mime_type, data).description(…).relationship(…)`; `data` may be a
    `Vec<u8>` or an `Arc<Vec<u8>>`, and the relationship is `Unspecified` unless given.
  - `Date { year, month, day, hour, minute, second }` becomes
    `Date::new(year, month, day, hour, minute, second)`, also in a `const`.
  - `Diagnostic { … }` becomes `Diagnostic::new(severity, code, message, source).at(line, column)`.
  - An `Output` or `Timings` of your own, in tests for example, starts from `Default::default()`
    and sets its fields; a `Failure` comes from an `Error` with `Failure::from`.
  - A `match` over `Error`, `PdfError`, `FontError`, `Conformance`, `Relationship`,
    `FacturXProfile` or `Algorithm` needs a `_` arm, and a pattern that takes one of the structs
    apart needs `..`. A `match` over `Severity` may stay as it is. *(1.0.0)*
- **Strict mode and new diagnostics.** A `size` in `@page` that Chrome does not take (`80mm auto`,
  `A6`, three lengths) now gets a `css.invalid-value` warning. The page keeps its size as before,
  but `strict()` and `--strict` fail on such a document. Write both lengths (`size: 80mm 200mm`,
  `size: 105mm 148mm` for A6). *(1.0.0)*
