# Coming from wkhtmltopdf

wkhtmltopdf prints HTML with an old WebKit (Qt WebKit, from about 2012) and takes its page setup
from command line flags: the paper size, the margins, HTML files for the header and the footer,
a generated table of contents. Fagus prints like a current Chrome and takes all of this from the
document's CSS instead. This page shows how the flags of wkhtmltopdf and the habits of its
templates map to Fagus. The same applies to knplabs/snappy and barryvdh/laravel-snappy, which run
wkhtmltopdf.

A template moves in three steps:

1. Move the flags into an `@page` rule and margin boxes in the template.
2. Replace the header and footer files and their `[page]` and `[topage]` placeholders with margin
   boxes or running elements.
3. Remove what only wkhtmltopdf needed: JavaScript for page numbers, `-webkit-box` and table
   layouts, zoom factors against smart shrinking.

## Render a file

```sh
# wkhtmltopdf
wkhtmltopdf --page-size A4 -T 20mm -B 20mm invoice.html invoice.pdf

# Fagus
fagus render invoice.html -o invoice.pdf
```

`fagus render` takes the page size and margins from the document, so the flags move into its
stylesheet (below). It reads files only from the input's directory, or from `--root`, and fonts
from `--font-dir` ([cli.md](cli.md)). It reads no URLs: render the page on the server and pass
the HTML.

In PHP and Laravel:

| knplabs/snappy, barryvdh/laravel-snappy | Fagus |
|---|---|
| `new Knp\Snappy\Pdf('/usr/local/bin/wkhtmltopdf')` | nothing: Composer installs the binary ([getting-started.md](getting-started.md)) |
| `$snappy->getOutputFromHtml($html)` | `Fagus::html($html)->render()->bytes()` |
| `$snappy->generateFromHtml($html, 'invoice.pdf')` | `Fagus::html($html)->save('invoice.pdf')` |
| `$snappy->getOutput('https://…')`, `generate($url, …)` | render the HTML yourself and pass it to `Fagus::html()` |
| `$snappy->setOption('page-size', 'A4')` and the other page options | `@page` in the template (below) |
| `SnappyPdf::loadView('invoice', $data)` | `Fagus::view('invoice', $data)` |
| `SnappyPdf::loadHTML($html)` | `Fagus::html($html)` |
| `->setPaper('a4')->setOrientation('landscape')` | `@page { size: A4 landscape }` |
| `->setOption('margin-top', 20)` | `@page { margin-top: 20mm }` |
| `->download('invoice.pdf')`, `->inline('invoice.pdf')` | `->download('invoice.pdf')`, `->inline('invoice.pdf')` |
| `->save($path)` | `->save($path)`, or `->store($path, $disk)` |
| `->output()` | `->render()->bytes()` |

[php.md](php.md) and [laravel.md](laravel.md) describe the methods.

## Page size, orientation and margins

| wkhtmltopdf | Fagus |
|---|---|
| `--page-size A4`, `-s Letter` | `@page { size: A4 }`, `@page { size: letter }` |
| `--page-width 80mm --page-height 200mm` | `@page { size: 80mm 200mm }` |
| `-O Landscape`, `--orientation Landscape` | `@page { size: A4 landscape }` |
| `-T 20mm -R 15mm -B 25mm -L 15mm` | `@page { margin: 20mm 15mm 25mm 15mm }` |

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    size: A4 landscape;
    margin: 20mm 15mm 25mm;
  }
  @page :first {
    margin-top: 45mm;
  }
</style>
</head>
<body>
  <h1>Price list</h1>
</body>
</html>
```

Without `size`, the page is A4, the default of wkhtmltopdf too. Without `margin`, the margins are
Chrome's 28pt (about 1cm); wkhtmltopdf's default is 10mm. Unlike the flags, `@page` rules can
give the first page, left and right pages and named pages other sizes and margins
([paged-media.md](paged-media.md#use-different-first-left-and-right-pages)).

## Headers and footers

wkhtmltopdf draws the header and the footer from `--header-left`, `--header-center`,
`--header-right` (and the same for the footer), or from separate HTML files with `--header-html`
and `--footer-html`, and fills in placeholders such as `[page]` and `[topage]`, in HTML files with
a script that reads them from the URL. Fagus draws headers and footers in the margin boxes of
`@page`: 16 boxes around the page, such as `@top-left`, `@top-center`, `@bottom-right`.

| wkhtmltopdf | Fagus |
|---|---|
| `--header-left "ACME"` | `@top-left { content: "ACME" }` |
| `--footer-center "[page] / [topage]"` | `@bottom-center { content: counter(page) " / " counter(pages) }` |
| `[page]`, `[topage]` | `counter(page)`, `counter(pages)` |
| `[section]`, `[subsection]` | `string-set` on the headings and `string()` in the margin box |
| `[title]`, `[doctitle]`, `[webpage]`, `[date]`, `[time]`, `[isodate]` | write the text into the template, which the server renders anyway |
| `--replace name value` | the same: a value of the template |
| `--header-font-name`, `--header-font-size` | `font-family` and `font-size` in the margin box, or on `@page` |
| `--header-line`, `--footer-line` | `border-bottom` or `border-top` on the margin box |
| `--header-spacing`, `--footer-spacing` | `padding` of the margin box, or a larger page margin |
| `--header-html header.html`, `--footer-html footer.html` | a running element: `position: running(name)` and `content: element(name)` |

A header and footer of text:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    margin: 25mm 20mm;
    font: 9pt sans-serif;
    color: #555;
    @top-left { content: "ACME Inc."; }
    @top-right { content: string(chapter); }
    @bottom-center {
      content: "Page " counter(page) " of " counter(pages);
      border-top: 0.5pt solid #999;
    }
  }
  @page :first {
    @top-right { content: none; }
  }
  h2 { string-set: chapter content(); break-before: page; }
</style>
</head>
<body>
  <h1>Annual report 2026</h1>
  <h2>Revenue</h2>
  <h2>Outlook</h2>
</body>
</html>
```

