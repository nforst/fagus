# PHP API

The PHP API lives in the namespace `Fagus\` (`php/fagus/src`). It has four parts:

- `Fagus`: static entry points and the shared engine.
- `Document`: a document to render, built step by step.
- `Pdf`: the rendered PDF with its page count, diagnostics and timings.
- `Engine`: the `fagus` binary, running as a worker process.

In Laravel, use the facade described in [laravel.md](laravel.md); it offers the same document
methods and renders with the engine of `config/fagus.php`.

```php
use Fagus\Conformance;
use Fagus\Fagus;
use Fagus\Relationship;

$pdf = Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf')   // relative URLs and the files they may read
    ->root(__DIR__.'/resources')                 // a larger directory files may come from (../fonts/…)
    ->stylesheet($css)                           // after the document's own styles
    ->resource('logo.png', $logo)                // data the document refers to
    ->resolver(fn (string $url): ?string => $cache->get($url))
    ->pdfA(Conformance::PdfA3b)
    ->attach('data.csv', $csv, 'text/csv', Relationship::Supplement, 'Raw data')
    ->facturX($invoiceXml)                       // ZUGFeRD / Factur-X; the profile comes from the XML
    ->created(new DateTimeImmutable('2026-11-02 12:00')) // a fixed date makes the PDF reproducible
    ->strict()                                   // diagnostics become exceptions
    ->render();

$pdf->bytes();
$pdf->pages();
$pdf->diagnostics();  // CSS that was ignored, resources that are missing, with their position
$pdf->timings();      // milliseconds per phase: parse, style, layout, paint, pdf
$pdf->save('invoice.pdf');
$pdf->inline('invoice.pdf');   // or send it to the browser: headers and PDF
```

## `Fagus`

`Fagus\Fagus` is a final class with static methods only.

### `Fagus::html(string $html): Document`

Starts a document from HTML, parsed as browsers parse HTML. Same as `Document::html()`.

```php
Fagus::html('<h1>Invoice</h1>')->save('invoice.pdf');
```

### `Fagus::xhtml(string $xml): Document`

Starts a document from XHTML, parsed as XML as browsers parse `.xhtml` files: CDATA sections and
empty-element tags (`<div/>`) work, names are case-sensitive and nothing is implied (no `<tbody>`
added, no tags closed for you). Same as `Document::xhtml()`.

```php
Fagus::xhtml('<html xmlns="http://www.w3.org/1999/xhtml"><body><p>Invoice</p><div/></body></html>')
    ->save('invoice.pdf');
```

### `Fagus::engine(): Engine`

The engine that documents render with unless one is passed to `render()` or `save()`: the one set
with `useEngine()`, else one created on first use with the default settings (the binary from
`FAGUS_BINARY`, the installed one or the `PATH`, no font directories, no system fonts).

```php
Fagus::engine()->version(); // "1.0.0"
```

### `Fagus::useEngine(?Engine $engine): void`

Sets the shared engine, for example to configure fonts once at bootstrap. `null` drops it; the
next call to `engine()` creates a new default engine.

```php
use Fagus\Engine;
use Fagus\Fagus;

Fagus::useEngine(new Engine(
    fontDirectories: [__DIR__.'/resources/fonts'],
    locale: 'de-DE',
));
```

## `Document`

`Fagus\Document` is immutable: every method returns a changed copy and the document itself never
changes. You can build a base document once and derive variants from it.

```php
$base = Fagus::html($html)->baseDirectory(__DIR__.'/resources/pdf');

$base->save('invoice.pdf');
$base->pdfA('3b')->save('invoice-archive.pdf');
```

### `Document::html(string $html): Document`

A document from HTML. The constructor is private; use this or `Fagus::html()`.

### `Document::xhtml(string $xml): Document`

A document from XHTML, parsed as XML (see `Fagus::xhtml()`).

### `baseDirectory(string $directory): Document`

The directory that relative URLs (stylesheets, images, fonts, frames) resolve against. Only files
inside it are read, unless `root()` allows a larger directory. Without a base directory, Fagus
reads no files at all. See [resources.md](resources.md).

```php
Fagus::html('<img src="images/logo.png" alt="ACME">')
    ->baseDirectory(__DIR__.'/resources/pdf');   // reads resources/pdf/images/logo.png
```

### `root(string $directory): Document`

The directory that files may be read from. It must contain the base directory. Use it when the
document's URLs lead out of the base directory (`../fonts/Inter.woff2`). URLs that start with `/`
resolve against the root, as on a web server. Without `root()`, the root is the base directory.

```php
Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf/invoices')
    ->root(__DIR__.'/resources/pdf');   // allows ../fonts/… and /images/logo.png
```

### `stylesheet(string $css, string $name = 'stylesheet'): Document`

Adds a stylesheet after the document's own stylesheets, so its rules win over the document's
rules of the same specificity. It can be called several times; the stylesheets apply in that
order. `$name` is the source that diagnostics name for it.

```php
Fagus::html($html)->stylesheet(file_get_contents(__DIR__.'/pdf.css'), 'pdf.css');
```

Relative URLs in it resolve against the base directory.

### `resource(string $url, string $data): Document`

Hands over the data of a resource that the document refers to, by its URL exactly as written in
the document. Use it for data that is not in a file, such as a logo from a database or a chart
generated in PHP. A later call with the same URL replaces the data.

```php
Fagus::html('<img src="logo.png" alt="ACME"><img src="chart.svg" alt="Revenue">')
    ->resource('logo.png', $tenant->logo)
    ->resource('chart.svg', $chartSvg);
