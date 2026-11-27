# Fagus documentation

Fagus turns HTML and CSS into PDF for PHP and Laravel, the way Chrome prints them, without a
browser. Start with [Getting started](getting-started.md); the other pages are references to look
things up in.

## Using Fagus

- [Getting started](getting-started.md): requirements, installing the Composer package, the first
  PDF in PHP and in Laravel.
- [PHP API](php.md): documents, the PDF they render to, the engine, diagnostics and exceptions.
- [Laravel](laravel.md): the facade, Blade views, responses, disks, queues, Octane and the
  configuration.
- [Resources](resources.md): where images, style sheets, fonts and frames come from, and what Fagus
  never loads.
- [Deployment](deployment.md): Docker, Alpine, servers without access to GitHub, binaries of your
  own, worker processes, speed and memory.
- [Troubleshooting](troubleshooting.md): error messages, diagnostics, differences from Chrome, and
  moving from Dompdf, mPDF, Browsershot or Snappy.
- [Coming from wkhtmltopdf](from-wkhtmltopdf.md): the flags of wkhtmltopdf and Snappy as CSS,
  headers and footers, page numbers and tables of contents.
- [Stability and upgrades](stability.md): what the public API is, what a release may change,
  deprecations and the supported versions of PHP, Laravel and Rust.
- [Docs for AI assistants](ai-assistants.md): `llms.txt` and an MCP server for coding assistants.

## Writing documents

- [CSS reference](css.md): what Fagus supports, what it does differently from Chrome and what it
  does not support.
- [Paged media](paged-media.md): page sizes and margins, headers and footers, page numbers, page
  breaks, tables across pages, tables of contents, print shop marks.
- [Fonts](fonts.md): the built-in font, font directories, `@font-face`, system fonts, fallback,
  OpenType features and languages.
- [Extensions](extensions.md): what Fagus adds where CSS has no way, such as carry-over rows in
  tables that break across pages.

## The PDF

- [The PDF](pdf.md): metadata, links and the outline, tagged and accessible PDF (PDF/UA), PDF/A,
  attachments, electronic invoices (ZUGFeRD, Factur-X, XRechnung), reproducible output, merging
  finished PDFs into one.

## Without PHP

- [Command line](cli.md): the `fagus` binary.
- [Rust library](rust.md): the engine as a Rust crate.
- [Worker protocol](protocol.md): how PHP talks to the binary.

## The project

- [Releasing](releasing.md): how a release reaches GitHub and Composer.
- [Architecture decision records](adr/): why Fagus is built the way it is.