An HTML header or footer, the replacement for `--header-html` and `--footer-html`, is an element
of the document with `position: running()`: it leaves the flow and shows in the margin box on
every page. It is part of the document, so it takes the document's fonts and styles and needs no
`<!doctype>`, no styles of its own and no script:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    margin: 30mm 20mm 25mm;
    @top-center { content: element(letterhead); }
    @bottom-center { content: element(footer); }
  }
  .letterhead {
    position: running(letterhead);
    display: flex;
    justify-content: space-between;
    width: 100%;
    border-bottom: 1pt solid #1e3a8a;
  }
  .footer {
    position: running(footer);
    width: 100%;
    font-size: 8pt;
    text-align: right;
  }
  .footer::after { content: "Page " counter(page) " of " counter(pages); }
</style>
</head>
<body>
  <header class="letterhead">
    <strong>ACME Inc.</strong>
    <span>1 Main Street, Springfield</span>
  </header>
  <footer class="footer"></footer>

  <h1>Letter</h1>
</body>
</html>
```

Put running elements at the start of the body: they show from the page they appear on. As with
`--header-html`, the margin must be high enough for the header. Fonts, named strings and the
other details are in [paged-media.md](paged-media.md#add-a-header-and-a-footer).

## Page numbers in the content

`counter(page)` works in the content too, and `target-counter()` gives the page an element is on:

```css
.see-also::after { content: " (page " target-counter(attr(href), page) ")"; }
```

`counter(pages)` belongs in margin boxes and running elements; in the content, the number of pages
is not known yet ([paged-media.md](paged-media.md#number-the-pages)).

## Table of contents

wkhtmltopdf makes a table of contents from the headings with `toc`, styled with
`--xsl-style-sheet`, `--toc-header-text` and `--disable-dotted-lines`. In Fagus the table of
contents is part of the template: links to the headings, with `target-counter()` for their pages
and `leader()` for the dots.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  .toc a { display: block; color: inherit; text-decoration: none; }
  .toc a::after { content: leader(dotted) target-counter(attr(href), page); }
  h2 { break-before: page; }
</style>
</head>
<body>
  <nav class="toc">
    <h1>Contents</h1>
    <a href="#installation">Installation</a>
    <a href="#configuration">Configuration</a>
  </nav>
  <h2 id="installation">Installation</h2>
  <h2 id="configuration">Configuration</h2>
</body>
</html>
```