```

URLs in stylesheets are resolved against the stylesheet first: `url(img/bg.png)` in
`css/main.css` asks for `css/img/bg.png`. See [resources.md](resources.md).

### `resolver(callable $resolver): Document`

Sets a callback that answers requests for resources that are not handed over with `resource()`.
It gets the URL (resolved as described for `resource()`) and returns the data, or `null` when it
does not have it. Fagus asks the callback before it looks for a file inside the root; after
`null`, it tries the file. It is asked once for each URL in a render. The engine waits while the
callback runs.

```php
Fagus::html($html)->resolver(function (string $url): ?string {
    if (str_starts_with($url, 'media://')) {
        return Storage::get('media/'.substr($url, 8));
    }

    return null;
});
```

Fagus itself never goes to the network. A resolver may, under your own rules. Only one resolver
is kept; a second call replaces the first.

### `pdfA(Conformance|string $level = Conformance::PdfA3b): Document`

Writes the PDF as PDF/A. `$level` is a `Conformance` case or its value: `'1b'`, `'2b'`, `'2u'`,
`'2a'`, `'3b'`, `'3u'`, `'3a'`, `'4'`, `'4e'` or `'4f'` (case-insensitive). Any other string
throws a `Fagus\Exception\InvalidArgumentException` that lists the levels ("Unknown PDF/A level
'4z'. The levels are '1b', '2b', '2u', '2a', '3b', '3u', '3a', '4', '4e', '4f'."). The levels
ending in `a` are tagged for accessibility. PDF/A-1b refuses transparency, and neither PDF/A-1b
nor PDF/A-4 goes with `accessible()`. See [pdf.md](pdf.md#write-pdfa).

```php
Fagus::html($html)->pdfA();      // PDF/A-3b
Fagus::html($html)->pdfA('2u');
Fagus::html($html)->pdfA('4f');  // PDF 2.0, attachments allowed
```

### `tagged(bool $tagged = true): Document`

Whether the PDF is tagged with the logical structure of the document (headings, paragraphs,
lists, tables, images and links in reading order), which screen readers read. Tagging is on by
default. Without it, a PDF with many table cells is somewhat smaller and faster to write.
`accessible()` and the PDF/A levels ending in `a` tag the PDF whatever `tagged()` says.

```php
Fagus::html($html)->tagged(false);
```

### `accessible(bool $accessible = true): Document`

Writes the PDF as PDF/UA-1, the standard for accessible PDF. The document needs a `<title>`, a
language (`<html lang>`), alternative text for every image (`alt`, empty for decoration) and
headings that start at `<h1>` and skip no level. Without them, `render()` throws a
`RenderException` that lists what is missing:

```text
the document does not meet PDF/UA-1: the document has no title (add a <title>); the image `logo.png` in line 12 has no alternative text (add `alt`, empty if it is decoration)
```

`accessible()` also works together with PDF/A-2a, PDF/A-3a and Factur-X, but not with PDF/A-1b
and PDF/A-4, which are based on other PDF versions than PDF/UA-1. See [pdf.md](pdf.md).

### `forms(bool $forms = true): Document`

Makes the form controls of the document the fields of an interactive form, which a reader fills
in in a PDF viewer: text fields, text areas, checkboxes, radio buttons, drop-down lists, list
boxes (also `<select multiple>`) and the buttons that submit or reset a `<form>`, named after
their `name`, with their values and with `readonly`, `disabled`, `required` and `maxlength`. A
submit button sends the fields to the form's `action`, which must be an absolute URL; controls in
transformed boxes and on turned pages become fields too. The PDF looks the same as without it. Off by default, as Chrome prints controls as
they look; controls in an `inert` element stay so. See [pdf.md](pdf.md#fill-in-forms).

```php
Fagus::html('<label>Name <input name="name" required></label>')->forms()->save('form.pdf');
```

### `attach(string $name, string $data, string $mime = 'application/octet-stream', Relationship $relationship = Relationship::Unspecified, ?string $description = null): Document`

Attaches a file to the PDF. `$relationship` says how the file relates to the document (PDF 2.0
`AFRelationship`, see `Relationship` below). PDF/A-3 and PDF/UA require a description of every
attached file; without `$description`, the file name describes it. Call it once per file.

```php
use Fagus\Relationship;

Fagus::html($html)
    ->pdfA('3b')
    ->attach('timesheet.csv', $csv, 'text/csv', Relationship::Supplement, 'Hours worked');
