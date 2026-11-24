# The PDF

This page describes the PDF file Fagus writes and the options that change it: metadata, links and
bookmarks, fill-in forms, tagged and accessible PDF, PDF/A, attachments, electronic invoices,
passwords, digital signatures and the compression of images. How the pages look is up to the HTML and CSS (see
[css.md](css.md) and [paged-media.md](paged-media.md)); the methods of the document builder are
described in [php.md](php.md).

Every PDF Fagus writes is a PDF 1.7 file (PDF 1.4 for PDF/A-1b, PDF 2.0 for PDF/A-4) with:

- the fonts the text uses, embedded as subsets;
- the title, language, author, subject and keywords of the document;
- links and the document outline (bookmarks) from the headings;
- the logical structure for screen readers (tagged PDF), unless you turn it off;
- with `forms()`, the fields of an interactive form; with `-fagus-layer`, layers
  ([extensions.md](extensions.md#layers-that-show-on-screen-or-in-print));
- compressed streams and object streams (not in PDF/A-1b, which PDF 1.4 lacks them for).

```php
use Fagus\Conformance;
use Fagus\Fagus;
use Fagus\Signature;

$pdf = Fagus::html($html)
    ->pdfA(Conformance::PdfA3b)                  // PDF/A
    ->accessible()                               // PDF/UA-1
    ->attach('timesheet.csv', $csv, 'text/csv', description: 'Timesheet')  // attachments
    ->facturX($invoiceXml)                       // ZUGFeRD / Factur-X
    ->sign(Signature::fromPkcs12('signing.p12', $password))  // digital signature
    ->created(new DateTimeImmutable('2026-11-01 09:00'))  // reproducible output
    ->render();
```

## Set the title, language and other metadata

Fagus takes the metadata from the document, as a browser does. There is no method to set it
separately: write it into the HTML.

| PDF field | Comes from |
|---|---|
| Title | `<title>`, its white space collapsed |
| Language (`Lang`) | `<html lang>`, else the engine's `locale` (see [php.md](php.md)) |
| Author | `<meta name="author" content="…">` |
| Subject | `<meta name="description" content="…">` |
| Keywords | `<meta name="keywords" content="…">`, split at commas |
| Producer | `Fagus` |
| Creation and modification date | `created()`, else the current time |

```html
<html lang="de">
<head>
  <title>Rechnung RE-2026-0042</title>
  <meta name="author" content="Muster GmbH">
  <meta name="description" content="Rechnung RE-2026-0042 an Kunde AG">
  <meta name="keywords" content="Rechnung, Webentwicklung">
</head>
```

The same values go into the document information dictionary and the XMP metadata. With a title,
the PDF asks viewers to show the title instead of the file name (`DisplayDocTitle`), as Chrome's
PDFs do.

## Links

An `<a href>` becomes a link in the PDF when it leads to:

- an element of the document: `href="#total"` leads to the element with `id="total"` or to an
  `<a name="total">`; `href="#"` and `href="#top"` lead to the top of the first page;
- a URL with the scheme `http:`, `https:`, `mailto:`, `tel:` or `ftp:`.

Other links are dropped: relative URLs (`page.html`), `javascript:`, `file:` and any other scheme.
A web address without a path gets one (`https://example.com` becomes `https://example.com/`), as
in the URLs Chrome writes.

Each inline box of a link gets its own link area, and so do images and inline blocks inside a
link. Links in margin boxes (headers and footers) work on every page. Links that are hidden or
fully transparent make no link area.

Page numbers of link targets, such as a table of contents with `target-counter()` and `leader()`,
are described in [paged-media.md](paged-media.md).

## Control the document outline

The document outline, the bookmarks a PDF viewer shows beside the pages, comes from the headings.
The default style sheet gives `<h1>` to `<h6>` the levels 1 to 6; an element with
`role="heading"` is an entry at its `aria-level` (2 without one), as in Chrome, unless its
`bookmark-level` is set: `bookmark-level: none` leaves it out too. An entry's title
is the heading's text as it is shown: hidden text is left out and `text-transform` applies.
Entries point at the top left of their element.

Three properties from CSS Generated Content for Paged Media change the outline:

| Property | Values | Effect |
|---|---|---|
| `bookmark-level` | `none` or an integer from 1 | Makes an element an entry at that level, or none |
| `bookmark-label` | `content(text)` (initial), strings, `attr()` | The title of the entry |
| `bookmark-state` | `open` (initial), `closed` | Whether the entry shows its children |

```css
/* Chapters are entries, their headings are not. */
section.chapter { bookmark-level: 1; bookmark-label: "Chapter " attr(data-number); }
section.chapter h1 { bookmark-level: none; }

/* Leave the invoice's headings out of the outline. */
.invoice h2, .invoice h3 { bookmark-level: none; }

/* Show appendices collapsed. */
.appendix h1 { bookmark-state: closed; }
```

Deeper levels nest under the entry before them. Elements in margin boxes make no entries, and an
entry with an empty title is left out. A document without entries has no outline, except in
PDF/UA, which asks for one.

## Choose how the PDF opens

A PDF can say how a viewer shows it when it opens: what stands beside the pages, how the pages
are laid out, how large they are, and on which page it starts. HTML and CSS have no way to say
this, so it is an option of the document:

```php
use Fagus\PageLayout;
use Fagus\PageMode;
use Fagus\Zoom;

// A manual: the outline beside the pages, scrolling in one column, as wide as the window.
Fagus::html($html)->initialView(PageMode::Outlines, PageLayout::OneColumn, Zoom::FitWidth);

// An invoice with its XML: the list of attachments open.
Fagus::html($html)->facturX($xml)->initialView(PageMode::Attachments);

// Slides: full screen, one whole page at a time.
Fagus::html($html)->initialView(PageMode::FullScreen, PageLayout::SinglePage, Zoom::FitPage);
```

| Setting | Values | In the PDF |
|---|---|---|
| Page mode | `none`, `outlines`, `thumbnails`, `full-screen`, `attachments`, `layers` | `/PageMode`: the panel beside the pages, or full-screen mode |
| Page layout | `single-page`, `one-column`, `two-column-left`, `two-column-right`, `two-page-left`, `two-page-right` | `/PageLayout`: one page at a time or scrolling, in one column or two; `left` and `right` say where the odd pages go, so `right` leaves the first page alone like a cover |
| Zoom | `fit-page`, `fit-width` or a number of percent (100 is the actual size) | `/OpenAction`: a destination at the top of the page |
| Page | a page number from 1 | `/OpenAction`: the page the PDF opens at; the first one without it |
| Fit window | on or off | `/ViewerPreferences`: the viewer sizes its window to the first page shown |
| Center window | on or off | `/ViewerPreferences`: the viewer centers its window on the screen |

What is left out is not written, and the viewer decides. Without the option the PDF is the same,
byte for byte, as before. Fagus does not open the outline on its own for a document with
headings: say `outlines` where you want it.

**Viewers follow this to different degrees.** These entries are wishes. Adobe Acrobat follows
all of them, unless its user has set a default zoom or layout in the preferences or chose to
reopen documents as they were left. The viewers built into Chrome, Edge and Firefox follow the
page mode and the page to open at in part and ignore most of the rest; Preview on macOS ignores
nearly all of it. Check with the viewers your readers use, and do not rely on the view for
anything the reader must see.

With standards and other options:

- PDF/A-2, PDF/A-3, PDF/A-4 and PDF/UA-1 allow all of it. Under PDF/UA the viewer still shows
  the title instead of the file name.
- PDF/A-1b is PDF 1.4, which lacks the page modes `attachments` and `layers` and the page
  layouts `two-page-left` and `two-page-right`. Fagus leaves such a value out and reports a
  `pdf.view-ignored` warning; the rest of the view is written.
- A page the document does not have is left out with the same warning, and the PDF opens at its
  first page.
- Encrypted and signed PDFs take the view like any other.

The option is `initialView()` in [PHP](php.md#initialviewpagemode-pagemode--null-pagelayout-pagelayout--null-zoomfloatnull-zoom--null-int-page--null-bool-fitwindow--false-bool-centerwindow--false-document)
and [Laravel](laravel.md#document-methods), `--page-mode`, `--page-layout`, `--zoom` and
`--open-page` on the [command line](cli.md#render-an-html-file), `Document::initial_view` in
[Rust](rust.md#documents) and `view` in the [worker protocol](protocol.md).

## Fill-in forms

Chrome prints form controls as they look, and so does Fagus by default. With `forms()`, the
controls become the fields of an interactive form, which a reader fills in in a PDF viewer (an
AcroForm), and the PDF looks the same as without it:

```php
Fagus::html($html)->forms()->save('application.pdf');
```

On the command line, the flag is `--forms`; in Rust, `Document::forms(true)`.

| Control | Field |
|---|---|
| `<input>` of type `text`, `search`, `email`, `url`, `tel`, `number`, `password`, `date`, `time`, `datetime-local`, `month` or `week` | A text field with the `value` |
| `<textarea>` | A text field of several lines with its text |
| `<input type=checkbox>` | A checkbox, on where `checked` |
| `<input type=radio>` | A radio button; those with the same `name` are one field, of which one is on |
| `<select>` (without `multiple` and `size`) | A drop-down list with its options, the `selected` one chosen |
| `<select multiple>`, `<select size=3>` | A list box with its options; with `multiple`, several may be chosen, the `selected` ones are |
| `<button>`, `<button type=submit>`, `<input type=submit>` in a `<form action>` | A push button that sends the fields of its form to the `action` |
| `<button type=reset>`, `<input type=reset>` in a `<form>` | A push button that resets the fields of its form |

Other buttons (`<button type=button>`, `<input type=image>`), file inputs, sliders, color wells
and `<select multiple size=1>` stay as they look. So does every control inside an element with
`inert`, for a sample that is filled in for illustration:

```html
<p>Example: <span inert><input value="DE89 3704 0044 0532 0130 00"></span></p>
<p>Your IBAN: <input name="iban" maxlength="34" required></p>
```

What the fields get from the controls:

- **The name** is the `name`, else the `id`, else `field1`, `field2`, … in document order.
  Periods make a tree of fields, as PDF reads names: `customer.name` and `customer.email` are
  `name` and `email` in `customer`. A name that is taken gets `_2`, `_3`, …
- **The value** is the `value` (a number only if it is valid, an email address trimmed), the text
  of a `<textarea>`, the `value` (or text) of the chosen option (of every chosen option of a list
  box with `multiple`; a list box without a `selected` option has none chosen, as in HTML); a
  checkbox or radio button is on in the state named by its `value` (`on` without one). A password field stores no value; it
  shows its dots. A date stays as its `value` is written (`2026-11-19`), while the field shows it
  as the control does.
- **Flags:** `readonly` and `disabled` (also of a `<fieldset>`) make the field read-only,
  `required` required, `maxlength` its maximum length.
- **The description** for screen readers is the `aria-label`, the `title`, the text of the
  `<label>`, the `placeholder` or the name; of a button, its `aria-label`, `title`, label or
  name.
- **The text** a reader types has the font size, color and `text-align` of the control, in
  Helvetica, the font every viewer has.

Every field shows what the control shows, its value or placeholder in the control's fonts, drawn
in the field's appearance; the frame of the control (its border and background, the arrow of a
`<select>`) stays on the page below it. A viewer shows the PDF as it is printed, and redraws a
field only when a reader changes it. A control that is repeated, such as one in a header that
shows on every page, is one field with a place on each page: filling one fills all.

### Send and reset a form

A submit button sends the fields of its form (the controls inside the `<form>`, or those whose
`form` attribute names it) when a reader clicks it in a viewer that sends forms, such as Acrobat
and Foxit Reader; browsers' viewers (pdf.js, Chrome's) show the button but do not send. The
address and method are those of the button's `formaction` and `formmethod`, else of the form's
`action` and `method`:

```html
<form action="https://example.com/apply" method="post">
  <label>Name <input name="name" required></label>
  <button>Send</button> <button type="reset">Start over</button>
</form>
```

- The `action` must be an absolute `http:`, `https:` or `mailto:` URL. A PDF has no address of
  its own, so a relative `action` (`/apply`) leads nowhere and its button stays as it looks.
- `method="get"` (the default, as in HTML) sends the fields in the URL, `method="post"` in the
  body, in both cases encoded as `application/x-www-form-urlencoded` (the HTML form format of
  PDF). PDF has no `multipart/form-data`, so `enctype` makes no difference; a form with files
  cannot be sent from a PDF anyway. `method="dialog"` sends nothing: its button stays as it looks.
- A reset button sets the fields of its form back to their values in the PDF.
- A `disabled` button is a read-only field, which does nothing.

PDF/A forbids fields that act, so with `pdfA()` and `facturX()` the buttons stay as they look,
and the other controls still become fields.

### Turned fields and pages

A control inside a transformed box (`transform`, `rotate`, `scale`) becomes a field too: its
widget is the smallest rectangle around the box as it is drawn, and its appearance is turned,
skewed and scaled as the box is, so the PDF looks as without `forms()`. The same goes for
fields on pages turned with `page-orientation`. A viewer draws a field anew once a reader changes
it. For turns by a quarter, a half or three quarters (`rotate(90deg)`, `rotate(-90deg)`,
`rotate(180deg)`, and turned pages), the field tells viewers (`/MK /R`) to draw the new value
turned too. PDF knows no other angles and no skew: a field turned by 30° or skewed shows its
value as it was printed until a reader changes it, and from then on upright in the rectangle
around the box. A control in a 3D space (`transform-style: preserve-3d`) stays as it looks.

### Fields that are cut off

Chrome cuts a control off where it reaches out of the page area or out of a box around it with
`overflow` other than `visible`, but a PDF viewer draws a field over the page wherever its
rectangle lies, and annotations know no clipping. So a field lies only where its control shows:

- A control that is cut off entirely is no field; it stays as it is printed (that is, it does not
  show).
- A control that is partly cut off gets a field over the part that shows, and its appearance
  shows that part where it was printed (its `BBox` is cut to it). A reader who clicks the field
  types into that part; pdf.js and Chrome's viewer put their text box there too.
- A control in a box turned by other than a quarter turn, or skewed, keeps its whole rectangle:
  the part that shows is no rectangle in it.

Only clipping cuts a field. A control that is transparent (`opacity: 0`) or covered by other
content is still a field: such controls are
often laid over a printed form on purpose, to be filled in where the form shows its boxes. To
leave a control out of the fields, hide it with `visibility: hidden` or `display: none`.

Fields go with PDF/A-1b to PDF/A-4 and with PDF/UA-1: in a tagged PDF each field is a form field
of the logical structure (`Form`) with its widget, and the description is its alternative text.

## Tagged PDF

A tagged PDF contains the logical structure of the document: what is a heading, a paragraph, a
list or a table, in reading order. Screen readers read it, and PDF viewers use it to reflow text
and to copy tables. Fagus tags every PDF by default.

What Fagus tags:

- headings (`<h1>` to `<h6>`, `role="heading"`) and paragraphs;
- lists with their labels (the markers) and bodies;
- footnotes as notes with their markers as labels, after the text that calls them, the call a
  link to the footnote and the marker one back (see [Paged media](paged-media.md#footnotes));
- data tables, with header cells and their scope, row and column spans, captions, and header and
  footer groups; a header repeated on every page counts once;
- images (`<img>`, inline `<svg>`) as figures with their alternative text, MathML formulas with
  their `alttext` (or TeX annotation, or text);
- links, with their annotations and a description (their `aria-label`, `title`, text, or the
  `alt` of their images);
- with `forms()`, form fields with their widgets (see [Fill-in forms](#fill-in-forms));
- `<blockquote>`, `<q>`, `<article>`, `<section>`, `<figcaption>`, `<code>`, `<kbd>`, `<samp>`,
  `<em>`, `<strong>`, and `<abbr title>` with its expansion;
- changes of language (`lang` on an element);
- other blocks: a paragraph when they hold only text, a division when they hold blocks.

What Fagus marks as artifacts, content a reader skips:

- backgrounds, borders and other decoration;
- margin boxes (headers, footers, page numbers) and running elements, whatever they hold: keep
  content that a reader needs in the page area (see
  [Paged media](paged-media.md#fill-the-whole-page));
- the copies of repeated content: fixed boxes on every page after the first, table headers and
  footers repeated on later pages;
- layout tables: tables without `<th>`, `<caption>`, `<thead>`, `<tfoot>`, `summary` or a
  `role` of `table` or `grid` are divisions, their cells are not table cells;
- images with `alt=""` or `role="presentation"` (or `none`), and elements with
  `aria-hidden="true"`.

### Turn tagging off

A PDF without tags is somewhat smaller and faster to write, which shows with documents that have
many table cells. Turn tagging off with `tagged(false)`:

```php
Fagus::html($html)->tagged(false)->save('report.pdf');
```

In Laravel, the configuration key `tagged` sets the default for all documents. On the command
line, the flag is `--untagged`. `tagged(false)` has no effect when the PDF must be tagged: with
`accessible()` and with PDF/A-2a and PDF/A-3a.

## Make an accessible PDF (PDF/UA-1)

`accessible()` writes a PDF that conforms to PDF/UA-1 (ISO 14289-1), the standard for accessible
PDF. The PDF is tagged, has an outline, and Fagus checks what the standard requires of the
document itself:

- a title: a `<title>` with text;
- a language: `<html lang>`, or the engine's `locale`;
- alternative text for every image: `alt` (or `aria-label`) on `<img>`, a `<title>` or
  `aria-label` in an inline `<svg>`; images that are decoration take `alt=""`, `role="presentation"`
  or `aria-hidden="true"`;
- headings that start at `<h1>` and skip no level on the way down (an `<h3>` may follow an
  `<h1>` only after an `<h2>`).

```php
use Fagus\Exception\RenderException;
use Fagus\Fagus;

try {
    $pdf = Fagus::html($html)->accessible()->render();
} catch (RenderException $exception) {
    // the document does not meet PDF/UA-1: the document has no language (add `lang` to
    // <html>); the image `logo.png` in line 12 has no alternative text (add `alt`, empty if it
    // is decoration)
    echo $exception->getMessage();
}
```

A document that does not meet the standard is refused: rendering fails with a `RenderException`
whose message lists every problem, separated by semicolons. The messages are:

| Problem | Message |
|---|---|
| No title | `the document has no title (add a <title>)` |
| No language | ``the document has no language (add `lang` to <html>)`` |
| Image without alternative text | ``the image `logo.png` in line 12 has no alternative text (add `alt`, empty if it is decoration)`` |
| SVG without alternative text | ``the SVG in line 30 has no alternative text (add a <title> or `aria-label`, or `aria-hidden="true"` if it is decoration)`` |
| Formula without alternative text | ``the formula in line 41 has no alternative text (add `alttext`)`` |
| First heading not an `<h1>` | `the first heading, an <h2> in line 5, is not an <h1> (headings start at h1)` |
| Skipped heading level | `the <h3> in line 9 follows an <h1> (headings skip no level: use an <h2>)` |

The PDF itself is checked as it is written, for PDF/UA as for PDF/A. These problems end in the
same exception:

- ``Inter has no glyph for `☃`, and no other font has one``: a character that no font has, which
  other PDFs show as an empty box, with the warning `font.missing-glyph`. Add a font that has it
  (see [fonts.md](fonts.md)).
- `text in … cannot be mapped to Unicode` and `… shows U+E000 from the private use area, which
  has no meaning`: glyphs without a meaning as text, as in icon fonts.
- `the license of … does not allow embedding it`: the font's license forbids embedding it.
- Attached files that do not qualify (see [Attach files](#attach-files)), for example ``the
  attached file `notes` has no valid MIME type (like `text/csv`), which PDF/A-3 requires``.
- ``the document shows a PDF file as an image (a page template), whose content cannot be checked
  against the standard; use an SVG version of it (`pdftocairo -svg`)``: see
  [Use a PDF as page template](paged-media.md#use-a-pdf-as-page-template).

`accessible()` works together with PDF/A and with electronic invoices:

```php
Fagus::html($html)->pdfA(Conformance::PdfA3a)->accessible()->facturX($xml)->save('invoice.pdf');
```

## Write PDF/A

PDF/A (ISO 19005) is the standard for archiving. Fagus writes PDF/A-1b, PDF/A-2 and PDF/A-3 in
their levels b, u and a, and PDF/A-4 with its levels e and f:

| Level | `Conformance` | Name | Requires |
|---|---|---|---|
| PDF/A-1b | `PdfA1b` | `'1b'` | PDF 1.4, no transparency, fonts embedded, colors defined |
| PDF/A-2b | `PdfA2b` | `'2b'` | a reliable appearance: fonts embedded, colors defined |
| PDF/A-2u | `PdfA2u` | `'2u'` | 2b, and all text mapped to Unicode |
| PDF/A-2a | `PdfA2a` | `'2a'` | 2u, tagged, with a language and alternative text for images |
| PDF/A-3b | `PdfA3b` | `'3b'` | like 2b, and allows attachments of any type |
| PDF/A-3u | `PdfA3u` | `'3u'` | 3b, and all text mapped to Unicode |
| PDF/A-3a | `PdfA3a` | `'3a'` | 3u, tagged, with a language and alternative text for images |
| PDF/A-4 | `PdfA4` | `'4'` | PDF 2.0, all text mapped to Unicode |
| PDF/A-4e | `PdfA4e` | `'4e'` | 4, for engineering documents (allows 3D content, which Fagus does not write) |
| PDF/A-4f | `PdfA4f` | `'4f'` | 4, and allows attachments of any type |

```php
use Fagus\Conformance;
use Fagus\Fagus;

Fagus::html($html)->pdfA(Conformance::PdfA2b)->save('archive.pdf');
Fagus::html($html)->pdfA('3a')->save('archive.pdf');  // the name, in any case
Fagus::html($html)->pdfA()->save('archive.pdf');      // PDF/A-3b
```

A name that is not a level throws a `Fagus\Exception\InvalidArgumentException` that lists the
levels. On the command line, the flag is `--pdfa 3b`.

Which level to choose: PDF/A-2b or PDF/A-3b where an archive only asks for PDF/A, PDF/A-3 for
attachments and invoices, PDF/A-1b only for an archive that accepts nothing newer, and PDF/A-4
for one that asks for PDF 2.0.

What changes in a PDF/A file:

- The XMP metadata names the part and level (`pdfaid`).
- An sRGB ICC profile is the output intent, and colors are written in that color space. Without
  PDF/A, colors are plain RGB.
- Every character must have a glyph in a font. A character no font has (shown as an empty box
  in other PDFs) makes the document fail: ``Inter has no glyph for `☃`, and no other font has
  one``. The other checks of the written file are listed under PDF/UA above.
- Transparency, blend modes and masks are kept: PDF/A-2, PDF/A-3 and PDF/A-4 allow them.
  PDF/A-1 does not; see below.
- What would need a PostScript function, which PDF/A forbids, is drawn another way: conic
  gradients become images, and repeating linear gradients are written out period by period (one
  with more than 2000 periods in its box becomes its average color). Blurred shadows are images
  that viewers must not smooth.
- PDF/A-2 allows only PDF/A files as attachments, which Fagus does not check, so it refuses
  every attachment: ``PDF/A-2 allows only PDF/A files as attachments; attach `timesheet.csv` to a
  PDF/A-3 document instead``. Use PDF/A-3, which requires a description for each attached file
  (see below). PDF/A-4 refuses them the same way (attach them to PDF/A-4f), and PDF/A-1 allows
  none.
- The levels a are tagged whatever `tagged()` says, and fail without a language or with an
  image without alternative text.

A document that does not meet the level is refused with a `RenderException`, as with PDF/UA:

```text
the document does not meet PDF/A-3a: an image has no alternative text (add `alt`, empty for
decoration); the document has no language (add `lang` to <html>)
```

### PDF/A-1b

PDF/A-1 (ISO 19005-1) is based on PDF 1.4. The file has a classic cross-reference table and no
object streams, so it is somewhat larger. PDF/A-1 forbids transparency: no `opacity` below 1, no
translucent colors, no images with transparent pixels, no blend modes, masks or soft shadows.
Fagus draws what it can without transparency and refuses the rest:

- Content that does not show (`opacity: 0`, a clip that encloses nothing) is clipped away instead
  of drawn transparent.
- `isolation` changes nothing where nothing is transparent, and is left out.
- A PNG, GIF or WebP image with an alpha channel whose pixels are all opaque is embedded without
  that channel. GIF images always have one in the PDF otherwise.
- Everything else that is transparent is refused. Fagus does not flatten it onto what lies below
  (which would mean drawing that, text included, as pixels) and does not leave it out.

The message names what is transparent and the first page where it is:

```text
the document does not meet PDF/A-1b: PDF/A-1 forbids transparency, which the document uses for
`opacity` (first on page 1), translucent colors (first on page 2) and images with transparent
pixels (first on page 2); PDF/A-2b and later allow it
```

The causes are `opacity`, blend modes (`mix-blend-mode`, `background-blend-mode`), masks
(`mask`), translucent colors (also in borders and backgrounds), text in translucent colors or
color fonts (emoji), gradients with translucent colors, images with transparent pixels, SVG
images with transparency, shadows (`box-shadow`, `text-shadow`; a blurred shadow always has
transparent pixels) and filters (`filter`) drawn as pixels. Replace them with opaque colors and
images, or write PDF/A-2b, which keeps them.

PDF/A-1b goes with neither attachments (PDF/A-1 allows none) nor `accessible()`: PDF/UA-1 builds
on PDF 1.7, and the PDF is refused with ``PDF/A-1b and PDF/UA-1 need different PDF versions and
cannot be written together (PDF/UA-1 goes with PDF/A-2 and PDF/A-3)``. A Factur-X invoice is
raised to PDF/A-3b (see below). The PDF is tagged unless `tagged(false)`; Fagus writes no PDF/A-1a.

### PDF/A-4

PDF/A-4 (ISO 19005-4:2020) is based on PDF 2.0. The file starts with `%PDF-2.0`, and the metadata
is only in XMP: PDF 2.0 deprecates the document information dictionary, which PDF/A-4 forbids, so
the title, author and dates are XMP properties alone. The XMP names `pdfaid:part` 4 and
`pdfaid:rev` 2020, and `pdfaid:conformance` only for the levels e (`E`) and f (`F`). PDF/A-4 has
no levels b, u or a: all text must be mapped to Unicode, and tagging is recommended (Fagus tags
unless `tagged(false)`). As with the levels a, characters from the private use area (the icons of
icon fonts) are refused, since they have no meaning as text.

- PDF/A-4f allows attachments of any type, like PDF/A-3. The plain level allows only PDF/A files,
  which Fagus does not check, and refuses attachments: ``PDF/A-4 allows only PDF/A files as
  attachments; attach `timesheet.csv` to a PDF/A-4f document instead``.
- PDF/A-4e is meant for engineering documents with 3D and rich media, which Fagus does not
  write: the file is a PDF/A-4 file that says it is 4e.
- Factur-X 1.07 (ZUGFeRD 2.3) requires PDF/A-3, so an invoice is not written as PDF/A-4, not even
  PDF/A-4f: `facturX()` with a PDF/A-4 level fails with ``the invoice must be PDF/A-3, not
  PDF/A-4f (choose 3b, 3u or 3a)``.
- PDF/UA-1, which `accessible()` writes, builds on PDF 1.7. PDF/UA-2, which goes with PDF 2.0,
  Fagus does not write yet, so `accessible()` with PDF/A-4 fails with ``PDF/A-4 and PDF/UA-1 need
  different PDF versions and cannot be written together (PDF/UA-1 goes with PDF/A-2 and
  PDF/A-3)``. For an accessible archive, use PDF/A-2a or PDF/A-3a with `accessible()`.

## Attach files

`attach()` adds a file to the PDF, which PDF viewers list in their attachments panel:

```php
use Fagus\Fagus;
use Fagus\Relationship;

Fagus::html($html)
    ->pdfA('3b')
    ->attach('timesheet.csv', $csv, 'text/csv', Relationship::Supplement, 'Stundenzettel')
    ->save('invoice.pdf');
```

| Argument | Default | Meaning |
|---|---|---|
| `$name` | | The file name in the PDF |
| `$data` | | The content |
| `$mime` | `'application/octet-stream'` | The MIME type |
| `$relationship` | `Relationship::Unspecified` | How the file relates to the document |
| `$description` | `null` | A description viewers show; the file name if `null` |

The relationship (PDF 2.0 `AFRelationship`, required by PDF/A-3) is one of `Source` (the file
the document was made from), `Data` (data the document shows), `Alternative` (another form of the
same content), `Supplement` (additional information) or `Unspecified`. The file's modification
date is the document's creation date.

PDF/A-3 and PDF/UA require a description for every attached file. Without `$description`, the
file name describes it, in PHP, in Laravel and on the command line alike.
PDF/A-3 also requires a MIME type of the form `type/subtype`; ``the attached file `notes` has no
valid MIME type (like `text/csv`), which PDF/A-3 requires`` names a file without one.

Attachments work in plain PDF, in PDF/A-3 and in PDF/A-4f, not in PDF/A-1, PDF/A-2 and PDF/A-4. On the command line, `--attach
file` attaches a file as a supplement, with its name as description and a MIME type from its
extension (see [cli.md](cli.md)).

## Make an electronic invoice (ZUGFeRD, Factur-X, XRechnung)

ZUGFeRD 2 and Factur-X are the same standard for hybrid invoices: a PDF/A-3 file that people read,
with the invoice as XML (UN/CEFACT Cross Industry Invoice) attached for machines. `facturX()`
makes such a file from your HTML and the XML your application writes:

```php
Fagus::html($html)->facturX($invoiceXml)->save('invoice.pdf');
```

Fagus then:

- attaches the XML as `factur-x.xml` (as `xrechnung.xml` for the XRechnung profile), with the MIME
  type `text/xml`, the description "Factur-X/ZUGFeRD invoice" (unless you give another, see
  below) and the relationship `Alternative` (`Data` for the profiles Minimum and Basic WL, whose
  XML is not a complete invoice);
- writes the Factur-X properties into the XMP metadata, with their PDF/A extension schema:
  `DocumentType` `INVOICE`, `DocumentFileName`, `Version` (`1.0` unless you give another) and
  `ConformanceLevel`;
- writes PDF/A-3b, or the PDF/A-3 level you chose with `pdfA()`. A PDF/A-2 level is raised to
  the PDF/A-3 level of the same letter, PDF/A-1b to PDF/A-3b, with the warning
  `pdf.conformance-raised` ("PDF/A-2b is written as PDF/A-3b because Factur-X attaches its XML,
  which only PDF/A-3 allows"). A PDF/A-4 level is refused: Factur-X 1.07 requires PDF/A-3 (see
  [PDF/A-4](#pdfa-4)).

### The profile

The profile is read from the XML, from the `ID` in its `GuidelineSpecifiedDocumentContextParameter`:

| `InvoiceProfile` | `ConformanceLevel` | Guideline ID |
|---|---|---|
| `Minimum` | `MINIMUM` | ends in `minimum` |
| `BasicWl` | `BASIC WL` | ends in `basicwl` |
| `Basic` | `BASIC` | ends in `basic` |
| `En16931` | `EN 16931` | starts with `urn:cen.eu:en16931:2017` |
| `Extended` | `EXTENDED` | ends in `extended` |
| `XRechnung` | `XRECHNUNG` | contains `xrechnung` |

An XML that declares none is treated as EN 16931, with the warning `invoice.profile-unknown`. You
can name the profile yourself; if the XML declares another one, the warning
`invoice.profile-mismatch` says so:

```php
use Fagus\InvoiceProfile;

Fagus::html($html)->facturX($xml, InvoiceProfile::XRechnung)->save('invoice.pdf');
```

### The version and the description

`version` sets the `Version` of the XMP metadata and `description` the description of the
attached XML:

```php
Fagus::html($html)
    ->facturX($xml, InvoiceProfile::XRechnung, version: '3.0', description: 'XRechnung Invoice')
    ->save('invoice.pdf');
```

For the XRechnung profile, the Factur-X specification (1.07.2, which is ZUGFeRD 2.3.2, section
7.7) describes `Version` as the major and minor version of the invoice data specification, that is
of XRechnung. Fagus does not derive it from the XML: it writes `1.0` unless you give a version.

An empty version or description is refused: `facturX()` throws an `InvalidArgumentException`. One
with control characters fails at rendering ("the document does not meet Factur-X: the invoice
description contains control characters"). On the command line the options are
`--factur-x-version` and `--factur-x-description`, in Rust `factur_x_version()` and
`factur_x_description()`.

Fagus does not check the content of the XML, and it does not compare it with the HTML. Your
application writes both from the same data; validate the result as described below.

### A complete example

This renders an invoice and its XML to a PDF/A-3b file that veraPDF and Mustang accept. The XML
is [tests/corpus/invoice/factur-x.xml](../tests/corpus/invoice/factur-x.xml), an EN 16931 invoice.

`invoice.html`:

```html
<!doctype html>
<html lang="de">
<head>
  <meta charset="utf-8">
  <title>Rechnung RE-2026-0042</title>
  <meta name="author" content="Muster GmbH">
  <meta name="description" content="Rechnung RE-2026-0042 an Kunde AG">
  <style>
    @page {
      size: A4;
      margin: 20mm 20mm 25mm;
      @bottom-right { content: "Seite " counter(page) " von " counter(pages); font: 9pt sans-serif; }
    }
    body { font: 10pt/1.4 sans-serif; color: #1a1a1a; }
    h1 { font-size: 18pt; margin: 0 0 8mm; }
    .parties { display: flex; justify-content: space-between; margin-bottom: 10mm; }
    table { width: 100%; border-collapse: collapse; }
    th, td { padding: 2mm 0; border-bottom: 0.5pt solid #ccc; text-align: left; }
    .amount { text-align: right; }
    tfoot td { border: 0; }
    tfoot tr:last-child td { font-weight: bold; border-top: 1pt solid #1a1a1a; }
  </style>
</head>
<body>
  <h1>Rechnung RE-2026-0042</h1>
  <div class="parties">
    <p>Muster GmbH<br>Hauptstraße 1<br>12345 Musterstadt</p>
    <p>Kunde AG<br>Bahnhofstraße 5<br>54321 Beispielstadt</p>
  </div>
  <p>Rechnungsdatum: 01.11.2026 · Leistungsdatum: 30.09.2026 · Fällig: 15.11.2026</p>
  <table>
    <thead>
      <tr><th>Pos.</th><th>Leistung</th><th class="amount">Menge</th><th class="amount">Preis</th><th class="amount">Betrag</th></tr>
    </thead>
    <tbody>
      <tr><td>1</td><td>Webentwicklung Sprint 1</td><td class="amount">2 h</td><td class="amount">95,00 €</td><td class="amount">190,00 €</td></tr>
      <tr><td>2</td><td>Webentwicklung Sprint 2</td><td class="amount">2 h</td><td class="amount">95,00 €</td><td class="amount">190,00 €</td></tr>
      <tr><td>3</td><td>Webentwicklung Sprint 3</td><td class="amount">2 h</td><td class="amount">95,00 €</td><td class="amount">190,00 €</td></tr>
    </tbody>
    <tfoot>
      <tr><td colspan="4">Nettobetrag</td><td class="amount">570,00 €</td></tr>
      <tr><td colspan="4">Umsatzsteuer 19 %</td><td class="amount">108,30 €</td></tr>
      <tr><td colspan="4">Gesamtbetrag</td><td class="amount">678,30 €</td></tr>
    </tfoot>
  </table>
  <p>Bitte überweisen Sie den Betrag bis zum 15.11.2026 auf DE02 1203 0000 0000 2020 51.</p>
</body>
</html>
```

<details>
<summary><code>invoice.xml</code></summary>

```xml
<?xml version="1.0" encoding="UTF-8"?>
<rsm:CrossIndustryInvoice xmlns:rsm="urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100"
  xmlns:ram="urn:un:unece:uncefact:data:standard:ReusableAggregateBusinessInformationEntity:100"
  xmlns:udt="urn:un:unece:uncefact:data:standard:UnqualifiedDataType:100">
  <rsm:ExchangedDocumentContext>
    <ram:GuidelineSpecifiedDocumentContextParameter>
      <ram:ID>urn:cen.eu:en16931:2017</ram:ID>
    </ram:GuidelineSpecifiedDocumentContextParameter>
  </rsm:ExchangedDocumentContext>
  <rsm:ExchangedDocument>
    <ram:ID>RE-2026-0042</ram:ID>
    <ram:TypeCode>380</ram:TypeCode>
    <ram:IssueDateTime><udt:DateTimeString format="102">20261101</udt:DateTimeString></ram:IssueDateTime>
  </rsm:ExchangedDocument>
  <rsm:SupplyChainTradeTransaction>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>1</ram:LineID></ram:AssociatedDocumentLineDocument>
      <ram:SpecifiedTradeProduct><ram:Name>Webentwicklung Sprint 1</ram:Name></ram:SpecifiedTradeProduct>
      <ram:SpecifiedLineTradeAgreement><ram:NetPriceProductTradePrice><ram:ChargeAmount>95.00</ram:ChargeAmount></ram:NetPriceProductTradePrice></ram:SpecifiedLineTradeAgreement>
      <ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="HUR">2</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery>
      <ram:SpecifiedLineTradeSettlement>
        <ram:ApplicableTradeTax><ram:TypeCode>VAT</ram:TypeCode><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>19</ram:RateApplicablePercent></ram:ApplicableTradeTax>
        <ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>190.00</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation>
      </ram:SpecifiedLineTradeSettlement>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>2</ram:LineID></ram:AssociatedDocumentLineDocument>
      <ram:SpecifiedTradeProduct><ram:Name>Webentwicklung Sprint 2</ram:Name></ram:SpecifiedTradeProduct>
      <ram:SpecifiedLineTradeAgreement><ram:NetPriceProductTradePrice><ram:ChargeAmount>95.00</ram:ChargeAmount></ram:NetPriceProductTradePrice></ram:SpecifiedLineTradeAgreement>
      <ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="HUR">2</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery>
      <ram:SpecifiedLineTradeSettlement>
        <ram:ApplicableTradeTax><ram:TypeCode>VAT</ram:TypeCode><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>19</ram:RateApplicablePercent></ram:ApplicableTradeTax>
        <ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>190.00</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation>
      </ram:SpecifiedLineTradeSettlement>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>3</ram:LineID></ram:AssociatedDocumentLineDocument>
      <ram:SpecifiedTradeProduct><ram:Name>Webentwicklung Sprint 3</ram:Name></ram:SpecifiedTradeProduct>
      <ram:SpecifiedLineTradeAgreement><ram:NetPriceProductTradePrice><ram:ChargeAmount>95.00</ram:ChargeAmount></ram:NetPriceProductTradePrice></ram:SpecifiedLineTradeAgreement>
      <ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="HUR">2</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery>
      <ram:SpecifiedLineTradeSettlement>
        <ram:ApplicableTradeTax><ram:TypeCode>VAT</ram:TypeCode><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>19</ram:RateApplicablePercent></ram:ApplicableTradeTax>
        <ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>190.00</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation>
      </ram:SpecifiedLineTradeSettlement>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:ApplicableHeaderTradeAgreement>
      <ram:BuyerReference>K-1007</ram:BuyerReference>
      <ram:SellerTradeParty>
        <ram:Name>Muster GmbH</ram:Name>
        <ram:PostalTradeAddress><ram:PostcodeCode>12345</ram:PostcodeCode><ram:LineOne>Hauptstraße 1</ram:LineOne><ram:CityName>Musterstadt</ram:CityName><ram:CountryID>DE</ram:CountryID></ram:PostalTradeAddress>
        <ram:SpecifiedTaxRegistration><ram:ID schemeID="VA">DE123456789</ram:ID></ram:SpecifiedTaxRegistration>
      </ram:SellerTradeParty>
      <ram:BuyerTradeParty>
        <ram:Name>Kunde AG</ram:Name>
        <ram:PostalTradeAddress><ram:PostcodeCode>54321</ram:PostcodeCode><ram:LineOne>Bahnhofstraße 5</ram:LineOne><ram:CityName>Beispielstadt</ram:CityName><ram:CountryID>DE</ram:CountryID></ram:PostalTradeAddress>
      </ram:BuyerTradeParty>
    </ram:ApplicableHeaderTradeAgreement>
    <ram:ApplicableHeaderTradeDelivery>
      <ram:ActualDeliverySupplyChainEvent><ram:OccurrenceDateTime><udt:DateTimeString format="102">20260930</udt:DateTimeString></ram:OccurrenceDateTime></ram:ActualDeliverySupplyChainEvent>
    </ram:ApplicableHeaderTradeDelivery>
    <ram:ApplicableHeaderTradeSettlement>
      <ram:InvoiceCurrencyCode>EUR</ram:InvoiceCurrencyCode>
      <ram:SpecifiedTradeSettlementPaymentMeans>
        <ram:TypeCode>58</ram:TypeCode>
        <ram:PayeePartyCreditorFinancialAccount><ram:IBANID>DE02120300000000202051</ram:IBANID></ram:PayeePartyCreditorFinancialAccount>
      </ram:SpecifiedTradeSettlementPaymentMeans>
      <ram:ApplicableTradeTax>
        <ram:CalculatedAmount>108.30</ram:CalculatedAmount><ram:TypeCode>VAT</ram:TypeCode><ram:BasisAmount>570.00</ram:BasisAmount><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>19</ram:RateApplicablePercent>
      </ram:ApplicableTradeTax>
      <ram:SpecifiedTradePaymentTerms><ram:DueDateDateTime><udt:DateTimeString format="102">20261115</udt:DateTimeString></ram:DueDateDateTime></ram:SpecifiedTradePaymentTerms>
      <ram:SpecifiedTradeSettlementHeaderMonetarySummation>
        <ram:LineTotalAmount>570.00</ram:LineTotalAmount>
        <ram:TaxBasisTotalAmount>570.00</ram:TaxBasisTotalAmount>
        <ram:TaxTotalAmount currencyID="EUR">108.30</ram:TaxTotalAmount>
        <ram:GrandTotalAmount>678.30</ram:GrandTotalAmount>
        <ram:DuePayableAmount>678.30</ram:DuePayableAmount>
      </ram:SpecifiedTradeSettlementHeaderMonetarySummation>
    </ram:ApplicableHeaderTradeSettlement>
  </rsm:SupplyChainTradeTransaction>
</rsm:CrossIndustryInvoice>
```

</details>

`render.php`:

```php
<?php

require 'vendor/autoload.php';

use Fagus\Fagus;

$pdf = Fagus::html(file_get_contents(__DIR__.'/invoice.html'))
    ->facturX(file_get_contents(__DIR__.'/invoice.xml'))
    ->created(new DateTimeImmutable('2026-11-01 09:00', new DateTimeZone('Europe/Berlin')))
    ->render();

$pdf->save(__DIR__.'/invoice.pdf');

foreach ($pdf->diagnostics() as $diagnostic) {
    echo $diagnostic, "\n";
}
```

The result is a one-page PDF/A-3b file of about 23 KB with `factur-x.xml` attached and the
profile EN 16931 in its metadata. For an invoice that is also accessible, ask for PDF/A-3a and
PDF/UA-1; the HTML above meets both:

```php
use Fagus\Conformance;

Fagus::html($html)
    ->pdfA(Conformance::PdfA3a)
    ->accessible()
    ->facturX($xml)
    ->save('invoice.pdf');
```

In Laravel, the same methods follow the view:

```php
use Fagus\Laravel\Facades\Fagus;

return Fagus::view('invoices.show', ['invoice' => $invoice])
    ->facturX($xml)
    ->download("{$invoice->number}.pdf");
```

On the command line:

```sh
fagus render invoice.html -o invoice.pdf --factur-x invoice.xml
```

### Validate the result

Fagus checks PDF/A and PDF/UA while it writes the file, but not the invoice XML. Two free tools
check the result:

- [veraPDF](https://verapdf.org) checks PDF/A and PDF/UA. It reads the PDF/A level from the
  file's metadata:

  ```sh
  verapdf --format text invoice.pdf            # PDF/A
  verapdf --format text -f ua1 invoice.pdf     # PDF/UA-1
  ```

- [Mustang](https://www.mustangproject.org) checks the PDF/A file and the attached XML against the
  rules of ZUGFeRD, Factur-X and XRechnung:

  ```sh
  java -jar Mustang-CLI.jar --action validate --source invoice.pdf
  ```

  A valid invoice reports `<summary status="valid"/>`.

The repository's script `tools/validate.sh` renders every document of the test corpus as
PDF/A-1b, 2b, 2u, 3b, 3a, 4, 4e and 4f, as PDF/UA-1 and as Factur-X invoices (the invoices also
signed with test keys), and checks them with both tools (documents that Fagus refuses for a level, such as those with transparency for
PDF/A-1b, are left out of it):

```sh
VERAPDF=/path/to/verapdf MUSTANG=/path/to/Mustang-CLI.jar tools/validate.sh /tmp/validate
```

It uses `target/release/fagus`, or the binary in `FAGUS`. With veraPDF 1.30.3 and Mustang
2.26.0 every file passes.

## Protect the PDF with a password

`encrypt()` encrypts the PDF with AES-256, as PDF 2.0 defines it (the standard security handler,
revision 6). It takes a user password, an owner password and permissions:

```php
use Fagus\Fagus;
use Fagus\Permission;

Fagus::html($html)
    ->encrypt(
        userPassword: 'open sesame',
        ownerPassword: 'change me',
        permissions: [Permission::Print, Permission::Copy],
    )
    ->save('salary.pdf');
```

- The **user password** is needed to open the PDF. Without one, the PDF opens without a password,
  but with only the permissions given.
- The **owner password** opens the PDF too, and lifts the restrictions of the permissions. Without
  one, Fagus uses a random password that nobody knows, so the restrictions stay. Giving both the
  same password lifts the restrictions for everyone who can open the file.
- The **permissions** say what a reader with the user password (or with none, if there is none)
  may do. Without the argument, everything is allowed; with an empty list, nothing beyond reading.

At least one of the passwords is needed. `encrypt(ownerPassword: 'change me', permissions: [])`
makes a PDF that opens for everyone but cannot be printed, copied from or changed.

| Permission | Allows |
|---|---|
| `Permission::Print` (`print`) | Printing, in full quality |
| `Permission::PrintLowQuality` (`print-low-quality`) | Printing, only as a low-resolution image |
| `Permission::Modify` (`modify`) | Changing the content |
| `Permission::Copy` (`copy`) | Copying text and images |
| `Permission::Annotate` (`annotate`) | Adding and changing comments, and filling in forms |
| `Permission::FillForms` (`fill-forms`) | Filling in form fields |
| `Permission::Assemble` (`assemble`) | Inserting, deleting and rotating pages, changing bookmarks |

Screen readers and other assistive technology may always extract the text: PDF 2.0 says to allow
it in every encrypted file, and Fagus does. No permission takes it away, so `accessible()` and
`encrypt()` go together (see below).

What to know about encrypted PDFs:

- The permissions are a request to the PDF viewer, which Acrobat and most other viewers honour.
  Anyone who can open the file can read its content, and tools exist that ignore the
  permissions. Only the user password keeps people out.
- Passwords are prepared with SASLprep (RFC 4013), as PDF 2.0 requires, and used as UTF-8, up to
  127 bytes; longer ones are cut there. SASLprep makes passwords that look the same the same:
  `ä` typed as one character or as `a` and a combining mark, a no-break space or another space
  (the ASCII space), ligatures and full-width letters (`ﬁ` is `fi`); soft hyphens and zero-width
  spaces go. Right-to-left passwords (Arabic, Hebrew) work; one that mixes right-to-left
  letters with Latin ones, or has a control character, a private-use character or a direction
  mark, is refused when the PDF is rendered (``the user password cannot be used: …``, which
  does not show the password). Emoji and other characters newer than Unicode 3.2 are kept.
  Viewers that follow PDF 2.0 prepare a typed password in the same way; others, and tools such
  as qpdf, take its UTF-8 as it is, so they need the prepared form, which for most passwords is
  the password itself.
- Everything is encrypted: the content of the pages, fonts, images, attachments, the XMP metadata
  and the strings of the document information (title, author), so a search engine or a file
  manager sees no title either.
- AES-256 needs a PDF 2.0 reader or a PDF 1.7 one with Adobe's extension level 8, which the PDF
  declares: Acrobat 9 and later, and every current viewer, browser and library (pdf.js, PDFium,
  Preview, Poppler, qpdf, pypdf) open it. Older encryption (RC4, AES-128) is not available: it
  is deprecated in PDF 2.0, and RC4 is broken.
- PDF/A forbids encryption, so `encrypt()` cannot go with `pdfA()` or `facturX()` (which
  writes PDF/A-3): rendering fails with ``the document does not meet PDF/A-3b: the PDF cannot be
  encrypted (PDF/A forbids encryption)``. Encrypt a copy of the archived invoice if you need
  one.
- PDF/UA (`accessible()`) allows encryption, as long as assistive technology may extract the
  text (ISO 14289-1, §7.16), which every encrypted PDF of Fagus allows, whatever the
  permissions. The XMP metadata, with the PDF/UA identification and the title, is encrypted
  like everything else and readable once the PDF is opened, with the user password or without
  one where there is none; PDF/UA does not ask for more. veraPDF reads encrypted files only
  without a user password: check the others decrypted (`qpdf --password=… --decrypt`), after
  `qpdf --show-encryption` has shown `extract for accessibility: allowed`.
- An encrypted PDF can be signed: `encrypt()` with `sign()` encrypts the signature's update with
  the file key too (see [Sign an encrypted PDF](#sign-an-encrypted-pdf)).
- The file key, its salts and the initialization vectors are random, from the operating system:
  an encrypted PDF is not byte-reproducible (see
  [Make the output reproducible](#make-the-output-reproducible)), even with `created()`. The Rust
  library has `Encryption::seed()` for tests that compare bytes.

On the command line, `--user-password`, `--owner-password` and `--permissions print,copy` encrypt
the PDF; the passwords can come from files or environment variables, so that they do not show up
in the list of processes (see [cli.md](cli.md#protect-the-pdf-with-a-password)).

To check an encrypted file, `qpdf --password=… --show-encryption file.pdf` shows the encryption
and the permissions, and `qpdf --password=… --decrypt file.pdf plain.pdf` writes it unencrypted.

## Sign the PDF

`sign()` signs the PDF digitally, so that readers can see who issued it and that nobody has
changed it since. The signature follows PAdES (ETSI EN 319 142-1), the European standard for
signed PDF, as a baseline signature of level B-B, B-T with a timestamp, B-LT with the validation
data to check it without the network, or B-LTA with an archive timestamp after that. It is
invisible unless you place it (see [Visible signatures](#visible-signatures)): PDF viewers show
it in their signature panel.

```php
use Fagus\Fagus;
use Fagus\Signature;

Fagus::html($html)
    ->pdfA('3b')
    ->sign(Signature::fromPkcs12('/run/secrets/signing.p12', $password)
        ->reason('Rechnung')
        ->location('Berlin'))
    ->save('invoice.pdf');
```

The key comes from a PKCS#12 file (`.p12` or `.pfx`, as certificate authorities issue them and
OpenSSL, Java and Windows export them), RSA or ECDSA on the curve P-256. The file's other
certificates, those of the chain up to the root, go into the signature too, so that readers can
check it without looking them up. A reader trusts the signature when it trusts the root: one of
the Adobe Approved Trust List (AATL) or the EU Trusted Lists for viewers such as Acrobat, or your
own certificate authority inside a company.

`Signature::fromPkcs12Data($data, $password)` takes the file's bytes instead of a path, for keys
that come from a secrets manager or a database rather than from a file.

| Method | Meaning |
|---|---|
| `name(string $name)` | The name of the signer as the signature shows it; readers show the name in the certificate otherwise |
| `reason(string $reason)` | Why the document is signed |
| `location(string $location)` | Where it is signed |
| `contactInfo(string $contactInfo)` | How to reach the signer |
| `time(DateTimeInterface $time)` | The time of signing; the creation date of the document (see [`created()`](#make-the-output-reproducible)) by default |
| `timestamp(callable $timestamper)` | Timestamps the signature (B-T), see below |
| `certify(Certification $certification)` | Makes it a certification signature, see [Certify the PDF](#certify-the-pdf) |
| `visibleOn(string $id)`, `visibleAt(int $page, float $x, float $y, float $width, float $height)` | Shows the signature on a page, see [Visible signatures](#visible-signatures) |
| `revocation(callable $revocation)` | Adds the validation data (B-LT), see [Long-term signatures](#long-term-signatures-pades-b-lt-and-b-lta) |
| `archiveTimestamp()` | Adds an archive timestamp after the validation data (B-LTA) |

What the signature contains:

- CMS signed data (`ETSI.CAdES.detached`) with SHA-256, over the whole file except the signature
  itself. The signed attributes name the signer's certificate (`signing-certificate-v2`) and, as
  PAdES requires, no signing time: the time of signing is the signature dictionary's `/M`, which
  readers show as the claimed time.
- An invisible signature field (a widget of no size) on the first page, in the document's form,
  or the visible field it is placed in.

Fagus adds the signature to the finished PDF as an incremental update: the file as it would be
without the signature, then the signature field, the signature and a cross-reference section of
their own. The signature covers every byte before and after its value. Signing works with every
PDF/A level (PDF/A-1, -2, -3 and -4 allow signatures), with tagged PDF and with Factur-X
invoices: signed files pass veraPDF (every PDF/A level and PDF/UA-1) and Mustang as unsigned
ones do. The field has an empty appearance, which PDF/A-1 requires, and stays out of the logical
structure of a tagged PDF, as a field of no size that shows nothing.

Change nothing after signing: any program that writes the file again (to compress it, merge it
or add pages) breaks the signature. A signed PDF can be signed again by a program that adds its
signature as another incremental update, such as Acrobat.


### Sign an encrypted PDF

`sign()` together with `encrypt()` (or `--sign-p12` with a password on the command line) signs
the encrypted PDF: the signature's incremental update is encrypted with the file key, as the rest
of the file is, except the signature value itself, which ISO 32000-2 leaves unencrypted so that
readers can check it. The signature covers the encrypted bytes; a reader that opens the file with
the user password (or none, if there is only an owner password) validates it as usual.
Acrobat, pyHanko and qpdf (`qpdf --password=… --check`) read such files.

### Certify the PDF

A certification signature (DocMDP) is the author's signature: besides who signed, it says what
later changes the document allows. Viewers such as Acrobat show it as *certified* and report the
certification as broken when a change goes beyond what it allows.

```php
use Fagus\Certification;
use Fagus\Signature;

$signature = Signature::fromPkcs12($path, $password)->certify(Certification::FillForms);
```

| `Certification` | Allows |
|---|---|
| `Certification::NoChanges` (`no-changes`) | No changes |
| `Certification::FillForms` (`fill-forms`) | Filling in the form's fields and signing them |
| `Certification::Annotate` (`annotate`) | Filling in, signing, and adding or changing comments |

Validation data and document timestamps (B-LT, B-LTA) may follow every certification; only in
PDF/A-1, which requires an appearance for every field, does the archive timestamp's (empty)
appearance count as a change that `NoChanges` forbids, for validators such as pyHanko. A
certification signature must be the first signature of the PDF, which it is when Fagus signs the
PDF it renders. On the command line, `--sign-certify fill-forms`.

### Visible signatures

A visible signature shows on a page: a signature field with a box that viewers draw, and that
they let the reader click to see who signed. `visibleOn()` places it in the box of an element
of the document, found by its `id`; what the element's content paints (its text, images and the
boxes inside it) becomes the field's appearance, while the element's own background and border
stay on the page:

```php
$html = <<<'HTML'
    <div id="signature" style="width: 7cm; border: 1px solid #999; padding: 4px">
        <strong>Digitally signed</strong><br>Rechnungsstelle, Berlin<br>2026-11-19
    </div>
    HTML;

Fagus::html($html)
    ->sign(Signature::fromPkcs12($path, $password)->visibleOn('signature'))
    ->save('invoice.pdf');
```

`visibleAt(page: 1, x: 340, y: 700, width: 200, height: 60)` places the field in an area of a
page instead, in points from the top left corner of the page (as
[`reportLayout()`](php.md#find-where-elements-landed) reports the boxes of elements),
where it shows what the page has there. The field is called `Signature1` (or with the first
number that no field of the form has); readers that do not see it get the element's
`aria-label`, `title` or text as its description, which a PDF/UA document needs. A visible
signature works with tagged PDF, PDF/UA, every PDF/A level and encryption.

Rendering fails with ``the document has no element with the id `signature` for the signature``
if there is no such element, and with ``the element for the signature has no box to show it in``
if it is not rendered (`display: none`) or transformed. An element broken across pages gets a
widget on each page, all of one field. Positioned descendants with a `z-index` of their own are
painted with their stacking context, on the page. On the command line, `--sign-element signature`.

### Long-term signatures (PAdES B-LT and B-LTA)

A signature can be checked as long as its certificates can: once they expire, or their authority
stops answering, a reader cannot find out whether they were valid when the document was signed.
Level B-LT puts what the check needs into the PDF: the certificates and their revocation status,
OCSP responses or certificate revocation lists (CRLs), in the document security store (DSS), an
incremental update after the signature. Level B-LTA adds a document timestamp after that, which
proves when the validation data was there and can be renewed before its algorithms grow weak.

Fagus does not go to the network: `revocation()` takes a callback that gets a
`Fagus\RevocationRequest` for each certificate of the signature and of its timestamp but the
roots, and returns what the certificate's OCSP responder or CRL distribution point answered:

```php
use Fagus\RevocationRequest;
use Fagus\Signature;
use Illuminate\Support\Facades\Http;

$signature = Signature::fromPkcs12($path, $password)
    ->timestamp(fn (string $digest): string => Http::withBody(Signature::timestampRequest($digest), 'application/timestamp-query')
        ->post('https://freetsa.org/tsr')->throw()->body())
    ->revocation(function (RevocationRequest $request): array {
        foreach ($request->ocspUrls as $url) {
            $response = Http::withBody($request->ocspRequest, 'application/ocsp-request')->post($url);
            if ($response->successful()) {
                return [$response->body()];
            }
        }
        foreach ($request->crlUrls as $url) {
            return [Http::get($url)->throw()->body()];
        }

        return [];
    })
    ->archiveTimestamp();
```

| `RevocationRequest` | |
|---|---|
| `certificate` | The certificate (DER) |
| `issuer` | The certificate of its issuer (DER) |
| `ocspUrls` | The OCSP responders the certificate names (authority information access) |
| `crlUrls` | The CRLs the certificate names (CRL distribution points) |
| `ocspRequest` | The OCSP request for the certificate (DER), to POST with the content type `application/ocsp-request` |

The callback returns a list of OCSP responses and CRLs (DER), any number of each, and may add
certificates the check needs, such as that of an OCSP responder that its response does not
hold; an empty list for a certificate whose status cannot be had. Fagus fails with ``the
revocation data is neither an OCSP response nor a CRL`` for anything else, and with ``the OCSP
responder did not answer the request`` for an OCSP response that says so; it does not check the
answers further. B-LT needs a timestamp too (`timestamp()`): without one, the validation data is
added all the same, which Acrobat calls *LTV enabled*, but the signature is not of a PAdES level.
`archiveTimestamp()` uses the callback of `timestamp()` once more, for a timestamp of the whole
file, and needs `timestamp()` and `revocation()`. Its authority's certificates get no validation
data of their own; they are usually those of the signature's timestamp, whose data is there.

### Reproducible signatures

The same document, key and time give the same file: RSA signatures (PKCS#1 v1.5) are the same
for the same data, ECDSA signatures are made deterministic as RFC 6979 describes, and the time
of signing is the document's creation date or the time you give. Fix `created()` and the signed
file is byte for byte the same on every run. Timestamps, revocation data and encrypted files are
not: they carry the authority's time, the responder's, and random initialization vectors.

### Keys in a hardware security module or key management service

Keys that must not leave a hardware security module (HSM) or a key management service (AWS KMS,
Azure Key Vault, Google Cloud KMS) sign through a callback. Fagus computes the SHA-256 digest of
what is signed (the signed attributes of the signature), the callback signs it and returns the
signature; you give the certificates:

```php
use Fagus\Signature;

$signature = Signature::withSigner(
    certificates: [file_get_contents('signer.pem'), file_get_contents('intermediate.pem')],
    signer: fn (string $digest, string $algorithm): string => $kms->signDigest($keyId, $digest),
);
```

`$certificates` are the signer's certificate first, then those of its chain, each PEM or DER. The
callback gets the 32 bytes of the digest and the algorithm, which follows from the certificate:
`Signature::RSA_SHA256` (`'RSA-SHA256'`) or `Signature::ECDSA_P256_SHA256`
(`'ECDSA-P256-SHA256'`). It returns the signature of the digest as it is, without hashing it
again: for RSA the PKCS#1 v1.5 signature, for ECDSA the DER-encoded signature or the 64 bytes of
r and s. Key management services call this signing a digest (AWS KMS: `MessageType` `DIGEST`).
Fagus checks the signature against the certificate and fails with ``the signer's RSA-SHA256
signature does not match the certificate`` if it does not fit, for example because the digest was
hashed once more.

With an RSA key in a PEM file, PHP's OpenSSL extension signs a digest like this (the prefix is
the `DigestInfo` of SHA-256, which PKCS#1 v1.5 signs):

```php
$key = openssl_pkey_get_private(file_get_contents('signer.key'));
$signer = function (string $digest) use ($key): string {
    openssl_private_encrypt(hex2bin('3031300d060960864801650304020105000420').$digest, $signature, $key);

    return $signature;
};
```

### Timestamps (PAdES B-T)

A timestamp from a time-stamping authority (TSA) proves that the signature existed at that time,
which keeps it valid after the signer's certificate has expired. Fagus does not go to the
network, so a callback asks the authority: it gets the SHA-256 digest of the signature and
returns the authority's answer (RFC 3161), the timestamp token or the whole response.
`Signature::timestampRequest()` makes the request to send:

```php
use Fagus\Signature;
use Illuminate\Support\Facades\Http;

$signature = Signature::fromPkcs12($path, $password)->timestamp(
    fn (string $digest): string => Http::withBody(Signature::timestampRequest($digest), 'application/timestamp-query')
        ->post('https://freetsa.org/tsr')
        ->throw()
        ->body(),
);
```

With Guzzle:

```php
$client = new GuzzleHttp\Client();
$signature = Signature::fromPkcs12($path, $password)->timestamp(
    fn (string $digest): string => (string) $client->post('https://freetsa.org/tsr', [
        'headers' => ['Content-Type' => 'application/timestamp-query'],
        'body' => Signature::timestampRequest($digest),
    ])->getBody(),
);
```

FreeTSA is a free public authority, good for trying it out; qualified timestamps (eIDAS) come
from qualified trust service providers, often with a login. Fagus checks that the token is a
timestamp of the signature and fails with ``the timestamp is not one of this signature`` if it is
not. It reserves 12 KiB for the token; should an authority's be larger, it signs once more with
room for it, so the callbacks may be called twice for one document.

### On the command line and in Rust

`fagus render --sign-p12 signing.p12` signs with a PKCS#12 file; the password comes from the
environment variable `FAGUS_SIGN_PASSWORD` or a file (`--sign-password-file`), not from the
command line, where other users of the machine could read it (see [cli.md](cli.md)).
`--sign-certify` and `--sign-element` certify and place the signature. The command line does not
timestamp and adds no validation data. In Rust, `Document::sign` takes a `fagus::Signature`,
with the traits `Signer`, `Timestamper` and `Revocation` for keys, authorities and responders
outside (see [rust.md](rust.md)).

### Check a signature

Acrobat Reader shows the signature in its signature panel; it trusts it if it trusts the root
certificate. On the command line, [pyHanko](https://github.com/MatthiasValvekens/pyHanko) checks
PAdES signatures:

```sh
pyhanko sign validate --pretty-print --trust root.pem invoice.pdf
pyhanko sign validate --pretty-print --trust root.pem --password=… encrypted.pdf
```

For B-LT and B-LTA, pyHanko's `validate_pdf_ltv_signature()` checks the signature with the
validation data in the file alone.

## Make the output reproducible

A PDF contains its creation date, by default the current time, so two renderings of the same
document differ. With a fixed date, the same input gives a byte-identical file:

```php
Fagus::html($html)
    ->created(new DateTimeImmutable('2026-11-01 09:00', new DateTimeZone('Europe/Berlin')))
    ->save('invoice.pdf');
```

`created()` takes any `DateTimeInterface`; the PHP package converts it to UTC. On the command line,
`--created` takes a date in UTC (`2026-11-01T08:00:00Z`) or with an offset
(`2026-11-01T09:00:00+01:00`), which is converted to UTC (see [cli.md](cli.md)). The PDF holds the
date in UTC.

The output is the same for the same HTML, resources, fonts, options and version of Fagus, on
every platform. Fonts installed on the machine (`systemFonts: true`) make it depend on the
machine, and an encrypted PDF differs every time: its keys are random (see
[Protect the PDF with a password](#protect-the-pdf-with-a-password)).

## File size and compression

Fagus keeps files small without options to set:

- fonts are embedded as subsets, with only the glyphs the document uses;
- page contents, fonts and images are compressed (Flate); all other objects go into compressed
  object streams with a cross-reference stream;
- an image used many times is embedded once, and a background of more than 4096 tiles is one
  tiling pattern;
- JPEG images are embedded as they are, without decoding and compressing them again.

The pixels of other images are compressed without loss, which keeps every pixel and makes a
photo large: `imageQuality()` writes them as JPEGs instead (see
[Make photos smaller](#make-photos-smaller)).

The invoice above is 16 KB as a tagged PDF, 15 KB without tags and 23 KB as PDF/A-3 with its XML
(which also carries an ICC profile). The tags grow with the number of elements, most with large
tables; `tagged(false)` leaves them out where accessibility is not needed.

The binary compresses on two threads of its own while it draws the pages; the environment variable
`RAYON_NUM_THREADS` sets how many.

### Make photos smaller

A PDF keeps the pixels of a PNG, WebP, GIF, AVIF, BMP or TIFF image without loss. That is right
for logos, charts and screenshots, but a photo then takes many times the room of its file: a
WebP photo of 18 KB (480 × 320 pixels) becomes 200 KB in the PDF. `imageQuality()` writes such images as JPEGs,
which PDF stores as they are:

```php
Fagus::html($html)->imageQuality(80)->save('catalog.pdf');
```

The quality goes from 1 (the smallest file) to 100 (the best picture); 80 is a good value for
screens and office printers, 90 and more for print. A page with that photo and nothing else:

| Quality | Size of the PDF |
|---|--:|
| none (without loss) | 202 KB |
| 100 | 143 KB |
| 95 | 60 KB |
| 90 | 42 KB |
| 80 | 26 KB |
| 50 | 14 KB |

A photo of 1200 × 1200 pixels (68 KB as WebP) makes a PDF of 1.7 MB without the option and of
122 KB at a quality of 80. Writing a JPEG is also faster than compressing the pixels without
loss: that page renders in about half the time.

What becomes a JPEG, and what does not:

| Image | With `imageQuality()` |
|---|---|
| PNG, GIF, WebP, AVIF, BMP and TIFF images whose pixels are all opaque | A JPEG of the quality |
| The same with transparent pixels | As without it: pixels without loss and a soft mask. A JPEG has no transparency, and its artifacts show at soft edges |
| PNG, GIF, lossless WebP, BMP and TIFF images whose JPEG would be no smaller than their file | As without it. Logos, charts, QR codes and scans in black and white compress better without loss, and stay sharp |
| JPEG | Embedded as it is, as always: never compressed a second time |
| A CMYK JPEG without an ICC profile, which is converted to sRGB | A JPEG of the quality |
| Images with a `filter` | A JPEG of the quality where the filtered pixels are all opaque (a blur or shadow makes transparent ones) |
| SVG, and pages of PDF files | Vector graphics, as always. Raster images inside an SVG image stay without loss |
| Shadows, gradients and SVG filters that Fagus draws as pixels | Without loss |

The JPEGs are baseline JPEGs that every viewer reads. Below a quality of 90 their color has half
the resolution of their brightness (4:2:0, as cameras save photos), from 90 on the full
resolution (4:4:4), which keeps colored text and thin colored lines sharp. An image whose pixels
are all gray becomes a gray JPEG, and the ICC profile of an image goes into its JPEG. Images of
16 bits per channel become 8 bits. The same image and quality give the same bytes on every
platform, so the output stays [reproducible](#make-the-output-reproducible).

A screenshot with text can be smaller as a JPEG and still look worse: the rule above only looks
at the size. Choose a quality of 90 or more for documents with screenshots, or leave the option
off for them.

JPEG is allowed in every PDF/A level and in PDF/UA, so `imageQuality()` goes with `pdfA()` and
`accessible()`. In PDF/A-1b, which has no transparency, an image with transparent pixels is
refused with or without it.

The option does not scale images down: a photo of 12 megapixels stays one of 12 megapixels.
Scale large photos to the size they are printed at before you hand them to the document.

On the command line the option is `--image-quality 80`, in Rust `Document::image_quality(80)`,
and in Laravel the `image_quality` key of `config/fagus.php` sets it for every document. The
JPEG encoder is the crate [jpeg-encoder](https://github.com/vstroebel/jpeg-encoder); this
software is based in part on the work of the Independent JPEG Group.

## Images in the PDF

Fagus reads PNG, JPEG, GIF, WebP, AVIF, BMP, TIFF and SVG images, and pages of PDF files. Other formats (JPEG XL, ICO …)
report `image.unsupported` and are left out; `<picture>` and `srcset` skip JPEG XL sources, as
Chrome chooses among the formats it can show. An image of more than 200 megapixels reports
`image.too-large`, one that does not decode `image.broken`.

How images go into the PDF:

- JPEG as it is, compressed as JPEG. A CMYK JPEG without an ICC profile is converted to sRGB as
  Chrome draws it, instead of leaving the conversion to the viewer.
- PNG, GIF and WebP as their pixels, compressed without loss (or as JPEGs with `imageQuality()`,
  see [Make photos smaller](#make-photos-smaller)); transparency becomes a soft mask.
  A PNG whose `gAMA` chunk Chrome corrects for is corrected the same way. An animated GIF shows
  its first frame.
- AVIF as its pixels, decoded by Fagus, compressed without loss (see [AVIF](#avif)).
- BMP and TIFF as their pixels, decoded by Fagus, compressed without loss. BMP is read as Chrome
  reads it; TIFF, which Chrome does not show, as Safari shows it: its first image, gray, RGB, CMYK
  (converted to RGB as CMYK JPEGs) or YCbCr, uncompressed or compressed with LZW, Deflate,
  PackBits, CCITT Group 4 or JPEG. Palette and Lab TIFFs and CCITT Group 3 report `image.broken`.
- SVG as vector graphics, with their text, gradients and patterns. Filtered SVG and SVG filters
  that PDF cannot express are drawn as pixels at 300 dpi, as Chrome prints them.
- A page of a PDF file, which Chrome does not show, as Safari and Prince show it: as a Form
  XObject, with its text, vector graphics, fonts and images as they are in the file, as large as
  its crop box (see [Use a PDF as page template](paged-media.md#use-a-pdf-as-page-template)).
  Its links, annotations and tags are left out, and filters (`filter`, shadows) do not change it.
- At their full resolution: Fagus does not scale images down, with `imageQuality()` either.
  Scale large photos before you hand them to the document.
- Without interpolation (`/Interpolate`), as Chrome writes them: viewers decide how to smooth.

### AVIF

AVIF images are read as Chrome 157 reads them:

- 8, 10 and 12 bits, 4:2:0, 4:2:2, 4:4:4 and monochrome, of full or limited range. The chroma of
  subsampled images is upsampled bilinearly, and YUV becomes RGB with the matrix the image names
  (BT.601, BT.709, BT.2020, FCC, SMPTE 240M, YCgCo, YCgCo-R, identity, or derived from the
  primaries), as libavif converts it.
- An alpha channel (an auxiliary image), also premultiplied; a grid of images in one; an
  animation (`avis`) shows its first frame.
- `irot` and `imir` turn the image as Chrome turns it; they are part of the image, not metadata,
  so `image-orientation: none` keeps them. A clean aperture (`clap`) crops the image where it
  starts at its top left corner, the only ones Chrome applies.
- An ICC profile goes into the PDF with the pixels, as for PNG and JPEG. Without one, the color
  code points (`nclx`, or the AV1 sequence header) say what the colors are: sRGB and the BT.709
  family of transfer functions stay as they are, as Chrome draws them; other primaries (Display P3,
  BT.2020 …) and transfer functions (gamma 2.2 and 2.8, linear, SMPTE 240M and 428) are converted
  to sRGB. HDR images (PQ and HLG) are tone mapped to SDR as Chrome 157 maps them on an SDR
  display. Logarithmic transfer functions, IEC 61966-2-4 and BT.1361, which Chrome does not show,
  report `image.broken`, as do constant-luminance matrices and ICtCp.

The AV1 pictures are decoded by [rav1d](https://github.com/memorysafety/rav1d), the Rust port of
dav1d, without its assembly: the build needs no C compiler or assembler, and AV1 decoding gives
the same pixels on every platform. It is several times slower than Chrome's dav1d: a photo of 6
megapixels takes 0.2 to 0.7 seconds on one core. Decoded images are kept for the next document,
as other images are.

## Gradients, filters and differences from Chrome

Fagus draws what it can as vectors, which stay sharp at any zoom and print resolution:

- Linear, radial and conic gradients are PDF shadings. They interpolate between their stops in
  sRGB as Chrome's PDF output does, whatever color space the gradient names, with color hints
  mixed in the named space. Where Chrome prints a gradient with translucent stops in another color
  space (`in oklch`) as an image, Fagus samples it into a shading. A conic gradient with more than
  96 stops (a repeating one of short periods) and every conic gradient in PDF/A is an image.
- `filter`: Chrome prints filtered content as an image. Fagus applies color filters to the colors
  of what the box draws and keeps text and shapes as vectors where they stay sharp; `blur()` and
  SVG filters without a vector form are drawn as pixels.
- `mix-blend-mode`, `background-blend-mode` and `isolation` are PDF blend modes in transparency
  groups; `mask` and `clip-path` are soft masks and clipping paths.
- `@page` backgrounds paint their colors and images but no gradients, as in Chrome.

Other places where Fagus deliberately prints differently from Chrome (the A4 default page size,
the language of form controls, fonts in margin boxes) are listed in [css.md](css.md),
[paged-media.md](paged-media.md) and [fonts.md](fonts.md).