The template loops over the same sections it prints, so the table of contents lists what you
choose. `leader(space)` or `leader(" ")` replaces `--disable-dotted-lines`. The entries are links
in the PDF ([paged-media.md](paged-media.md#build-a-table-of-contents)).

## Cover page

`cover cover.html` becomes the first element of the document on a page of its own, with a named
page for its own margins:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { margin: 20mm; }
  @page cover { margin: 0; background: #1d3557; }
  body { margin: 0; }
  .cover { page: cover; height: 297mm; box-sizing: border-box; padding: 60mm 20mm; color: white; }
</style>
</head>
<body>
  <div class="cover"><h1>Product catalog 2027</h1></div>
  <h2>Introduction</h2>
</body>
</html>
```

See [paged-media.md](paged-media.md#fill-the-whole-page).

## Page breaks

Templates for wkhtmltopdf use `page-break-before`, `page-break-after` and `page-break-inside`.
They work in Fagus as aliases of `break-before`, `break-after` and `break-inside`:

```css
.chapter { break-before: page; }        /* page-break-before: always */
h1, h2, h3 { break-after: avoid; }      /* page-break-after: avoid */
tr, figure { break-inside: avoid; }     /* page-break-inside: avoid */
```

Old WebKit broke lines of text in half at the end of a page and ignored `page-break-inside` on
table rows; workarounds against that (wrapping each row in a table of its own, fixed heights per
page) can go. Fagus breaks pages where Chrome breaks them, repeats `<thead>` and `<tfoot>` on
every page and keeps rows with `break-inside: avoid` together
([paged-media.md](paged-media.md#control-page-breaks)).

## Sizes, zoom and smart shrinking

wkhtmltopdf lays out the page at a width of its own and shrinks it to the paper ("smart
shrinking"), so its CSS pixels come out smaller than 1/96 inch, and the factor depends on the page
and the platform. Templates made for it often use large pixel sizes or `--zoom` to make up for
that. Fagus lays out at CSS sizes, as Chrome prints: 1px is 1/96 inch, 12pt is 12pt.

| wkhtmltopdf | Fagus |
|---|---|
| `--disable-smart-shrinking` | the default: there is no smart shrinking. Content wider than the page area is scaled down by at most 1.5 times, as Chrome's print does. |
| `--zoom 0.8` | `html { zoom: 0.8 }` |
| `--dpi 300` | not needed: text and shapes stay vectors; CSS pixels are 1/96 inch |
| `--image-dpi`, `--image-quality`, `--lowquality` | not available: images are embedded at full resolution, JPEG as it is ([pdf.md](pdf.md#images-in-the-pdf)) |
| `--minimum-font-size` | not available: set the sizes in CSS |

After the move, check font sizes and widths: a template that wkhtmltopdf shrank can come out larger
in Fagus. Sizes in `pt` and `mm` are what they say.

## Print styles

wkhtmltopdf applies `@media screen` rules unless you pass `--print-media-type`. Fagus always
applies `@media print` and never `@media screen`, as Chrome does when it prints. Move rules that
the PDF needs out of `@media screen` ([css.md](css.md#media-queries-and-feature-queries)).

wkhtmltopdf only knows `display: -webkit-box` for flexible layouts and no grid. Fagus lays out
flex, grid, `calc()` and custom properties as Chrome does; the old `-webkit-box` still works.

## JavaScript

Fagus runs no JavaScript, so `--javascript-delay`, `--run-script`, `--window-status`,
`--no-stop-slow-scripts` and `--enable-javascript` have no counterpart. What scripts did for
wkhtmltopdf moves to the server:

- **Page numbers in header and footer files**, read from the URL by a script: `counter(page)` and
  `counter(pages)` (above).
- **Charts** drawn by Chart.js or similar: draw them on the server as SVG and put the SVG into the
  template or hand it over with `resource()`. SVG stays vector graphics in the PDF.
- **Values computed in the browser** (totals, dates, formatting): compute them in PHP or Blade.
- **Content loaded by scripts**: load it on the server before you render.

## Files, the network and fonts

| wkhtmltopdf | Fagus |
|---|---|
| `--enable-local-file-access`, `--allow <dir>` | `baseDirectory()` and `root()`; `--root` on the command line. Without a base directory, Fagus reads no files ([resources.md](resources.md#read-files-from-a-base-directory)) |
| `--disable-local-file-access` | the default without a base directory |
| `--cookie`, `--custom-header`, `--username`, `--password`, `--proxy` | not available: Fagus loads nothing from the network. A `resolver()` of yours may, with the cookies and headers you choose ([resources.md](resources.md#answer-requests-with-resolver)) |
| `--load-error-handling ignore` | the default: a missing resource is left out and reported as a diagnostic; `strict()` or `--strict` fails instead |
| `--user-style-sheet print.css` | `stylesheet($css)` in PHP, or a `<link>` in the template |
| `--encoding windows-1252` | not available: HTML and stylesheets are UTF-8. Convert other encodings first, such as with `mb_convert_encoding()` |

Fonts that wkhtmltopdf found installed on the server are not used by default: load them with
`@font-face`, put them in a font directory (`fontDirectories`, `--font-dir`) or turn on system
fonts ([fonts.md](fonts.md)). Web fonts from Google Fonts or a similar service load through a
resolver ([resources.md](resources.md#load-web-fonts-through-the-resolver)).

wkhtmltopdf set text without a `font-family` in the system's serif font. Fagus has none: such text
comes out in the built-in Inter, or in the first font of your font directories. Set `font-family`
on `body`, or set the `serif` family ([fonts.md](fonts.md#set-the-generic-families)).

## The PDF

| wkhtmltopdf | Fagus |
|---|---|
| `--title "Invoice 42"` | `<title>Invoice 42</title>` |
| `--outline`, `--no-outline` | the outline (bookmarks) always comes from the headings; `bookmark-level: none` leaves headings out |
| `--outline-depth 2` | `h3, h4, h5, h6 { bookmark-level: none; }` |
| `--no-background` | backgrounds always print; remove them in the stylesheet |
| `--grayscale` | `html { filter: grayscale(1); }` |
| `--disable-internal-links`, `--disable-external-links` | not available: every `<a href>` to an element or a web address is a link in the PDF |
| `--enable-forms` | not available: form controls are drawn as Chrome prints them |
| `--collate`, `--copies` | not available: they are printer settings |

The metadata, links, the outline, tagged PDF, PDF/A and invoices are described in [pdf.md](pdf.md).

## Several pages in one PDF

`wkhtmltopdf cover.html a.html b.html out.pdf` puts several documents into one PDF. Render one
document instead: put the pages one after the other into the template, each starting with
`break-before: page`, and give them named pages where they need their own page setup
([paged-media.md](paged-media.md#give-sections-pages-of-their-own)). `[sitepage]` and
`[sitepages]`, the page number within one of the inputs, have no direct counterpart;
`counter-set: page 1` in an `@page` rule starts the numbering again.