```

PDF/A-2 allows only PDF/A files as attachments: with `pdfA('2b')` and a CSV attachment, `render()`
throws a `RenderException` ("the document does not meet PDF/A-2b: PDF/A-2 allows only PDF/A files
as attachments; attach `timesheet.csv` to a PDF/A-3 document instead"). Use PDF/A-3 for other
files. PDF/A-4 refuses them the same way (use PDF/A-4f), and PDF/A-1b allows none.

### `facturX(string $xml, ?InvoiceProfile $profile = null, ?string $version = null, ?string $description = null): Document`

Makes the PDF an electronic invoice of ZUGFeRD or Factur-X: the XML is attached under the name
its profile requires (`factur-x.xml`, or `xrechnung.xml` for XRechnung), described in the XMP
metadata, and the PDF conforms to PDF/A-3 (PDF/A-3b unless another PDF/A-3 level is chosen; a
PDF/A-4 level throws a `RenderException`, as Factur-X requires PDF/A-3). The
profile is the one the XML declares in its guideline parameter (EN 16931 if it declares none),
unless `$profile` is given. `$version` is the `Version` of the XMP metadata (`'1.0'` by default),
`$description` the description of the attached XML (`'Factur-X/ZUGFeRD invoice'` by default); an
empty one throws an `InvalidArgumentException`.

```php
use Fagus\InvoiceProfile;

Fagus::html($html)->facturX($xml);                            // profile from the XML
Fagus::html($html)->facturX($xml, InvoiceProfile::XRechnung); // profile given
Fagus::html($html)->facturX($xml, version: '3.0', description: 'XRechnung Invoice');
```

Diagnostics report a profile that differs from the XML's (`invoice.profile-mismatch`), an XML
without a profile (`invoice.profile-unknown`) and a PDF/A-1 or PDF/A-2 level raised to PDF/A-3
(`pdf.conformance-raised`). See [pdf.md](pdf.md).

### `encrypt(?string $userPassword = null, ?string $ownerPassword = null, ?array $permissions = null): Document`

Protects the PDF with passwords: it is encrypted with AES-256, as PDF 2.0 defines it. The user
password is needed to open the PDF; without one, it opens without a password but with only the
permissions given. The owner password lifts those restrictions; without one, a random password
nobody knows is used. At least one of them is needed (an empty string counts as none), or an
`InvalidArgumentException` is thrown.

`$permissions` is a list of `Permission` cases or their values (`'print'`, `'copy'`, …) for what
the reader may do without the owner password: everything if `null`, nothing beyond reading if
empty. An unknown value throws an `InvalidArgumentException`.

```php
use Fagus\Permission;

