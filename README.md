<p align="center">
  <img src="art/banner.png" alt="Fagus: HTML and CSS to PDF for PHP and Laravel, without a browser" width="1280">
</p>

<p align="center">
  <a href="https://github.com/nforst/fagus/actions/workflows/ci.yml"><img src="https://github.com/nforst/fagus/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://packagist.org/packages/nforst/fagus"><img src="https://img.shields.io/packagist/v/nforst/fagus" alt="Latest version on Packagist"></a>
  <a href="https://packagist.org/packages/nforst/fagus"><img src="https://img.shields.io/packagist/dependency-v/nforst/fagus/php" alt="PHP version"></a>
  <a href="#license"><img src="https://img.shields.io/packagist/l/nforst/fagus" alt="Licence"></a>
</p>

<p align="center">
  <a href="https://fagus.niklasforst.de/docs"><strong>Documentation</strong></a>
</p>

# Fagus

HTML and CSS to PDF for PHP and Laravel, without a browser.

> **Status:** stable since 1.0. It renders real invoices and reports (Quittio's invoice template,
> Tailwind-style pages) like Chrome prints them, and the CSS coverage keeps growing. Only a major
> release changes the API; [Stability](https://fagus.niklasforst.de/docs/stability) says what is
> covered and how deprecations work. Design decisions are recorded in [docs/adr](https://fagus.niklasforst.de/docs/adr).

```sh
composer require nforst/fagus
```

```php
use Fagus\Fagus;

Fagus::html($html)->save('invoice.pdf');
Fagus::html($html)->inline('invoice.pdf'); // or show it in the browser
```

Fagus is a Composer package that turns HTML and CSS into PDF the way Chrome prints them. There is
no Chrome, Puppeteer or Node to install, no Docker image and no system library: the rendering
happens in a native engine written in Rust, which Composer downloads for your platform (checked
against its SHA-256) and which PHP keeps running as a worker process between documents.

- **Simple:** `composer require` is all it takes, on Linux (x64 and arm64, glibc or musl, so Alpine
  too), macOS (Intel and Apple silicon), Windows x64 and FreeBSD x64. Every platform prints the
  same PDF.
- **Fast:** a one-page invoice in about 2.5 ms once the worker runs, ten pages in about 17 ms,
  measured from PHP on an Apple M2 Pro (tagged PDF). The first document of a process takes about
  9 ms. For one page that is 10 to 30 times faster than mPDF, Dompdf or a Chrome kept running,
  see the [comparison](#compared-with-other-renderers).
- **Faithful:** modern CSS (flex, grid, Tailwind v4 included) renders the way it does in Chrome;
  page breaks, running headers, page counters and repeated table headers work the way print CSS
  specifies them.
- **Standards:** PDF/A-3 with ZUGFeRD/Factur-X attachments and accessible PDF (PDF/UA) built in.
- **Safe:** no network access, no files outside the directory you allow, no scripts; the engine has
  no `unsafe` code of its own (the command line has one call of glibc's `malloc_trim`).

Dompdf and mPDF stop at CSS 2 with some extensions, without flex or grid; Browsershot, Snappy and
Gotenberg need a browser or a service. Fagus sits in between: a PHP package with the CSS of a
browser. The [comparison](#compared-with-other-renderers) below shows speed and features side by
side.

## Installation

PHP 8.3 or newer. Composer asks whether to trust the plugin of `nforst/fagus`: it installs the
`fagus` binary of the package's release after every `composer install` and `composer update`.

```sh
composer require nforst/fagus
```

In Laravel 13 the same package registers itself (facade `Fagus`, Blade views, responses, disks);
publish the configuration if you want to change it:

```sh
php artisan vendor:publish --tag=fagus-config
```

[Getting started](https://fagus.niklasforst.de/docs/getting-started) walks through the first PDF;
[Deployment](https://fagus.niklasforst.de/docs/deployment) covers Docker, Alpine, servers without access to GitHub and
binaries of your own.

## Usage

```php
use Fagus\Conformance;
use Fagus\Fagus;

$pdf = Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf')   // relative URLs and the files they may read
    ->resource('logo.png', $logo)                // data the document refers to
    ->pdfA(Conformance::PdfA3b)
    ->facturX($invoiceXml)                       // ZUGFeRD / Factur-X
    ->render();

$pdf->save('invoice.pdf');
$pdf->pages();
$pdf->diagnostics();  // CSS that was ignored, resources that are missing, with their position

Fagus::html($html)->accessible()->save('letter.pdf'); // PDF/UA-1
```

In Laravel, from a Blade view:

```php
use Fagus\Laravel\Facades\Fagus;

return Fagus::view('invoices.show', ['invoice' => $invoice])->name('invoice.pdf'); // inline
Fagus::view('invoices.show', $data)->download('invoice.pdf');
Fagus::view('invoices.show', $data)->pdfA('3b')->store('invoices/2026-0042.pdf', 's3');
```

The page size, margins, headers and footers come from `@page` rules in the document, the PDF's
title from `<title>` and its language from `<html lang>`, as in a browser. The packages describe
everything else is in the [documentation](https://fagus.niklasforst.de/docs).

## Compared with other renderers

### Speed

Time per PDF of an invoice, the median of three runs in new processes on an Apple M2 Pro (macOS 27).
*First* is the first PDF of a process, as in a PHP-FPM request (for Fagus it includes starting the
worker, for Chrome kept running it does not include starting the browser); every other column is a further PDF in the same process, as in a queue worker or Octane.
The invoice uses only CSS 2.1 (tables, no flex or grid) so that every engine lays it out.

| Engine | 1 page, first | 1 page | 10 pages | 40 pages |
|---|--:|--:|--:|--:|
| **Fagus** | **9 ms** | **2.3 ms** | **17 ms** | **63 ms** |
| Chrome 157, kept running (as in Gotenberg) | 103 ms | 75 ms | 122 ms | 217 ms |
| mPDF 8.3 | 63 ms | 25 ms | 233 ms¹ | 898 ms¹ |
| Dompdf 3.1 | 70 ms | 48 ms | 349 ms¹ | 2,550 ms¹ |
| WeasyPrint 66 | 158 ms | 105 ms | 668 ms | 2,596 ms |
| Browsershot 5.5 (a new Chrome per PDF) | 611 ms | 601 ms | 650 ms | 751 ms |

¹ Same content, other page breaks: Dompdf sets the 10 pages on 12 and the 40 on 44, mPDF (which
ignores `@font-face` and uses its own font) on 8 and 31.

With modern CSS (the invoice of `tools/gen.py`: flex, grid, margin boxes with page numbers), which
Dompdf and mPDF cannot lay out, Fagus takes 2.5 ms for one page and 17.5 ms for ten, Chrome 81 and
123 ms, WeasyPrint 134 and 715 ms. Chrome is measured from Node with Puppeteer; through Gotenberg
an HTTP request comes on top. The scripts and documents are in [bench/compare](bench/compare).

Memory, as the physical footprint of PHP and the worker together: 22 MB for the one-page invoice and
60 MB for 40 pages; Chrome kept running takes 250 to 400 MB, WeasyPrint 74 to 191 MB. Four Fagus
workers side by side render 224 ten-page PDFs a second, one Chrome with four tabs 24. The tables are
in [bench/README.md](bench/README.md#memory-and-cpu-time-memory).

### Features

| | Fagus | Chrome (Browsershot, Gotenberg) | WeasyPrint | Dompdf | mPDF |
|---|:-:|:-:|:-:|:-:|:-:|
| Installs with | Composer (downloads one binary) | Node, Puppeteer and Chrome, or a service | Python and Pango | Composer | Composer |
| Licence | MIT or Apache-2.0 | MIT, BSD | BSD-3-Clause | LGPL-2.1 | GPL-2.0 |
| Flexbox, grid | ✓ | ✓ | ✓² | – | – |
| Headers and footers in `@page` margin boxes, "page 2 of 5" | ✓ | ✓ | ✓ | –³ | –³ |
| HTML in headers and footers (`position: running()`), `string-set` | ✓ | – | ✓ | – | – |
| Table of contents with page numbers (`target-counter()`) | ✓ | – | ✓ | – | –³ |
| Table headers repeated on every page | ✓ | ✓ | ✓ | ✓ | ✓ |
| Hyphenation (`hyphens: auto`) | ✓ | ✓ | ✓ | – | ✓ |
| Footnotes (`float: footnote`) | ✓ | – | ✓ | – | – |
| JavaScript | –⁴ | ✓ | – | – | – |
| Bookmarks (PDF outline) | ✓ | with an option | ✓ | – | –³ |
| Tagged PDF, PDF/UA | ✓ | tagged only | with an option | – | – |
| PDF/A | 1b, 2b, 2u, 2a, 3b, 3u, 3a, 4, 4e, 4f | – | ✓ | – | 1b, 3b |
| ZUGFeRD / Factur-X | built in | – | by hand | – | by hand |
| Same PDF on Linux, macOS and Windows | ✓ | depends on the fonts installed | depends on the fonts installed | ✓ | ✓ |

² WeasyPrint passes the simple cases tested here; its documentation calls flex layout partly
supported. ³ mPDF has headers, footers, bookmarks and tables of contents through its own tags
(`<htmlpageheader>`, `<bookmark>`, `<tocpagebreak>`), Dompdf page numbers through a PHP callback,
not through CSS. ⁴ Fagus runs no scripts on purpose, so that a template cannot reach the network
or the server; draw charts on the server instead, as inline SVG or images (with any PHP chart
library).

The CSS rows are checked by rendering the documents in
[bench/compare/probes](bench/compare/probes) with every engine (`bench/compare/run.sh`); the other
rows follow each project's documentation. Snappy and laravel-snappy use wkhtmltopdf, whose
repository was archived in 2023 and whose WebKit dates from 2012; it is not measured.

## What it renders

- **CSS:** the cascade with layers, nesting, `:has()`, custom properties, `@property` and `calc()`, and the presentational attributes of HTML (`width`, `align`, `bgcolor`, `cellpadding` …); colors of CSS Color 4 and 5; block, inline, float, flex (with wrapping), grid and multi-column layout (with spanning elements); tables with spans and collapsing borders; positioning and stacking; images and inline SVG, with `object-fit`; backgrounds, gradients (with `in <color space>` interpolation), shadows, transforms; lists and counters; `@font-face` with TTF, OTF, WOFF and WOFF2, variable fonts and `unicode-range`; OpenType features through `font-variant` (with made-up small capitals) and `font-feature-settings`; line breaking as in Chrome, with `overflow-wrap`, `word-break` and hyphenation with the dictionaries and rules of Chrome on Linux (the same on every system); text decorations with their thickness, offset and styles; `text-overflow: ellipsis`.
- **Pages:** `@page` sizes and margins, `:first`, `:left`, `:right` and named pages, margin boxes with running elements, named strings and page counters; breaks inside paragraphs (with orphans and widows), tables, flex, grid and multi-column containers, headings kept with what follows them, with headers and footers repeated on every page.
- **PDF:** fonts embedded as subsets, vector gradients, links and the document outline, PDF/A-1b, PDF/A-2, PDF/A-3 and PDF/A-4, attachments, ZUGFeRD/Factur-X invoices with the XMP metadata they need. veraPDF and Mustang check the output (`tools/validate.sh`).
- **Accessibility:** tagged by default, with the structure of the document (headings, paragraphs, lists, data tables, images with their alternative text, links) in reading order, headers, footers and decoration marked as artifacts; PDF/UA-1, also together with PDF/A-2a, PDF/A-3a or a Factur-X invoice.
- **Frames:** an `<iframe>` (its `srcdoc`, else its `src`), and an `<object>` or `<embed>` whose resource is an HTML document, shows that document as Chrome prints it: laid out on a viewport as large as the frame's content box (which its media queries and viewport units see; its media type is still `print`), not paginated, cut off at the frame's edges, on a transparent canvas, with its own style sheets, fonts and images; text files and images show as Chrome's documents for them, and `frame-sizing: content-height` makes a frame as tall as a document that allows it (`<meta name=responsive-embedded-sizing>`). A frame's document loads like any other resource (a `data:` URL, the resources handed over, the caller's loader, a file inside the root), its relative URLs resolving against its own URL; scripts do not run. Frames nest at most 8 deep, and one render lays out at most 256 documents in frames with 8 MiB of markup together, so that frames that include themselves end.
- **Diagnostics:** CSS that is ignored and resources that are missing are reported with their source position.
- **Extensions** where CSS has no way, prefixed with `-fagus-`: carry-over rows with the running total where a table breaks (*Übertrag*), see [Extensions](https://fagus.niklasforst.de/docs/extensions).

## Fonts and languages

Fonts come from `@font-face` rules and from the font directories you configure. Fagus brings Inter
in regular and bold for text that no other font covers, so it also renders without fonts of its
own. Inter is then also what `serif` and text without a `font-family` get, as Fagus has no serif
font; with fonts of yours, it is the first of them, unless the engine's `serif`, `sansSerif` and
`monospace` options name the families ([fonts](https://fagus.niklasforst.de/docs/fonts#set-the-generic-families)).

With `systemFonts: true` (`system_fonts` in Laravel, `--system-fonts` on the command line) the fonts
installed on the machine serve too, found as Chrome finds them: on macOS in `/System/Library/Fonts`,
`/Library/Fonts` and `~/Library/Fonts`, on Linux through fontconfig, on Windows in `%WINDIR%\Fonts`
and the user's fonts. Families that documents name then come from the system, the generic families
are those of Chrome on macOS (Times, Helvetica, Menlo, San Francisco for `system-ui`, Apple Chancery,
Papyrus; Hiragino, PingFang or Apple SD Gothic Neo by the `lang` of the text), and characters that
the document's fonts lack fall back as in Chrome (to PingFang or Songti for Chinese, Hiragino for
Japanese, Geeza Pro for Arabic, Zapf Dingbats or Apple Symbols for symbols …). Configured fonts and
`@font-face` still come first for their families. Fonts are only indexed at start and read when a
document uses them. It is off by default, so that a document prints the same on every machine.

Form controls show texts of their own: the labels of submit and reset buttons, "Choose File" and "No
file chosen", the placeholders and formats of date and time fields (`mm/dd/yyyy`, 12-hour time),
the decimal separator of number fields, the "Details" of a `<details>` without summary. Chrome writes
these in the language of its user interface; Fagus writes them as Chrome does in the language of the
control instead, which Chrome itself does not do: its `lang` (inherited, as `<html lang="de">`), else
the engine's `locale` (`'de-DE'`, `FAGUS_LOCALE` in Laravel), else American English. Fagus has
Chrome's texts and formats for English (`en-US`, `en-GB`), German, French, Spanish, Italian, Dutch,
Portuguese (`pt-BR`, `pt-PT`) and Polish. Text without `lang` is shaped in that language too, as
Chrome shapes it in the language of its user interface (a font's forms for the language apply: EB
Garamond sets no `fi` ligature in German).

## How it works

The PHP package starts the `fagus` binary with the first document and talks to it through pipes
([worker protocol](https://fagus.niklasforst.de/docs/protocol)). The worker keeps running while the engine lives: under
PHP-FPM it serves the documents of one request; in queue workers, under Octane (the Laravel
integration warms the engine, see [Laravel](https://fagus.niklasforst.de/docs/laravel)) and in FrankenPHP's worker mode it serves every document. HTML, fonts and PDFs travel through the pipes as they are.

The binary compresses the PDF on two threads of its own while it draws the pages, which makes large
documents a few percent faster; `RAYON_NUM_THREADS` sets how many there are.

## Without PHP

The engine also works on its own, as a command line tool and as a Rust library:

```sh
fagus render invoice.html -o invoice.pdf --font-dir fonts --factur-x invoice.xml
```

```rust
let mut engine = fagus::Engine::new();
engine.add_font_dir("fonts")?;
let output = engine.render(&fagus::Document::html(html).base_directory("assets"))?;
std::fs::write("invoice.pdf", output.pdf)?;
```

`engine.add_system_fonts()` and `engine.set_locale(...)` match the PHP options above. The Cargo
feature `parallel` (on by default) brings the compression threads: built without it
(`--no-default-features`), Fagus writes each document on one thread, and the Rust library
compresses on rayon's global pool.

## Layout of the repository

| Path | Contents |
|---|---|
| `composer.json`, `php/fagus` | The Composer package `nforst/fagus` (`Fagus\\`) |
| `php/laravel` | Its Laravel integration (`Fagus\\Laravel\\`) |
| `crates/fagus-dom` | Arena DOM, HTML parsing (html5ever) |
| `crates/fagus-css` | CSS parsing (lightningcss), selector matching, rule index |
| `crates/fagus-style` | Cascade and computed values |
| `crates/fagus-text` | Fonts, shaping (harfrust), line breaking |
| `crates/fagus-layout` | Box tree, layout, fragmentation, paged media |
| `crates/fagus-paint` | Display lists |
| `crates/fagus-pdf` | PDF output (krilla) |
| `crates/fagus` | Public Rust API |
| `crates/fagus-cli` | The `fagus` binary |
| `crates/fagus-testkit` | Reference tests, image comparison, benchmarks |
| `docs` | The [documentation](https://fagus.niklasforst.de/docs) |
| `docs/adr` | Architecture decision records |
| `docs/protocol.md` | The worker protocol between the binary and PHP |
| `docs/releasing.md` | How a release reaches GitHub and Packagist |
| `tests/corpus` | Test documents |
| `bench` | Benchmarks against other engines |
| `tools` | Scripts for references and comparisons |

The Composer package is this repository: `composer.json` is at the root, and the archives Composer
downloads hold only the PHP code (see `.gitattributes`).

## Development

```sh
cargo build --release
cargo test
cargo clippy --all-targets
cargo run -p fagus-cli -- render tests/corpus/invoice/invoice-3.html -o /tmp/invoice.pdf

composer install
vendor/bin/phpunit      # PHP and Laravel, against target/release/fagus
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

The built-in font Inter is licensed under the [SIL Open Font License 1.1](crates/fagus-text/fonts/Inter-LICENSE.txt).

The predefined counter styles and the algorithms of the complex ones, the marks of form controls,
the distribution of the height of cells spanning table rows and the spacing of East Asian punctuation and text follow Chromium and are licensed under its [BSD license](crates/fagus-layout/CHROMIUM-LICENSE). The
geometry of `shape-outside` follows Blink's shapes, licensed under [Adobe's BSD license](crates/fagus-layout/ADOBE-SHAPES-LICENSE). The vertical orientation of
characters and their classes for the spacing of East Asian text are taken from the Unicode Character Database, licensed under the [Unicode License V3](crates/fagus-layout/UNICODE-LICENSE). The glyph outlines of
Apple's `hvgl` table (PingFang on macOS) are read as HarfBuzz reads them, under its ["Old MIT" license](crates/fagus-text/HARFBUZZ-LICENSE).
The hyphenation dictionaries are Chromium's, each under the license listed in
[crates/fagus-text/hyphenation/LICENSE.txt](crates/fagus-text/hyphenation/LICENSE.txt). The user
agent style sheet follows the rendering section of the HTML Standard (WHATWG, BSD 3-Clause for
portions in source code).

[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES) collects all of these with the licenses of the Rust
crates the binary is built from (`tools/notices.sh`); it comes with every release and with the
Composer package.
