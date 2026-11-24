# Troubleshooting

## Installing the binary

### The plugin is blocked

```text
nforst/fagus contains a Composer plugin which is blocked by your allow-plugins config. You may add it to the list if you consider it safe.
```

Composer runs without asking (in CI, in Docker builds, with `--no-interaction`) and
`composer.json` does not allow the plugin. Allow it and commit the change:

```sh
composer config allow-plugins.nforst/fagus true
```

If you answered `n` when Composer asked, `composer.json` has `"nforst/fagus": false`; the plugin
then never runs and the engine reports that the binary was not found. Change it to `true`, or
install the binary with `vendor/bin/fagus-install`.

### The binary was not found

```text
The fagus binary was not found. Install it with `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`), set its path in the Engine or in the FAGUS_BINARY environment variable, or put it on the PATH.
```

There is no binary in the package, `FAGUS_BINARY` is not set and there is no `fagus` on the
`PATH`. Usually the plugin did not run (`--no-plugins`, blocked) or could not install the binary;
`composer install` then printed a warning that starts with `fagus:`. Run the installer to see why:

```sh
vendor/bin/fagus-install
php artisan fagus:install
```

Copying `vendor/` from a machine of another platform also leaves the wrong binary behind; run the
installer on the target ([deployment.md](deployment.md#docker)).

### A binary that is given does not exist

```text
The fagus binary `/usr/local/bin/fagus` could not be started. It does not exist or is not executable.
```

The path in `FAGUS_BINARY`, in `new Engine(binary: …)` or in `config/fagus.php` is wrong, or the
file is not executable (`chmod +x`). A binary that is given is never replaced by another one.

### `proc_open()` is disabled

```text
The fagus binary `/app/vendor/nforst/fagus/php/fagus/bin/fagus` could not be started. PHP does not allow proc_open() (disable_functions).
```

Remove `proc_open` (and `proc_get_status`, `proc_close`, `proc_terminate`) from
`disable_functions` in `php.ini`. Fagus runs the binary as a process and needs them.

### A development version without binaries

```text
This version of nforst/fagus (0.0.1) is a development version without built binaries. Build the binary with `cargo build --release` and set FAGUS_BINARY to it.
```

You installed a branch (such as `dev-main`) rather than a release. Only releases list the
binaries to install; the plugin installs nothing for a branch. Require a release, or build the
binary from source and set `FAGUS_BINARY` ([deployment.md](deployment.md#use-a-binary-of-your-own)).

### An unsupported platform

```text
There is no fagus binary for Linux on armv7l. Build it from source with `cargo build --release` and set FAGUS_BINARY to it.
```

Releases have binaries for Linux (x64, arm64, glibc and musl), macOS (x64, arm64), Windows (x64)
and FreeBSD (x64). Other platforms, such as OpenBSD, NetBSD or FreeBSD on arm64, need a binary
built from source.

### The download failed

```text
The fagus binary could not be downloaded from https://github.com/nforst/fagus/releases/download/v0.2.0/fagus-x86_64-unknown-linux-gnu. The server answered 404.
```

- **From a private fork**, downloads without a token fail with 404. Give Composer a token
  (`composer config --global github-oauth.github.com <token>`) or set `FAGUS_GITHUB_TOKEN`.
- **Without access to GitHub**, point `FAGUS_DOWNLOAD_URL` at a directory or URL with the files
  of the release ([deployment.md](deployment.md#servers-without-access-to-github)).
- With `FAGUS_DOWNLOAD_URL` set to a directory, the message ends in "The file cannot be read."
  when the file `fagus-<target>` is not there.

### The checksum does not match

```text
The downloaded fagus-x86_64-unknown-linux-gnu is not the binary of this release: its SHA-256 is 3f1c…, expected 9a07…. It was not installed.
```

The file is not the binary of the package's release: a file of another release in
`FAGUS_DOWNLOAD_URL`, a proxy that changed the download, or a broken download. Nothing was
installed. Put the files of the matching release into `FAGUS_DOWNLOAD_URL`, or use
`FAGUS_BINARY` for a binary of your own.

### The binary cannot be written

```text
The fagus binary could not be written to `/app/vendor/nforst/fagus/php/fagus/bin/fagus`. Check that the directory is writable; on Windows, also stop the processes that run the binary (queue workers, Octane), which keep it locked.
```

The user that runs Composer cannot write into `vendor/`, or, on Windows, a running worker locks
the old binary.

### Another protocol

```text
The fagus binary `/usr/local/bin/fagus` (version 2.0.0) speaks protocol 2; this package, nforst/fagus 0.2.0, needs protocol 1. Install the binary of the package's release with `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`), or point FAGUS_BINARY at the binary of version 0.2.0.
```

`FAGUS_BINARY` or the `PATH` points to a binary of another major release. Use the binary of the
package's release.

### Another release

A warning in the error log (in Laravel, in Laravel's log), once per process:

```text
The fagus binary `/usr/local/bin/fagus` is version 0.1.0, this package nforst/fagus 0.2.0. Both speak protocol 1, so documents render, but a binary of another release ignores options newer than itself and may lay out differently. Install the binary of the package's release with `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`), or point FAGUS_BINARY at the binary of version 0.2.0.
```

The binary works, but an option that came after its release (such as the Factur-X `version`
before 0.2.0) is ignored without an error. Install the binary of the package's release, or
update the binary that `FAGUS_BINARY` names when you update the package.

### A binary that cannot encrypt

```text
The fagus binary cannot encrypt PDFs and wrote the PDF without a password. Install the binary of the same release.
```

`encrypt()` was called, but the binary is older than encryption and left the passwords out. The
PDF is not handed out. Use the binary of the package's release.

## Rendering

### The worker did not answer in time

```text
The fagus worker did not answer within 60 seconds and was stopped.
```

A document took longer than the engine's `timeout` (60 seconds by default) between two answers.
The worker was stopped; the next document starts a new one. Raise the timeout
(`new Engine(timeout: 120.0)`, `'timeout'` in `config/fagus.php`), render the document in a
queue, or find what makes it slow with `$pdf->timings()` on a smaller version of it. Windows
has no timeout.

### The document exceeded its limits

```text
the document needs more memory than its budget of 2.0 GiB (table cells 1.8 GiB, styles 156 MiB, document tree 15 MiB)
the document took longer than its time limit of 54.0 s
```

The document needed more memory or time than one document may take ([php.md](php.md#limits),
[cli.md](cli.md#limits)), so it was not rendered: the engine throws a `RenderException` whose
diagnostics hold `limit.memory` or `limit.time`, and the worker goes on with the next document.
The message names what held most of the memory. Large images count with four bytes a pixel
whatever their file size: scale them down before they go into the document. If the document is
expected to be this large or slow, raise `memoryBudget` or `timeLimit` (`memory_budget`,
`time_limit` in Laravel, `--memory-budget` and `--time-limit` on the command line).

### A font directory does not exist

```text
The font directory `/app/resources/fonts` does not exist. Create it or remove it from the engine's font directories.
```

A directory in the `fontDirectories` of `new Engine(…)` does not exist. The engine checks them
before it starts the worker. Create the directory (in a container image too) or leave it out.
Laravel skips the directories of its `fonts` configuration that do not exist, so this does not
happen there.

### The worker stopped

```text
The fagus worker stopped. It exited with code 1. Error: reading fonts in /app/resources/fonts
```

The worker process ended. The message carries its exit code and the end of what it wrote to its
error output. Common causes:

- A font directory cannot be read (its permissions), as in the message above.
- The process was killed from outside, for example by the kernel's out-of-memory killer. A
  process ended by a signal has no exit code in the message.

The engine stops the worker; the next document starts a new one. If a document makes the worker
stop every time, please report it with the document.

### Strict mode

```text
2 diagnostics in strict mode:
<style> 1:3:5: warning[css.unknown-property]: unknown property `colr`; the declaration is ignored
resources: warning[resource.not-loaded]: image `logo.png` could not be loaded
```

`strict()` turns every diagnostic into a `RenderException`, whose `diagnostics()` lists them.
Fix them, or drop `strict()` for documents whose diagnostics you accept.

### PDF/UA requirements

```text
the document does not meet PDF/UA-1: the document has no title (add a <title>); the document has no language (add `lang` to <html>); the image `logo.png` in line 12 has no alternative text (add `alt`, empty if it is decoration); the first heading, an <h2> in line 14, is not an <h1> (headings start at h1)
```

`accessible()` checks what PDF/UA-1 requires of the document and throws a `RenderException` that
lists everything missing. Its `diagnostics()` hold what was found before, such as an image that
could not be loaded:

| Problem | Fix |
|---|---|
| the document has no title | `<title>Invoice 2026-0042</title>` |
| the document has no language | `<html lang="en">` |
| the image `…` has no alternative text | `alt="…"`, or `alt=""` for decoration |
| the SVG in line … has no alternative text | a `<title>` or `aria-label` in the `<svg>`, or `aria-hidden="true"` for decoration |
| the formula in line … has no alternative text | `alttext` on `<math>` |
| the first heading … is not an `<h1>` | start with `<h1>` |
| the `<h3>` in line … follows an `<h1>` | headings skip no level: use an `<h2>` |

### PDF/A requirements

```text
the document does not meet PDF/A-2b: PDF/A-2 allows only PDF/A files as attachments; attach `timesheet.csv` to a PDF/A-3 document instead
```

The PDF cannot meet the PDF/A level that was asked for. PDF/A-2 allows only PDF/A files as
attachments; use PDF/A-3 (`pdfA('3b')`) for other files. See [pdf.md](pdf.md). The exception's
`diagnostics()` hold what was found before the PDF was refused.

```text
the document does not meet PDF/A-1b: PDF/A-1 forbids transparency, which the document uses for `opacity` and translucent colors, first on page 1; PDF/A-2b and later allow it
```

PDF/A-1b allows no transparency. Replace what the message names with opaque colors and images,
or write PDF/A-2b, which keeps it (see [pdf.md](pdf.md#pdfa-1b)).

```text
the document does not meet PDF/A-3b: the PDF cannot be encrypted (PDF/A forbids encryption)
```

`encrypt()` goes with neither `pdfA()` nor `facturX()` (the message then names Factur-X). Leave
out one of them; see [pdf.md](pdf.md#protect-the-pdf-with-a-password). `accessible()` goes with
`encrypt()`.

### Encryption without a password

```text
Encryption needs a user password or an owner password.
```

`encrypt()` got neither password, or only empty ones. It throws a
`Fagus\Exception\InvalidArgumentException` when it is called. An unknown permission, such as
`'everything'`, throws one too, with the list of permissions.

### A password that cannot be used

```text
the PDF could not be written: the user password cannot be used: it contains a character that SASLprep (RFC 4013) does not allow in passwords, such as a control character, a private-use character or a direction mark
```

Passwords are prepared with SASLprep, as PDF 2.0 requires, which refuses control characters
(a tab, a line break), characters of the private use area, direction marks and the like, and
right-to-left letters mixed with left-to-right ones. Choose another password; the message never
shows it. See [pdf.md](pdf.md#protect-the-pdf-with-a-password).

### An unknown PDF/A level

```text
Unknown PDF/A level '4z'. The levels are '1b', '2b', '2u', '2a', '3b', '3u', '3a', '4', '4e', '4f'.
```

`pdfA()` got a string that is not a level. It throws a `Fagus\Exception\InvalidArgumentException`
when it is called, before anything is rendered. Pass one of the levels, or a `Conformance` case.

### The PDF cannot be saved

```text
The PDF could not be written to `/var/www/storage/invoices/2026-0042.pdf`.
```

The directory does not exist or is not writable. `save()` does not create directories.

### An internal error

```text
internal error while rendering: …
```

Fagus stopped where it should not. This is a bug; please report it with the document.

## Read diagnostics

Every `Pdf` carries the diagnostics of its document: what Fagus ignored or could not load, with
the position in the source.

```php
$pdf = Fagus::html($html)->baseDirectory(__DIR__.'/assets')->render();

foreach ($pdf->diagnostics() as $diagnostic) {
    echo $diagnostic, "\n";
}
```

```text
html:4: warning[resource.not-loaded]: stylesheet `css/missing.css` could not be loaded
<style> 1:1:34: warning[css.unknown-property]: unknown property `colr`; the declaration is ignored
resources: warning[resource.not-loaded]: image `https://example.com/a.png` could not be loaded
```

Each line reads `source:line:column: severity[code]: message`. The source is `<style> 1` for the
first `<style>` element, the URL of a linked or imported stylesheet, the name given to
`stylesheet()`, `html` for the markup, `resources` for loaded files and `pdf` for the output.

| Code | Meaning |
|---|---|
| `css.unknown-property` | A property Fagus does not know; the declaration is ignored. |
| `css.invalid-value` | A value the property does not take; the declaration is ignored. |
| `css.parse-error` | A rule or declaration that cannot be parsed; it is ignored. |
| `css.unparsable` | Rules or declarations the CSS parser cannot handle are left out. |
| `css.too-deep` | Blocks or functions nest too deep; the stylesheet is ignored. |
| `html.too-deep` | Elements nest hundreds of levels deep; the rest of the document is left out (severity `error`). |
| `resource.not-loaded` | A stylesheet, image, font or SVG reference could not be loaded ([resources.md](resources.md)). |
| `image.unsupported` | An image is not PNG, JPEG, GIF, WebP, BMP, TIFF, AVIF, SVG or PDF, or not a valid SVG, or a PDF that needs a password or is PDF 2.0. |
| `image.too-large` | An image has more than 200 megapixels, or a PDF image more than 128 MiB of data or content. |
| `image.broken` | An image could not be decoded and is left out, or a PDF image names a page its file does not have. |
| `font.unsupported` | A file is not a font Fagus can read, or an installed font has no glyphs it can draw. |
| `font.missing-glyph` | No font has a glyph for some characters; they are drawn as empty boxes ([fonts.md](fonts.md)). |
| `frame.limit` | The documents in frames reach their limit; later frames show nothing. |
| `layout.too-many-pages` | The document reaches the limit of 10,000 pages; what follows is left out. |
| `limit.memory` | The document needs more memory than its budget and is not rendered (severity `error`; [The document exceeded its limits](#the-document-exceeded-its-limits)). |
| `limit.time` | The document took longer than its time limit and is not rendered (severity `error`). |
| `pdf.layers-unsupported` | PDF/A-1 has no layers: what `-fagus-layer` puts on one is printed as if it had none ([extensions.md](extensions.md#layers-that-show-on-screen-or-in-print)). |
| `pdf.layer-shown` | PDF/A and PDF/UA do not let layers depend on printing: a layer with `screen` or `print` shows in both. |
| `pdf.conformance-raised` | Factur-X needs PDF/A-3; a PDF/A-2 level was raised to the PDF/A-3 level of the same letter, PDF/A-1b to PDF/A-3b. |
| `invoice.profile-mismatch` | The invoice XML declares another profile than the one given. |
| `invoice.profile-unknown` | The invoice XML declares no Factur-X profile. |

Declarations in `style` attributes are not reported. Diagnostics of documents in frames are not
reported either.

## It looks different from Chrome

Fagus aims to print like Chrome on Linux. When a PDF differs from Chrome's, check these first.

- **Fonts.** Chrome uses the fonts installed on the machine; Fagus does not by default, so that
  every machine prints the same. A family Fagus does not have falls back, in the end to the
  built-in Inter, and so does text without a `font-family`, which Chrome sets in a serif font
  ([Why is my text in Inter, or in the wrong font?](#why-is-my-text-in-inter-or-in-the-wrong-font)).
  Load the fonts with `@font-face`, put them in a font directory, or turn on
  `systemFonts` ([fonts.md](fonts.md)). Look for `resource.not-loaded` diagnostics of fonts.
- **Paper size.** Without `@page { size }`, Fagus prints A4; Chrome uses the paper size of its
  print settings, which is Letter in the United States. Set the size in the document:
  `@page { size: A4 }` or `@page { size: letter }`.
- **Margins.** Chrome's print dialog has its own margin settings. Set `@page { margin: … }` in
  the document and choose "Default" margins in Chrome to compare.
- **Print media.** Fagus applies `@media print` rules and never `@media screen` ones, as Chrome
  does when it prints. A page that looks right in the browser window can differ in print: compare
  with Chrome's print preview, not with the window. Tailwind's `print:` variants apply.
- **Width of the page.** Media queries and viewport units see the page area, not a browser
  window: A4 with Fagus's default margins is about 720 px wide, below Tailwind's `md` breakpoint
  (768 px). In Chrome's print preview, the page area is below it too.
- **Backgrounds.** Fagus prints backgrounds and shadows. Turn on "Background graphics" in
  Chrome's print dialog to compare.
- **Headers and footers.** Chrome adds the date, title and URL in its own headers and footers;
  turn them off in the print dialog. Fagus prints only the margin boxes of `@page`.
- **Scripts.** Fagus runs no JavaScript. Content that scripts create in the browser (charts,
  client-side templates) is missing; render it on the server.
- **Remote resources.** Images, fonts and stylesheets with `https://` URLs are not loaded
  ([resources.md](resources.md)).
- **Form controls.** Fagus writes the texts of form controls (button labels, date formats) in
  the language of the control's `lang`, else the engine's `locale`; Chrome writes them in the
  language of its user interface.

[css.md](css.md) lists where Fagus deliberately differs from Chrome, and
[paged-media.md](paged-media.md) describes pages, margins and headers.

## FAQ

### How do I add page numbers, headers and footers?

With `@page` margin boxes in the document. A running element repeats an element of the
document in a margin box:

```css
@page {
  @top-left { content: element(letterhead); }
  @bottom-right { content: "Page " counter(page) " of " counter(pages); }
}
.letterhead { position: running(letterhead); }
```

See [paged-media.md](paged-media.md).

### Why is my text in Inter, or in the wrong font?

Fagus uses only the fonts you give it. A family it does not have falls back to the built-in Inter,
and so do `serif` and text without any `font-family`: Fagus has no serif font. With fonts in a font
directory, the fallback is the first font found there instead (in alphabetical order of the
files' paths), which may not be the one you expect. Name the family in your CSS
(`body { font-family: "Source Serif 4" }`), or set the generic families on the engine
([fonts.md](fonts.md#set-the-generic-families)):

```php
new Engine(fontDirectories: [...], serif: 'Source Serif 4', sansSerif: 'Inter');
```

In Laravel, set `families` in `config/fagus.php`.

### How do I force a page break?

```css
.page-break { break-before: page; }
```

### Can I fetch images from a URL?

Not through Fagus. Hand them over with `resource()`, or answer the URLs with a `resolver()` that
fetches what you allow ([resources.md](resources.md#answer-requests-with-resolver)).

### Can Fagus render a page of my running site?

No. Fagus renders HTML that you give it. Render the view on the server (in Laravel,
`Fagus::view()`), and make its files available through the base directory.

### Migrating from Dompdf

Dompdf supports CSS 2.1 with some additions, without flex or grid. With Fagus, layouts with flex,
grid, `calc()` and custom properties work as in Chrome, so the table-based workarounds can go.

| Dompdf | Fagus |
|---|---|
| `$dompdf->loadHtml($html); $dompdf->render(); $dompdf->output()` | `Fagus::html($html)->render()->bytes()` |
| `$dompdf->setPaper('A4', 'landscape')` | `@page { size: A4 landscape }` |
| `$dompdf->stream('invoice.pdf')` | `Fagus::html($html)->name('invoice.pdf')->inline()` (Laravel), or send `bytes()` |
| `chroot` | `baseDirectory()` and `root()` |
| `isRemoteEnabled` | not available; `resource()` or `resolver()` |
| `<script type="text/php">` for page numbers | `counter(page)` and `counter(pages)` in `@page` margin boxes |
| `isPhpEnabled`, `isJavascriptEnabled` | not available |
| barryvdh/laravel-dompdf: `Pdf::loadView('invoice', $data)->download('invoice.pdf')` | `Fagus::view('invoice', $data)->download('invoice.pdf')` |
| `->stream()` | `->inline()`, or return the `PendingPdf` |
| `->save($path)` | `->save($path)`, or `->store($path, $disk)` |

Fonts that Dompdf loaded from `@font-face` load the same way; fonts registered in its font
cache go into a font directory.

### Migrating from mPDF

mPDF has its own HTML and CSS support, with page features written as custom tags and
placeholders. Fagus uses standard CSS for them.

| mPDF | Fagus |
|---|---|
| `new Mpdf(['format' => 'A4', 'margin_left' => 20, …])` | `@page { size: A4; margin: 20mm }` |
| `$mpdf->WriteHTML($html); $mpdf->Output('invoice.pdf', 'F')` | `Fagus::html($html)->save('invoice.pdf')` |
| `$mpdf->Output('', 'S')` | `Fagus::html($html)->render()->bytes()` |
| `SetHTMLHeader()`, `SetHTMLFooter()`, `<htmlpageheader>` | `@page` margin boxes with running elements |
| `{PAGENO}`, `{nbpg}` | `counter(page)`, `counter(pages)` |
| `<pagebreak>` | `break-before: page` |
| `PDFA`, `PDFAauto` | `pdfA('3b')` |
| `SetAssociatedFiles()` | `attach()` or `facturX()` |
| `fontDir`, `fontdata` | `@font-face`, or `fontDirectories` on the engine |
| `SetTitle()`, `SetAuthor()` | `<title>`, `<meta name="author">` |
| `SetWatermarkText()` | a positioned element or a background in the document |
| `SetDocTemplate('letterhead.pdf')` | `@page { background: url(letterhead.pdf) no-repeat }`, positioned by minus the margins ([page templates](paged-media.md#use-a-pdf-as-page-template)) |
| `SetDocTemplate('letterhead.pdf', true)` (the template's pages in turn) | `@page :nth(2n) { background-image: url(letterhead.pdf#page=2) }`, and so on for more pages |
| `SetPageTemplate()`, `UseTemplate()` of one page | `@page :first { background-image: url(letterhead.pdf#page=1) }`, or a named page |

### Migrating from Browsershot, Snappy or other browser-based tools

Browsershot (and spatie/laravel-pdf on top of it) runs Chrome through Puppeteer and Node;
Snappy runs wkhtmltopdf, an old WebKit. Fagus needs neither. HTML and CSS that print well in
Chrome print the same in Fagus; what changes is everything around them:

| Browsershot / laravel-pdf / Snappy | Fagus |
|---|---|
| `Browsershot::html($html)->format('A4')->margins(…)->save('x.pdf')` | `@page { size: A4; margin: … }` and `Fagus::html($html)->save('x.pdf')` |
| `Pdf::view('invoice', $data)->format('a4')->name('invoice.pdf')` | `Fagus::view('invoice', $data)->name('invoice.pdf')` |
| `->showBackground()` | not needed: backgrounds always print |
| `->headerHtml()`, `->footerHtml()`, `--header-html` with `pageNumber` and `totalPages` | `@page` margin boxes with `counter(page)` and `counter(pages)` |
| `->emulateMedia('screen')` | not available: Fagus applies print styles; move rules out of `@media screen` |
| `->waitUntilNetworkIdle()`, `->setDelay()`, JavaScript | not available: no network, no scripts |
| `Browsershot::url('https://…')`, `$snappy->generate($url, …)` | render the HTML yourself and pass it to `Fagus::html()` |
| `$snappy->getOutputFromHtml($html)` | `Fagus::html($html)->render()->bytes()` |
| Assets loaded from the running site | files through `baseDirectory()`, or `resource()` and `resolver()` |

With spatie/laravel-pdf, you can also keep the code as it is and switch to its `fagus` driver
(see [laravel.md](laravel.md#use-fagus-with-spatielaravel-pdf)).

For Snappy and wkhtmltopdf, [Coming from wkhtmltopdf](from-wkhtmltopdf.md) maps its flags to CSS and to the PHP API.

Remove Chrome, Node, Puppeteer or wkhtmltopdf from the servers once nothing else needs them.