Fagus::html($html)->encrypt(userPassword: $password);                  // to open; everything allowed
Fagus::html($html)->encrypt(userPassword: $password, ownerPassword: $admin, permissions: [Permission::Print]);
Fagus::html($html)->encrypt(ownerPassword: $admin, permissions: []);  // opens for everyone, read only
```

Passwords are prepared with SASLprep (RFC 4013), as PDF 2.0 requires, so that a password opens
the PDF however its characters were typed (`ä` composed or decomposed, a no-break space or a
space); a password with a character SASLprep forbids, such as a control character, makes
`render()` throw a `RenderException`. The bytes of an encrypted PDF differ from run to run, since
its keys are random. PDF/A forbids encryption: with `pdfA()` or `facturX()`, `render()` throws a
`RenderException`. `accessible()` and `encrypt()` go together: assistive technology may always
extract the text, as PDF/UA requires. The passwords are marked `#[\SensitiveParameter]`, so stack traces leave
them out. See [pdf.md](pdf.md#protect-the-pdf-with-a-password).

### `created(DateTimeInterface $date): Document`

The creation date written into the PDF; the current time by default. Fagus converts it to UTC. A
fixed date makes the output reproducible: the same document, fonts and binary give the same bytes.

```php
Fagus::html($html)->created(new DateTimeImmutable('2026-11-02 12:00', new DateTimeZone('Europe/Berlin')));
```

### `sign(Signature $signature): Document`

Signs the PDF digitally (PAdES), also an encrypted one, see [`Signature`](#signature) below and
[pdf.md](pdf.md#sign-the-pdf).

```php
use Fagus\Signature;

Fagus::html($html)->sign(Signature::fromPkcs12(storage_path('signing.p12'), $password));
```

### `strict(bool $strict = true): Document`

Fails with a `RenderException` when the PDF has diagnostics (CSS that is ignored, resources that
are missing), instead of returning it. Useful in tests and CI.

```php
Fagus::html($html)->strict()->render();
```

```text
2 diagnostics in strict mode:
<style> 1:3:5: warning[css.unknown-property]: unknown property `colr`; the declaration is ignored
resources: warning[resource.not-loaded]: image `logo.png` could not be loaded
```

### `reportLayout(bool $reportLayout = true): Document`

Reports where the elements with an `id` ended up, which [`Pdf::layout()`](#find-where-elements-landed)
returns: their pages and boxes, and the rows of their tables. Off by default, as finding them takes
a little time; a document rendered without it renders as fast as before.

### `render(?Engine $engine = null): Pdf`

Renders the document with `$engine`, or with `Fagus::engine()`. Throws a `RenderException` when
the document cannot be rendered (or has diagnostics in strict mode) and an `EngineException` when
the engine cannot be started or stops.

```php
$pdf = Fagus::html($html)->render();
$pdf = Fagus::html($html)->render($myEngine);
```

### `save(string $path, ?Engine $engine = null): Pdf`

Renders the document and writes the PDF to `$path`. Returns the `Pdf`, so you can still read its
page count and diagnostics.

```php
$pdf = Fagus::html($html)->save(storage_path('invoices/2026-0042.pdf'));
```

### `inline(string $filename = 'document.pdf', ?Engine $engine = null): Pdf`

Renders the document and sends the PDF to the browser to show it, as [`Pdf::inline()`](#pdf) does:
the headers and the PDF in one call. Returns the `Pdf`.

```php
Fagus::html($html)->inline('invoice.pdf');
```

### `download(string $filename = 'document.pdf', ?Engine $engine = null): Pdf`

Renders the document and sends the PDF to the browser as a download, as `Pdf::download()` does.

```php
Fagus::html($html)->download("invoice-{$number}.pdf");
```

In Laravel, return `Fagus::view(…)->inline()` or `->download()` from the controller instead
([laravel.md](laravel.md)); they make a response, which these methods do not.

### What comes from the document

There are no methods for page size, margins, headers, footers or metadata. As in a browser, they
come from the document:

| PDF | From |
|---|---|
| Page size and margins | `@page { size: A4; margin: 20mm }` ([paged-media.md](paged-media.md)) |
| Headers, footers, page numbers | `@page` margin boxes |
| Title | `<title>` |
| Language | `<html lang>`, else the engine's `locale` |
| Author | `<meta name="author" content="…">` |
| Subject | `<meta name="description" content="…">` |
| Keywords | `<meta name="keywords" content="…">`, separated by commas |
| Outline (bookmarks) | the headings (`bookmark-level`, `bookmark-label`, `bookmark-state`) |

## `Pdf`

`Fagus\Pdf` is a final readonly class: a rendered PDF with what Fagus noticed while rendering it.

| Method | Returns |
|---|---|
| `bytes(): string` | The PDF file. |
| `pages(): int` | The number of pages. |
| `diagnostics(): list<Diagnostic>` | CSS that was ignored, resources that could not be loaded, and other notes, in the order they were found. Empty for a clean document. |
| `timings(): array<string, float>` | Milliseconds per phase, with the keys `parse`, `style`, `layout`, `paint` and `pdf`. |
| `layout(): Layout` | Where the elements with an `id` ended up (see [Find where elements landed](#find-where-elements-landed)). Throws a `FagusException` if the document was rendered without `reportLayout()`. |
| `save(string $path): void` | Writes the PDF to `$path`. Throws a `FagusException` ("The PDF could not be written to `…`.") if it cannot. |
| `inline(string $filename = 'document.pdf'): void` | Sends the PDF to the browser to show it: `Content-Type: application/pdf`, `Content-Disposition: inline` with the file name, `Content-Length`, then the PDF. A name that is not ASCII goes as `filename*` with an ASCII fallback; control characters and slashes are left out. Throws a `FagusException` if output was already sent, since the headers cannot follow it. |
| `download(string $filename = 'document.pdf'): void` | As `inline()`, with `Content-Disposition: attachment`: the browser downloads the PDF. |

```php
$pdf = Fagus::html($html)->render();

file_put_contents('invoice.pdf', $pdf->bytes());
echo array_sum($pdf->timings()), " ms\n";

foreach ($pdf->diagnostics() as $diagnostic) {
    error_log((string) $diagnostic);
}
```

The constructor is public, `new Pdf(string $bytes, int $pages, array $diagnostics = [], array $timings = [], ?Layout $layout = null)`,
so tests can make a `Pdf` without rendering.

### Find where elements landed

With `reportLayout()`, the PDF tells where each element with an `id` ended up: on which pages, in
which boxes, and for a table which rows went on which page. It answers what a script in a browser
would measure, after the pages are laid out:

```php
use Fagus\Layout\Section;

$pdf = Fagus::html($html)->reportLayout()->render();
$layout = $pdf->layout();

$total = $layout->element('total');            // Layout\Element, or null
$total->lastPage();                             // 3
$total->boxes[0]->y;                            // 412.5, points from the top of the page
$total->boxes[0]->bottom();                     // 427.5

foreach ($layout->element('items')->rows as $row) {
    if ($row->section === Section::Body) {
        echo "row {$row->index} on page ", implode(' and ', $row->pages()), "\n";
    }
}
```

All lengths are in PDF points (1/72 inch), measured from the top left corner of the page box (the
trim box of a page with bleed), before `page-orientation` turns the page and before `transform`.

| Class | What it holds |
|---|---|
| `Fagus\Layout` | `pages` (`list<Layout\Page>`), `elements` (`list<Layout\Element>`, in document order); `element(string $id): ?Element` (the first with the `id`), `page(int $number): ?Page`. |
| `Fagus\Layout\Page` | `number` (from 1), `width`, `height`. |
| `Fagus\Layout\Element` | `id`, `tag` (`td`, `div`), `boxes` (`list<Box>`, in page order), `rows` (`list<Row>`, for tables); `pages(): list<int>`, `firstPage(): ?int`, `lastPage(): ?int`, `rowsOnPage(int $page): list<Row>`. |
| `Fagus\Layout\Box` | `page` (from 1), `x`, `y`, `width`, `height` of the border box; `bottom()`, `right()`. |
| `Fagus\Layout\Row` | `section` (`Layout\Section::Header`, `Body` or `Footer`), `index` (its place among the rows of that section, from 0), `id`, `boxes`; `pages(): list<int>`. |

- An element has a box for each page it is on, and an inline element one for each line. A header
  or footer that repeats, a fixed box and a running element in the page margins have one on each
  page they show on. An element that is not rendered (`display: none`) has none.
- Every element with an `id` is reported, also those inside tables; give an element an `id` to
  find it. Tables list all their rows (`<tr>`), with or without an `id`: a row's box is the area of
  its cells on that page, two boxes for a row that breaks across pages.
- Numbers are rounded to a hundredth of a point.

#### Render twice to adjust the layout

What other tools do with a script after layout, you do with two renders: render, read where things
landed, change the document and render again. Here the notes box fills what is left of the last
page, so that the signature line sits at its bottom:

```php
$document = Fagus::html($html)->reportLayout();
$layout = $document->render()->layout();

$notes = $layout->element('notes');
$page = $layout->page($notes->lastPage());
$box = $notes->boxes[count($notes->boxes) - 1];
// The page margins are 20mm (56.69pt); the signature line below the notes is 40pt tall.
$left = $page->height - 56.69 - 40 - $box->bottom();

$pdf = $document
    ->stylesheet(sprintf('#notes { min-height: %.2fpt }', $box->height + max(0, $left)))
    ->render();
```

The same works for a table whose last page holds only the totals (move a row there with
`break-before: page`), a cover whose title must fit on one line, or an index of where each section
starts. Rendering is deterministic, so the second render lays out everything that did not change
exactly as the first. To make a page as tall as its content, there is no need for this:
`-fagus-page-height: fit-content` does it in one render (see
[Extensions](extensions.md#pages-that-grow-with-their-content)).

## `Engine`

`Fagus\Engine` is the `fagus` binary, running as a worker process. Configure one when the
defaults do not fit and make it the shared engine with `Fagus::useEngine()`, or pass it to
`render()`.

```php
use Fagus\Engine;
use Fagus\Fagus;

Fagus::useEngine(new Engine(
    binary: '/usr/local/bin/fagus',           // FAGUS_BINARY, the installed binary or the PATH if null
    fontDirectories: [__DIR__.'/resources/fonts'],
    sansSerif: 'Inter',                       // the families of sans-serif, serif and monospace
    serif: 'Source Serif 4',
    monospace: 'JetBrains Mono',
    timeout: 60.0,                            // seconds to wait for each answer of the worker
    systemFonts: false,
    locale: 'de-DE',
));
```

### Constructor

All arguments are optional and named as below.

| Argument | Type, default | Meaning |
|---|---|---|
| `binary` | `?string`, `null` | The path of the `fagus` binary. If `null`: `FAGUS_BINARY`, else the binary the plugin installed into the package, else `fagus` (`fagus.exe` on Windows) on the `PATH`. A binary that is given here or in `FAGUS_BINARY` must exist and be executable. |
| `fontDirectories` | `list<string>`, `[]` | Directories with fonts for every document: TTF, OTF, TTC, OTC, WOFF and WOFF2 files, in the directory and its subdirectories. Each directory must exist: before it starts the worker, the engine throws an `EngineException` that names a missing one ("The font directory `…` does not exist."). |
| `sansSerif` | `?string`, `null` | The family that `sans-serif` names. |
| `serif` | `?string`, `null` | The family that `serif` names. |
| `monospace` | `?string`, `null` | The family that `monospace` names. |
| `timeout` | `float`, `60.0` | Seconds to wait for each answer of the worker. A worker that does not answer in time is stopped and an `EngineException` is thrown. Not on Windows, where PHP cannot wait for a pipe with a time limit. |
| `systemFonts` | `bool`, `false` | Whether the fonts installed on the machine serve too, as in Chrome. The output then depends on the machine. |
| `locale` | `?string`, `null` | The language of the texts that form controls show of their own (button labels, date formats) and of the PDF where a document has no `lang`, such as `'de-DE'`. American English if `null`. |
| `maxDocuments` | `?int`, `null` | The documents a worker renders before the engine replaces it with a new one, which takes 3 to 4 ms. No limit if `null`. See [deployment.md](deployment.md#restart-workers). |
| `maxMemory` | `?int`, `null` | Megabytes of resident memory: a worker that has more after a document is replaced with a new one. The worker tells its size on Linux (now) and macOS (its peak); on Windows the limit does not apply. No limit if `null`. |
| `onWarning` | `?Closure(string): void`, `null` | Receives the engine's warnings: so far, that the binary is of another release than the package ([protocol.md](protocol.md#versions)). If `null`, they go to PHP's error log (`error_log()`), never to the output. The Laravel integration writes them to Laravel's log. |
| `sandbox` | `bool`, `true` | Whether the worker renders in its sandbox on Linux, without network, programs or writing files ([cli.md](cli.md#sandbox)). `false` starts it with `--no-sandbox`. |
| `memoryBudget` | `?int`, `null` | Megabytes one document may take, all its parts together: images, layout, fonts, resources ([limits](#limits)). A document that needs more throws a `RenderException` with the diagnostic `limit.memory`. 2048 if `null`, none if `0`. |
| `timeLimit` | `?float`, `null` | Seconds one document may take in all. A document that takes longer throws a `RenderException` with the diagnostic `limit.time`, and the worker goes on. A tenth less than `timeout` if `null`; none if `0`. |

Form controls write their texts (the labels of submit and reset buttons, "Choose File", the
placeholders and formats of date and time fields, the decimal separator of number fields) as
Chrome does, but in the language of the control: its `lang` (inherited, as from
`<html lang="de">`), else `locale`, else American English. Chrome itself takes the language of
its user interface. Fagus has Chrome's texts for English (`en-US`, `en-GB`), German, French,
Spanish, Italian, Dutch, Portuguese (`pt-BR`, `pt-PT`) and Polish. Text without `lang` is shaped
in the `locale` language too.

Fonts, generic families and system fonts are described in [fonts.md](fonts.md).

### Limits

`timeout` and `timeLimit` both bound time, in two ways:

- `timeout` is how long the engine waits for each answer of the worker. When it runs out, the
  engine stops the worker process (it may be stuck) and throws an `EngineException`; the next
  document starts a new worker. A document that asks for many resources through a `resolver()`
  gets the whole `timeout` for each answer. It does not apply on Windows.
- `timeLimit` is how long one document may take in the worker, from start to end, including the
  time the worker waits for your resolver. When it runs out, the worker ends the document itself
  and answers with an error: the engine throws a `RenderException` whose diagnostics hold
  `limit.time`, and the same worker renders the next document. It applies on every system.

By default `timeLimit` is a tenth less than `timeout` (54 seconds for 60), so that a document
that takes too long ends with a `RenderException` before the engine has to stop the worker. Set
it higher, or to `0` for none, for documents that ask for many resources and take longer than
`timeout` in all.

`memoryBudget` bounds what a document holds: decoded images (four bytes a pixel), the pixels of
filters and shadows, layout boxes and fragments, table cells, computed styles, the document
tree, its fonts and its resources (each content once, however many URLs it has). It is an
estimate, not the worker's resident memory, which is larger (fonts, caches, what the allocator
keeps); `maxMemory` and the system's limits stay the last line ([deployment.md](deployment.md#untrusted-documents)).
The limits never change a PDF: a document within them renders as without them.

```php
new Engine(memoryBudget: 1024, timeLimit: 20.0);   // in Laravel: memory_budget and time_limit
```

The constructor does not start the worker. The worker starts with the first call that needs it
(`render()`, `addFont()`, `version()`).

### `render(Document $document): Pdf`

Renders a document. `$document->render($engine)` calls this.

### `addFont(string $data): int`

Adds a font (TTF, OTF, TTC, OTC, WOFF or WOFF2 data) for all later documents of this engine and returns the number
of faces in it. The engine keeps the data and hands it to every new worker it starts. Data that is
not a font throws an `EngineException` ("the data is not a supported font").

```php
Fagus::engine()->addFont(file_get_contents(__DIR__.'/resources/fonts/CorporateSans.otf'));
```

### `version(): string`

The version of the running binary, such as `1.0.0`. Starts the worker if it is not running.

### `sandbox(): string`

How much of its [sandbox](cli.md#sandbox) the running worker is in: `on`, `partial` (the kernel
lacks Landlock, as before Linux 5.13, or seccomp) or `off` (turned off, or not Linux).
Starts the worker if it is not running.

### `close(): void`

Stops the worker process. It ends when its input closes; one that has not ended after two seconds
is terminated. The next document starts a new worker, which gets the fonts of `addFont()` again.
The destructor calls `close()`.

### `Engine::PROTOCOL`

The version of the worker protocol this package speaks: `1`. A binary that speaks another
protocol is refused with an `EngineException` that names both versions
([troubleshooting.md](troubleshooting.md#another-protocol)). A binary of another release that
speaks the same protocol renders, but ignores options newer than itself: the engine warns about
it once per process and binary, through `onWarning` ([protocol.md](protocol.md#versions)).

### The worker's lifetime

The worker process starts with the first document and keeps running, with its fonts and caches
warm, until the engine is closed or destroyed:

- Under PHP-FPM, the shared engine lives as long as the request, so the worker serves the
  documents of one request.
- In queue workers, under Octane or FrankenPHP's worker mode and in long-running scripts, it
  serves every document of the process.

When a render fails with an `EngineException` (a timeout, a worker that exited), the worker is
stopped and the next document starts a new one. A `RenderException` leaves the worker running.
With `maxDocuments` or `maxMemory`, the engine also closes a worker after the document that
reaches the limit, and the next document starts a new one.
More in [deployment.md](deployment.md#worker-processes).

## `Diagnostic` and `Severity`

`Fagus\Diagnostic` is a final readonly class with public properties:

| Property | Type | Meaning |
|---|---|---|
| `severity` | `Severity` | `Severity::Info`, `Severity::Warning` or `Severity::Error`. |
| `code` | `string` | A stable code, such as `css.unknown-property` or `resource.not-loaded`. |
| `message` | `string` | What happened, in English. |
| `source` | `string` | Where: `<style> 1` (the first `<style>` element), the URL of a linked stylesheet, the name given to `stylesheet()`, `html`, `resources` or `pdf`. |
| `line` | `int` | 1-based line in the source, 0 if unknown. |
| `column` | `int` | 1-based column, 0 if unknown. |

Cast it to a string for one line in the format `source:line:column: severity[code]: message`:

```php
foreach ($pdf->diagnostics() as $diagnostic) {
    if ($diagnostic->code === 'resource.not-loaded') {
        $logger->warning((string) $diagnostic);
    }
}
// resources: warning[resource.not-loaded]: image `logo.png` could not be loaded
```

`Fagus\Severity` is a string-backed enum: `Info = 'info'`, `Warning = 'warning'`,
`Error = 'error'`. The codes are listed in
[troubleshooting.md](troubleshooting.md#read-diagnostics).

## `Signature`

`Fagus\Signature` is a digital signature for `sign()`: the key that signs and what the signature
says. Like the document, it is immutable: every method returns a changed copy. A
`var_dump()` shows neither the key nor its password.

| Method | Description |
|---|---|
| `Signature::fromPkcs12(string $path, string $password)` | Signs with the key (RSA, or ECDSA on P-256) and the certificates of a PKCS#12 file (`.p12`, `.pfx`). Throws an `InvalidArgumentException` if the file cannot be read; a wrong password makes `render()` throw a `RenderException` |
| `Signature::fromPkcs12Data(string $data, string $password)` | The same with the bytes of a PKCS#12 file, such as one from a secrets manager. Throws an `InvalidArgumentException` for empty data |
| `Signature::withSigner(array $certificates, callable $signer)` | Signs with a key outside, such as one in an HSM or a key management service: `$certificates` are the signer's certificate and those of its chain (PEM or DER), `$signer` is `fn (string $digest, string $algorithm): string` |
| `name(string $name)` | The name of the signer, as the signature shows it |
| `reason(string $reason)` | Why the document is signed |
| `location(string $location)` | Where it is signed |
| `contactInfo(string $contactInfo)` | How to reach the signer |
| `time(DateTimeInterface $time)` | The time of signing; the document's creation date by default |
| `timestamp(callable $timestamper)` | Timestamps the signature (PAdES B-T): `fn (string $digest): string` returns the answer of a time-stamping authority |
| `Signature::timestampRequest(string $digest): string` | The RFC 3161 request to send to the authority for a digest |
| `certify(Certification $certification)` | Makes it a certification signature (DocMDP) that allows `Certification::NoChanges`, `FillForms` or `Annotate` ([pdf.md](pdf.md#certify-the-pdf)) |
| `visibleOn(string $id)` | Shows the signature in the box of the element with this `id`, with the element's content as its appearance ([pdf.md](pdf.md#visible-signatures)) |
| `visibleAt(int $page, float $x, float $y, float $width, float $height)` | Shows the signature in an area of a page (from 1), in points from its top left corner. Throws an `InvalidArgumentException` for a page below 1 or an empty area |
| `revocation(callable $revocation)` | Adds the validation data (PAdES B-LT): `fn (RevocationRequest $request): array` returns OCSP responses and CRLs (DER) for a certificate ([pdf.md](pdf.md#long-term-signatures-pades-b-lt-and-b-lta)) |
| `archiveTimestamp()` | Adds an archive timestamp after the validation data (PAdES B-LTA), from the `timestamp()` callback |

The signer callback gets the SHA-256 digest (32 bytes) to sign and the algorithm,
`Signature::RSA_SHA256` (`'RSA-SHA256'`) or `Signature::ECDSA_P256_SHA256`
(`'ECDSA-P256-SHA256'`), and returns the signature of the digest as it is: PKCS#1 v1.5 for RSA,
DER or the 64 bytes of r and s for ECDSA. Fagus checks it against the certificate.
`Fagus\RevocationRequest` has the readonly properties `certificate`, `issuer` (DER),
`ocspUrls`, `crlUrls` and `ocspRequest` (DER). An exception of a callback ends the render with that exception, and the worker is restarted for the next
document. [pdf.md](pdf.md#sign-the-pdf) has examples for a key management service and for a
public time-stamping authority with Laravel's `Http` and Guzzle.

```php
use Fagus\Signature;

$signature = Signature::fromPkcs12(storage_path('signing.p12'), config('services.signing.password'))
    ->reason('Rechnung')
    ->location('Berlin');

Fagus::html($html)->pdfA('3b')->sign($signature)->save('invoice.pdf');
```

## `Certification`

`Fagus\Certification` is a string-backed enum of what a certified PDF allows (see
[`certify()`](#signature)): `NoChanges = 'no-changes'`, `FillForms = 'fill-forms'`,
`Annotate = 'annotate'`.

## `Conformance`

`Fagus\Conformance` is a string-backed enum of PDF/A levels, for `pdfA()`:

| Case | Value | Level |
|---|---|---|
| `Conformance::PdfA1b` | `'1b'` | PDF/A-1b |
| `Conformance::PdfA2b` | `'2b'` | PDF/A-2b |
| `Conformance::PdfA2u` | `'2u'` | PDF/A-2u |
| `Conformance::PdfA2a` | `'2a'` | PDF/A-2a |
| `Conformance::PdfA3b` | `'3b'` | PDF/A-3b |
| `Conformance::PdfA3u` | `'3u'` | PDF/A-3u |
| `Conformance::PdfA3a` | `'3a'` | PDF/A-3a |
| `Conformance::PdfA4` | `'4'` | PDF/A-4 |
| `Conformance::PdfA4e` | `'4e'` | PDF/A-4e |
| `Conformance::PdfA4f` | `'4f'` | PDF/A-4f |

PDF/A-1b is PDF 1.4 without transparency. PDF/A-3 allows attachments of any type, as ZUGFeRD and
Factur-X need; PDF/A-4 is PDF 2.0, and PDF/A-4f allows attachments of any type too. The levels
ending in `a` are tagged for accessibility.

## `InvoiceProfile`

`Fagus\InvoiceProfile` is a string-backed enum of ZUGFeRD and Factur-X profiles, for
`facturX()`:

| Case | Value |
|---|---|
| `InvoiceProfile::Minimum` | `'minimum'` |
| `InvoiceProfile::BasicWl` | `'basicwl'` |
| `InvoiceProfile::Basic` | `'basic'` |
| `InvoiceProfile::En16931` | `'en16931'` |
| `InvoiceProfile::Extended` | `'extended'` |
| `InvoiceProfile::XRechnung` | `'xrechnung'` |

## `Permission`

`Fagus\Permission` is a string-backed enum of what the reader of an encrypted PDF may do without
the owner password, for `encrypt()`:

| Case | Value | Allows |
|---|---|---|
| `Permission::Print` | `'print'` | Printing, in full quality |
| `Permission::PrintLowQuality` | `'print-low-quality'` | Printing, only as a low-resolution image |
| `Permission::Modify` | `'modify'` | Changing the content |
| `Permission::Copy` | `'copy'` | Copying text and images |
| `Permission::Annotate` | `'annotate'` | Adding and changing comments, and filling in forms |
| `Permission::FillForms` | `'fill-forms'` | Filling in form fields |
| `Permission::Assemble` | `'assemble'` | Inserting, deleting and rotating pages, changing bookmarks |

## `Relationship`

`Fagus\Relationship` is a string-backed enum of how an attached file relates to the document
(PDF 2.0 `AFRelationship`), for `attach()`:

| Case | Value | Use for |
|---|---|---|
| `Relationship::Source` | `'source'` | The source the document was made from. |
| `Relationship::Data` | `'data'` | Data the document shows, such as a table as CSV. |
| `Relationship::Alternative` | `'alternative'` | Another representation of the same content. |
| `Relationship::Supplement` | `'supplement'` | Additional material. |
| `Relationship::Unspecified` | `'unspecified'` | None of these; the default. |

## Exceptions

All exceptions extend `Fagus\Exception\FagusException`, which extends PHP's `RuntimeException`.
Catch `FagusException` to handle anything that goes wrong in Fagus.

```php
use Fagus\Exception\EngineException;
use Fagus\Exception\RenderException;

try {
    $pdf = Fagus::html($html)->accessible()->render();
} catch (RenderException $exception) {
    // The document cannot be rendered as asked, or has diagnostics in strict mode.
    report($exception);
    foreach ($exception->diagnostics() as $diagnostic) {
        // those strict mode refused, or those found before rendering failed
    }
} catch (EngineException $exception) {
    // The binary was not found, could not be started, timed out or stopped.
}
```

| Exception | Thrown when |
|---|---|
| `FagusException` | Base class. Thrown itself by `Pdf::save()` when the file cannot be written. |
| `RenderException` | The document cannot be rendered (for example PDF/UA requirements that are not met), or has diagnostics in strict mode. `diagnostics(): list<Diagnostic>` returns the diagnostics strict mode refused, or those found before rendering failed (CSS that was ignored, resources that are missing), which often explain the failure. |
| `EngineException` | The binary was not found or could not be started, a font directory does not exist, the worker did not answer within the timeout, it stopped, it speaks another protocol, it cannot encrypt (a binary older than `encrypt()`, which would have written the PDF without a password), or a font passed to `addFont()` cannot be read. When the worker has exited, the message ends with its exit code and what it wrote to its error output. |
| `InvalidArgumentException` | A value Fagus does not know, such as a PDF/A level passed to `pdfA()` as a string that is not one, or `encrypt()` without a password. |
| `InstallException` | The binary could not be installed: a development version without binaries, an unsupported platform, a failed download, a checksum mismatch, or a directory that cannot be written. Thrown by `Installer`, `vendor/bin/fagus-install` and `php artisan fagus:install`; the Composer plugin turns it into a warning, or into an error with `FAGUS_INSTALL_STRICT` ([deployment.md](deployment.md#fail-the-build-without-the-binary)). |

The exact messages are listed in [troubleshooting.md](troubleshooting.md).

## `Installer`

`Fagus\Installer` installs the binary of the package's release for the platform PHP runs on. The
Composer plugin, `vendor/bin/fagus-install` and `php artisan fagus:install` use it; you need it
only to install from your own code.

```php
use Fagus\Installer;

$path = (new Installer(
    token: null,          // GitHub token for a private repository; FAGUS_GITHUB_TOKEN or GITHUB_TOKEN if null
    source: null,         // a directory or URL with the files of the release; FAGUS_DOWNLOAD_URL if null
    cacheDirectory: null, // where downloaded binaries are kept for the next installation
))->install();            // or ->install('/opt/fagus') for another directory

Installer::target();          // "x86_64-unknown-linux-musl" on Alpine on x64
Installer::target('FreeBSD', 'amd64'); // "x86_64-unknown-freebsd"; the BSDs by their name, not "BSD"
Installer::asset('x86_64-pc-windows-msvc'); // "fagus-x86_64-pc-windows-msvc.exe"
```

`install(?string $directory = null): string` returns the path of the binary. A binary that is
already there with the right SHA-256 is kept. See [deployment.md](deployment.md).
