# Paged media

Fagus takes the page size, margins, headers, footers and page breaks from the document's CSS, as a
browser does when it prints. There are no PHP options for them: everything on this page goes into
the stylesheet. This keeps the layout in one place and lets you check a template in Chrome's print
preview, which shows the same result for nearly everything here. Where Fagus differs from Chrome,
this page says so.

All examples are complete documents. Render one with:

```php
use Fagus\Fagus;

Fagus::html(file_get_contents('example.html'))->save('example.pdf');
```

## Set the page size and margins

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    size: A4;
    margin: 20mm 18mm 25mm;
  }
  @page :first {
    margin-top: 45mm;
  }
</style>
</head>
<body>
  <h1>Invoice</h1>
</body>
</html>
```

`size` takes:

- a name: `A5`, `A4`, `A3`, `B5`, `B4`, `JIS-B5`, `JIS-B4`, `letter`, `legal`, `ledger`, optionally
  with `portrait` or `landscape` (`size: A4 landscape`);
- one length for a square page, or two for width and height (`size: 80mm 200mm` for a receipt);
  `calc()`, `min()`, `max()`, `clamp()`, `em` and viewport units work;
- `auto`, the default page.

Fagus parses `size` with Chrome's grammar: a value Chrome does not accept is ignored, and the page
keeps the size that applied before, with a `css.invalid-value` warning that says why. `A6` is one of
them (write `size: 105mm 148mm`, which the warning suggests), as are three lengths and `80mm auto`:
a page's height cannot follow its content in CSS, so give both lengths. For a receipt that is one
page as long as its content, add `-fagus-page-height: fit-content` (see
[Make the page as tall as its content](#make-the-page-as-tall-as-its-content)).

**Defaults.** A page without `size` is A4. Chrome takes the paper size of the printer or of the
system's region (Letter in the United States). A server has neither, so Fagus uses A4, so that a
document prints the same on every machine. Write `size: letter` for US documents. A page without
`margin` has Chrome's default margins of 28pt (about 1cm) on every side. The `body` element keeps
its default margin of 8px inside the page area, as in a browser; most print stylesheets set
`body { margin: 0 }`.

**Details:**

- Percentage margins refer to the page width for left and right and to the page height for top
  and bottom.
- `em` and `rem` in `@page` refer to the root element's font size.
- As in Chrome, each side of the PDF page is rounded to whole points and then to a 1/300 inch grid:
  A4 is 594.96 × 841.92pt.
- `width`, `height`, their minimums and maximums, `padding` and `border` of `@page` size the page
  box as in Chrome; `padding` and `border` lie inside the page margins.
- `@page` and its margin boxes take only the properties Chrome allows there. `transform`, `opacity`,
  `font-variant-*` and `font-feature-settings` are ignored.
- Content that reaches outside the page area is cut off at the page margins. Content wider than the
  page area makes Fagus lay the page out again on a wider area and scale it down, as Chrome's print
  does, by at most 1.5 times.

### Make the page as tall as its content

A receipt printer cuts its roll where the document ends. With `-fagus-page-height: fit-content`, a
Fagus [extension](extensions.md#pages-that-grow-with-their-content), the page grows with its
content: one page as tall as the content and the margins, at least as tall as `size` says.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    size: 80mm 40mm;
    margin: 4mm;
    -fagus-page-height: fit-content;
    @bottom-center { content: "Thank you for your visit"; font-size: 7pt; }
  }
  body { margin: 0; font: 9pt/1.3 sans-serif; }
  .total { font-weight: bold; text-align: right; }
</style>
</head>
<body>
  <h1>Café Fagus</h1>
  <p>2 × Cappuccino <span style="float: right">7.80 €</span></p>
  <p>1 × Apple cake <span style="float: right">4.20 €</span></p>
  <p class="total">Total 12.00 €</p>
</body>
</html>
```

The footer sits at the end of the grown page, and `counter(pages)` is 1. A page grows to at most
14,400pt (about 5 metres), the largest page a PDF holds; forced breaks still start new pages, each
as tall as what it holds. Browsers print the page at the size of `size`.

## Use different first, left and right pages

`:first` matches the first page of the document. `:left` and `:right` match the pages of a
two-sided document: a left-to-right document starts on a right page, a right-to-left one on a left
page, as in Chrome.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { size: A5; margin: 20mm; }
  @page :left  { margin-left: 25mm; margin-right: 15mm; }
  @page :right { margin-left: 15mm; margin-right: 25mm; }
  @page :left  { @bottom-left  { content: counter(page); } }
  @page :right { @bottom-right { content: counter(page); } }
  section { break-before: page; }
</style>
</head>
<body>
  <section>Page one is a right page.</section>
  <section>Page two is a left page.</section>
  <section>Page three is a right page.</section>
</body>
</html>
```

The more specific rule wins: a named page beats `:first` and `:blank`, which beat `:left` and
`:right`. `:first` is only ever the first page of the document, also in `@page chapter:first`; for
the first page of each chapter, use `:nth()` (see
[Style the first page of every section](#style-the-first-page-of-every-section)).

### Start chapters on a right page

`break-before: right` (or `left`, `recto`, `verso`, and the same values of `break-after`) starts a
new page on that side. Where the next page would be on the other side, Fagus inserts a blank page
first, and `@page :blank` matches it:

```css
.chapter { break-before: right; }
@page :blank {
  @top-center { content: none; }
  @bottom-center { content: "This page is intentionally left blank."; }
}
```

- `recto` is a right page and `verso` a left page in a left-to-right document, the other way round
  in a right-to-left one.
- A blank page has no content, but it counts: `counter(page)` and `counter(pages)` include it, and
  its margin boxes show what the `@page` rules for it say. It has the page name of the page after
  it.
- Where the content starts with such a break (`break-before: left` on the first element), the first
  page is on that side, and the pages after alternate from there.

This follows CSS Fragmentation and CSS Paged Media, as WeasyPrint does. Chrome treats these values
like `page` and inserts no blank page, and it ignores `@page :blank` rules, so a template that
relies on them prints differently in Chrome.

### Select pages by number

`:nth(An+B)` matches the pages of the document whose number (counting from 1, blank pages
included) is `An+B` for some `n` of 0 or more, as in `:nth-child()`: `:nth(3)` is the third page,
`:nth(2n)` every even page, `:nth(-n+2)` the first two.

```css
@page :nth(2) { margin-top: 30mm; }
@page :nth(odd) { @bottom-right { content: counter(page); } }
```

It counts sheets, not the `page` counter, which `counter-reset` and `counter-set` may restart.

## Give sections pages of their own

The `page` property puts an element on pages with a name, which `@page name` rules style. Where the
page name changes, a new page starts.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { size: A4; margin: 20mm; }
  @page wide { size: A4 landscape; margin: 15mm; }
  .wide { page: wide; }
</style>
</head>
<body>
  <p>Portrait text.</p>
  <section class="wide">
    <table><!-- a wide table --></table>
  </section>
  <p>Portrait again.</p>
</body>
</html>
```

Named pages work with `:first`, `:left` and `:right` (`@page wide:left`). Page names keep their
case, so `page: WordSection1` matches `@page WordSection1`, as in documents exported from Word.

To print a landscape page turned onto a portrait sheet, so that all sheets of the PDF have the same
orientation, add `page-orientation: rotate-left` (or `rotate-right`) to the named page. Links and
bookmarks on the page turn with it.

```css
@page wide {
  size: A4 landscape;
  page-orientation: rotate-left;
}
```

## Style the first page of every section

A page group is the run of pages of one element whose `page` is not `auto` and that has a forced
break before it (`break-before: page`, `left`, `right`, `recto` or `verso`). `:nth(An+B of name)`
counts the pages of each group of that page name, so `:nth(1 of chapter)` is the first page of
every chapter:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { margin: 20mm; @top-right { content: "Annual report"; } }
  @page :nth(1 of chapter) {
    margin-top: 60mm;
    @top-right { content: none; }
  }
  .chapter { page: chapter; break-before: page; }
</style>
</head>
<body>
  <section class="chapter"><h1>Revenue</h1><!-- several pages --></section>
  <section class="chapter"><h1>Outlook</h1><!-- several pages --></section>
</body>
</html>
```

- Each element starts a group of its own, also where the element before it has the same page
  name. Without a forced break before it, an element makes no group: two of them in a row share
  their pages.
- A group holds every page of its element, also pages of other names inside it: in a chapter with
  a landscape table on a page `wide` of its own, `:nth(3 of chapter)` is the third page of the
  chapter even where that is a `wide` page. A page can be in several groups, of nested elements.
- A blank page that a break to a left or right page inserts before the element is not in its
  group; one inside the element is.
- On a selector with a page name, `:nth()` without `of` counts the pages of that name's group
  where the page is in one, else those of the document: `@page chapter:nth(1)` is the first page
  of every chapter too. WeasyPrint counts the pages of the document there; write
  `:nth(1 of chapter)` for a template that prints the same in both.
- `:nth()` counts as much as `:first` for which rule wins, and the name after `of` as much as a
  page name: `@page :nth(1 of chapter)` beats `@page chapter`. CSS Generated Content for Paged
  Media leaves this open; WeasyPrint ranks them the same way.

This follows CSS Generated Content for Paged Media 3. Chrome does not support `:nth()` and ignores
these rules. An `:nth()` that is not valid (`:nth(x)`, `:nth(1 of)`) drops its rule with a
`css.parse-error` diagnostic.

## Add a header and a footer

The page margins hold 16 margin boxes: `@top-left-corner`, `@top-left`, `@top-center`,
`@top-right`, `@top-right-corner`, `@right-top`, `@right-middle`, `@right-bottom`,
`@bottom-right-corner`, `@bottom-right`, `@bottom-center`, `@bottom-left`, `@bottom-left-corner`,
`@left-bottom`, `@left-middle` and `@left-top`. A margin box shows only when its `content` is not
`none`.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    margin: 25mm 20mm;
    @top-left {
      content: "ACME Inc.";
      font-size: 9pt;
      color: #555;
    }
    @top-right {
      content: "Annual report 2026";
      font-size: 9pt;
      color: #555;
    }
    @bottom-center {
      content: "Page " counter(page) " of " counter(pages);
      font-size: 9pt;
      border-top: 0.5pt solid #999;
    }
  }
  @page :first {
    @top-left { content: none; }
    @top-right { content: none; }
  }
  section { break-before: page; }
</style>
</head>
<body>
  <h1>Annual report 2026</h1>
  <section><h2>Revenue</h2></section>
  <section><h2>Outlook</h2></section>
</body>
</html>
```

Margin boxes are laid out as Chrome lays them out:

- The boxes of an edge share its length by their content, as CSS Paged Media describes; `width`,
  `height` and margins of a margin box are honored.
- Text is aligned as in Chrome's user agent stylesheet: `@top-left` to the left, `@top-center` to
  the center, `@top-right` to the right, and vertically in the middle. `text-align` and
  `vertical-align` change it.
- Margin boxes take padding, borders, backgrounds and images (`content: url(logo.svg)`).
- The font of a margin box does not inherit from `body`: set it in the margin box or on `@page`.
  The font family, size and color of `@page` are inherited by its margin boxes.

**Fonts in margin boxes.** Chrome loads the faces of `@font-face` rules only while it lays out the
document, so a face that the document's content never uses is missing in the margin boxes, and
their text falls back to the next family. Fagus does the same. If your running head is set in an
italic that the body text does not use, either use that face somewhere in the content or load the
font from a font directory (see [Fonts](fonts.md#fonts-in-headers-and-footers)).

## Number the pages

`counter(page)` is the number of the current page, `counter(pages)` the number of pages. Both take
a counter style:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { @bottom-right { content: counter(page); } }
  @page front { @bottom-right { content: counter(page, lower-roman); } }
  .preface { page: front; }
  section { break-before: page; }
</style>
</head>
<body>
  <div class="preface">
    <section>Preface</section>
    <section>Contents</section>
  </div>
  <section>Chapter 1</section>
</body>
</html>
```

This prints i, ii and 3.

`counter-reset`, `counter-set` and `counter-increment` of the `page` counter in `@page` rules work
as in Chrome:

- `counter-increment: page 2` counts the page as two pages, and the pages after go on from there.
  `counter-increment: none` still counts the page as one.
- `counter-set: page 1` sets the counter to 1 on the page, and the pages after go on from there:
  this is the way to start the numbering again.
- `counter-reset: page 1` sets the counter only on the pages it matches: the pages after go on from
  the pages before, as if it had not been there. This is how Chrome does it.
- In a margin box, these properties change the number that box shows.
- On an element of the document, `counter-reset: page` and `counter-increment: page` do not change
  the page numbers.

`counter(pages)` is always the number of pages. `counter(page)` in the content and
`target-counter(..., page)` show the same number as the margin boxes.

Use `counter(pages)` in margin boxes and in running elements (below). In the normal flow of the
document and in `position: fixed` boxes, the number of pages is not known while the content is laid
out, and `counter(pages)` comes out as 0 or empty. `counter(page)` works everywhere, also in
`::before` and `::after` of content and in repeated table headers.

## Put HTML in the header and footer

A margin box can show an element of the document: `position: running(name)` takes the element out
of the flow, and `content: element(name)` places it in a margin box on every page. The element keeps
its own styles, so it can be a flex or grid container, hold images and links, and show page numbers.

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
    align-items: end;
    width: 100%;
    border-bottom: 1pt solid #1e3a8a;
  }
  .footer {
    position: running(footer);
    display: grid;
    grid-template-columns: 2fr 2fr 1fr;
    gap: 6mm;
    width: 100%;
    font-size: 7.5pt;
    color: #555;
  }
  .footer .page { text-align: right; }
  .footer .page::after { content: "Page " counter(page) " of " counter(pages); }
  section { break-before: page; }
</style>
</head>
<body>
  <header class="letterhead">
    <strong>ACME Inc.</strong>
    <span>1 Main Street, Springfield</span>
  </header>
  <footer class="footer">
    <div>ACME Inc. · 1 Main Street · Springfield</div>
    <div>IBAN DE00 0000 0000 0000 0000 00</div>
    <div class="page"></div>
  </footer>

  <h1>Letter</h1>
  <section>Second page</section>
</body>
</html>
```

Place running elements at the start of the document, before the content of the first page: a
running element shows on the page where it appears and on the pages after it. Several elements with
the same running name can follow each other; `element(name, first)` (the default), `start`, `last`
and `first-except` choose which one a page shows, as for `string()` below.

A running element is laid out again on every page, with that page's number: a column sized by its
content (`auto`) is as wide as "Page 10 of 12" on page 10. Chrome does not support running elements;
it shows the element where it is in the document.

`element()` stands alone in `content`: CSS Generated Content for Paged Media does not allow it
together with other values. `content: element(logo) element(address)` and
`content: element(title) " – " counter(page)` are invalid, so the declaration is ignored with a
`css.invalid-value` diagnostic and the margin box keeps the `content` that applied before (with
`var()`, it shows nothing). Put everything the box shows into one running element, with the page
number in its generated content as in the footer above, or use the neighbouring margin boxes.
WeasyPrint shows such combinations; Fagus follows the specification.

## Repeat a chapter title in the header

Named strings copy text from the document into the margin boxes. `string-set` on an element sets
the string, `string()` shows it.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    @top-left { content: string(chapter); }
    @top-right { content: string(section, last); }
  }
  @page :first {
    @top-left { content: none; }
  }
  h2 { string-set: chapter content(); break-before: page; }
  h3 { string-set: section content(); }
</style>
</head>
<body>
  <h1>Handbook</h1>
  <h2>Receiving</h2>
  <h3>Checking deliveries</h3>
  <h3>Storing goods</h3>
  <h2>Shipping</h2>
  <h3>Packing</h3>
</body>
</html>
```

`string-set` takes strings, `content()` (the element's text; also `content(before)`,
`content(after)`, `content(first-letter)` and `content(marker)`), `attr()` and counters.
`counter(page)` and `counter(pages)` in it are the number of the page the element is on and the page
count: `string-set: chapter content() ", page " counter(page)` keeps the page where the chapter
starts on the pages after it. An element with `display: none` sets its strings too, where its box
would be, so a hidden element can name the chapter without printing. `string()` takes a second
argument for the case that the string is set several times on a page:

| Value | Shows |
|---|---|
| `first` (default) | the first value set on the page, else the value from the pages before |
| `start` | the value at the top of the page |
| `last` | the last value set on the page |
| `first-except` | nothing on the page where the value is set, the value on the pages after |

`string()` also works in the generated content of the document, such as a table footer that shows a
subtotal per page (see [Extensions](extensions.md)).

## Control page breaks

```html
<!doctype html>
<html lang="en">
<head>
<style>
  h1, h2, h3 { break-after: avoid; }
  figure, blockquote, .address { break-inside: avoid; }
  p { orphans: 3; widows: 3; }
  .chapter { break-before: page; }
</style>
</head>
<body>
  <h1>Offer</h1>
  <div class="address">ACME Inc.<br>1 Main Street<br>Springfield</div>
  <section class="chapter"><h2>Scope</h2><p>…</p></section>
  <section class="chapter"><h2>Terms and conditions</h2><p>…</p></section>
</body>
</html>
```

- `break-before` and `break-after`: `page` forces a break; `left`, `right`, `recto` and `verso`
  force one to a page of that side, after a blank page where needed (see
  [Start chapters on a right page](#start-chapters-on-a-right-page)); `avoid` and `avoid-page`
  avoid one; `column` and `avoid-column` do the same in columns.
- `break-inside: avoid` keeps a box on one page if it fits. A box taller than a page breaks anyway.
- `orphans` and `widows` (2 by default) set how many lines of a paragraph stay at the bottom and the
  top of a page.
- The older `page-break-before`, `page-break-after` and `page-break-inside` work as aliases, and an
  empty `<div style="page-break-after: always"></div>` breaks the page, as in browsers.

**Keep headings with their text.** Headings do not stay with what follows them by themselves.
`h1, h2, h3 { break-after: avoid; }` moves a heading that would end a page to the next page together
with the start of its paragraph. A `break-before` of a first child and a `break-after` of a last
child pass on to the parent, since breaks fall between siblings. Where a heading that avoids the break after it
starts a page and the box after it avoids breaks inside but does not fit below it, the box breaks
inside, below the heading, as in Chrome: either break goes against an `avoid`, and the later one is
taken.

Fagus breaks pages where Chrome breaks them, including inside table rows, flex and grid items,
floats and positioned boxes, so a template that paginates well in Chrome's print preview paginates
the same way in Fagus.

## Repeat table headers on every page

`<thead>` and `<tfoot>` repeat on every page a table breaks across. `tr { break-inside: avoid; }`
keeps rows whole.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { size: A5; margin: 15mm; }
  table { width: 100%; border-collapse: collapse; }
  th, td { padding: 1mm 2mm; border-bottom: 0.5pt solid #ccc; text-align: left; }
  .num { text-align: right; }
  tr { break-inside: avoid; }
</style>
</head>
<body>
  <table>
    <thead>
      <tr><th>#</th><th>Item</th><th class="num">Amount</th></tr>
    </thead>
    <tfoot>
      <tr><td colspan="3">Prices in EUR</td></tr>
    </tfoot>
    <tbody>
      <tr><td>1</td><td>Item 1</td><td class="num">7.00</td></tr>
      <!-- … many more rows … -->
    </tbody>
  </table>
</body>
</html>
```

As in Chrome:

- A header or footer group repeats only if it avoids breaks inside, which `<thead>` and `<tfoot>`
  do by default. A `display: table-header-group` of your own needs `break-inside: avoid` to repeat.
- Headers and footers taller than a quarter of the page do not repeat.
- A row that does not fit breaks inside its cells unless it has `break-inside: avoid`; the rest of
  its cells continue below the repeated header.
- `counter-increment` and `string-set` work on rows, so `tbody tr { counter-increment: position; }`
  numbers the positions of an invoice.

To show the running total at the bottom of each page and bring it forward on the next (*Übertrag*),
use the carry-over rows of [Extensions](extensions.md).

## Break flex, grid and multi-column layouts across pages

Flex containers, grids and multi-column containers break across pages like other boxes, as in
Chrome:

- Flex rows break between their lines, column flex containers inside their items.
- Grids break between rows and inside items; items in the same row continue together.
- Multi-column containers fill their columns, continue on the next page when the columns are full,
  and balance the last page. `column-span: all` elements span the columns.
- `break-before`, `break-after` and `break-inside` work on flex and grid items.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { size: A5; margin: 15mm; }
  .cards { display: grid; grid-template-columns: repeat(2, 1fr); gap: 5mm; }
  .card { border: 0.5pt solid #999; padding: 4mm; height: 40mm; }
</style>
</head>
<body>
  <div class="cards">
    <div class="card">1</div><div class="card">2</div>
    <div class="card">3</div><div class="card">4</div>
    <div class="card">5</div><div class="card">6</div>
    <div class="card">7</div><div class="card">8</div>
    <div class="card">9</div><div class="card">10</div>
  </div>
</body>
</html>
```

The first page holds the cards 1 to 8, the second page 9 and 10.

## Build a table of contents

`target-counter()` shows a counter at the place a link points to, `leader()` fills the rest of the
line. Together they make a table of contents with page numbers:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  .toc a { display: block; color: inherit; text-decoration: none; }
  .toc a::after { content: leader(dotted) target-counter(attr(href), page); }
  .xref::after { content: " (page " target-counter(attr(href), page) ")"; }
  h2 { break-before: page; }
</style>
</head>
<body>
  <nav class="toc">
    <h1>Contents</h1>
    <a href="#installation">Installation</a>
    <a href="#configuration">Configuration</a>
    <a href="#upgrading">Upgrading</a>
  </nav>
  <h2 id="installation">Installation</h2>
  <p>Read <a class="xref" href="#upgrading">Upgrading</a> before you update.</p>
  <h2 id="configuration">Configuration</h2>
  <h2 id="upgrading">Upgrading</h2>
</body>
</html>
```

```text
Contents
Installation ............................................ 2
Configuration ........................................... 3
Upgrading ............................................... 4
```

- `target-counter(attr(href), page)` is the page the target starts on. Other counters give their
  value at the target, such as `target-counter(attr(href), chapter)` or `list-item`.
  `target-counters()` gives all levels of a nested counter with a separator.
- `target-text(attr(href))` is the target's text; `before`, `after` and `first-letter` select its
  generated content or first letter.
- `leader()` takes `dotted`, `solid`, `space` or a string.

A document that uses these functions is laid out twice: once to find the targets and once with
their numbers. Chrome supports neither function, so a table of contents built this way shows no
page numbers in Chrome's print preview.

The links of the table of contents are links in the PDF too. Headings also make the PDF's outline
(bookmarks): `bookmark-level`, `bookmark-label` and `bookmark-state` change it, and
`bookmark-level: none` leaves a heading out. See [PDF output](pdf.md).

## Fill the whole page

A page with `margin: 0` lets content reach the edge of the sheet. The background of `@page` covers
the whole sheet, margins included.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page { margin: 20mm; }
  @page cover { margin: 0; background: #1d3557; }
  body { margin: 0; }
  .cover {
    page: cover;
    height: 297mm;
    box-sizing: border-box;
    padding: 60mm 20mm;
    color: white;
  }
</style>
</head>
<body>
  <div class="cover"><h1>Product catalog 2027</h1></div>
  <h2>Introduction</h2>
</body>
</html>
```

- The background of the root element, or of `body` if the root has none, covers the canvas of every
  page (the page area and the page box's padding), as in browsers.
- The background of `@page` takes colors and images, but no gradients, as in Chrome 157. Put a
  gradient on an element that fills the page instead.
- A `position: fixed` element repeats on every page, placed relative to the page area, which makes
  watermarks easy. Marks in the page margins, such as fold marks, belong in a margin box
  (`content: url(fold-mark.svg)`), since content outside the page area is cut off, as in Chrome and
  as CSS Positioned Layout specifies.
- In a tagged PDF, what margin boxes show is an artifact that screen readers skip (see
  [PDF output](pdf.md#tagged-pdf)). Keep content that a reader needs in the page area: the address
  field of a letter, 45mm from the top edge of the sheet, is
  `position: absolute; top: calc(45mm - 15mm)` on a first page with a top margin of 15mm, and
  stays in the document's structure where it is in the HTML.

## Use a PDF as page template

A letterhead designed in another program can stay a PDF: Fagus shows a PDF file as an image, as
Safari and Prince do, so the page background takes it like any other image. The page is drawn as
vector graphics, with its text, fonts and images as they are in the file, and stays sharp and
selectable.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    size: A4;
    margin: 28mm 20mm 32mm 25mm;
    background: url(letterhead.pdf#page=2) -25mm -28mm no-repeat;
  }
  @page :first {
    margin-top: 45mm;
    background-image: url(letterhead.pdf);
    background-position: -25mm -45mm;
  }
</style>
</head>
<body>
  <p>Dear Ms. Lindner,</p>
</body>
</html>
```

- `#page=2` at the end of the URL chooses the page of the file (RFC 8118); without it, the first
  page is shown. A page the file does not have reports `image.broken`.
- The image is as large as the page's crop box, its `/Rotate` applied: an A4 page is as large as
  an A4 sheet (a point is 4/3 CSS pixels).
- The background of `@page` covers the whole sheet, but its images are placed in the page box,
  inside the margins, as in Chrome. A position of minus the left and top margins puts the
  template at the corner of the sheet; with `margin: 0`, `no-repeat` alone is enough. A template
  of another size than the sheet is stretched to it with `background-size`, for example
  `background: url(letterhead.pdf) -25mm -28mm / 210mm 297mm no-repeat`.
- The margins keep the text off the letterhead's header and footer; margin boxes (page numbers)
  are drawn on top of the template.
- Links and other annotations of the template's page are not taken over, nor its form fields or
  tags. In a tagged PDF the template is an artifact, as every background is.
- PDF/A and PDF/UA do not allow it: Fagus cannot check what the template contains (fonts that are
  not embedded, transparency), and refuses the document (a `RenderException` that says so, see
  [PDF output](pdf.md#make-an-accessible-pdf-pdfua-1)). Convert the letterhead to SVG
  (`pdftocairo -svg letterhead.pdf letterhead.svg`) and use that instead.
- A template that needs a password to open is not shown (`image.unsupported`); one its owner only
  restricts opens without one, as in viewers. PDF 2.0 files are not shown, as Fagus writes PDF 1.7.

To go through the pages of the template in turn, as mPDF's `SetDocTemplate($file, true)` does,
select the pages by number:

```css
@page { margin: 25mm; background: url(letterhead.pdf) -25mm -25mm no-repeat; }
@page :nth(2n) { background-image: url(letterhead.pdf#page=2); }
/* A template of three pages: */
@page :nth(3n+2) { background-image: url(letterhead.pdf#page=2); }
@page :nth(3n) { background-image: url(letterhead.pdf#page=3); }
```

A template can also go behind some pages only, with a named page (`page: letter`) or `:first`.
Chrome shows no PDF images; it prints such pages without the template.

## Prepare a document for a print shop

`bleed` adds room around the page for content that runs off the edge, and `marks` draws crop marks
and registration crosses outside it. Chrome ignores both.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    size: A5;
    margin: 15mm;
    bleed: 3mm;
    marks: crop cross;
  }
  html { background: #e8f0ff; }
</style>
</head>
<body><h1>Flyer</h1></body>
</html>
```

The PDF page grows by the bleed and the room for the marks. Its TrimBox is the A5 page, its
BleedBox the page with the bleed, so a print shop's software knows where to cut. Without a `bleed`
of its own, a page with `marks` gets a bleed of 6pt, as CSS Paged Media specifies.

## Footnotes

`float: footnote` takes an element out of the text and puts it at the bottom of the page, in the
footnote area. A call, a number in superscript, stays where the element was, and the footnote
starts with the same number. This follows CSS Generated Content for Paged Media 3, as WeasyPrint
and Prince print footnotes; Chrome has no footnotes and leaves the element in the text.

```html
<!doctype html>
<html lang="en">
<head>
<style>
  @page {
    size: A5;
    margin: 20mm;
    @footnote {
      border-top: 0.5pt solid #999;
      padding-top: 2mm;
    }
  }
  .note {
    float: footnote;
    font-size: 8pt;
  }
</style>
</head>
<body>
  <p>
    Payment is due within 14 days<span class="note">From the date of the invoice.</span>
    at a flat rate<span class="note">Up to 30 kg; heavier parcels are charged by weight.</span>.
  </p>
</body>
</html>
```

The footnote area grows from the bottom of the page area up, and the text of the page gets the
room above it. A page whose calls do not leave room for their footnotes ends earlier, and a
footnote that still does not fit breaks: it starts below the line of its call and goes on at the
top of the footnote area of the next page.

**Links.** The call is a link to its footnote and the marker a link back to the call (WeasyPrint
links neither).

**Calls and markers.** `::footnote-call` is the call in the text, `::footnote-marker` the number at
the start of the footnote. Both take styles and `content`; by default they are:

```css
::footnote-call {
  content: counter(footnote);
  vertical-align: super;
  font-size: smaller;
  line-height: inherit;
}
::footnote-marker { content: counter(footnote) ". "; }
```

`::footnote-call { content: none }` leaves the call out. The pseudo-elements inherit from the
footnote's element, so `font-size: 8pt` on the footnote makes its call smaller too.

**Numbering.** The `footnote` counter counts the footnotes through the document; each footnote
counts itself unless its own `counter-increment` names the counter. To number the footnotes of each
page from 1, reset the counter in `@page`:

```css
@page { counter-reset: footnote; }
```

`counter-reset: footnote` on an element (a chapter) starts the count again there, and
`counter(footnote, lower-roman)` writes the numbers in another style. WeasyPrint numbers through the
document whatever `@page` says; Prince numbers from each page, as Fagus does.

**The footnote area.** `@footnote` in an `@page` rule styles the area: margins, borders, padding
and a background. It has a top margin of `1em` by default, as in WeasyPrint. `max-height` limits
it; the footnotes that do not fit below it go on the next page. A page without footnotes has no
area.

**When a footnote does not fit** on the page of its call, `footnote-policy` on the footnote says
what happens:

| `footnote-policy` | What happens |
|---|---|
| `auto` (default) | The footnote starts below the line of its call and goes on on the next page, where at least `orphans` lines of it fit; else it goes on the next page whole. The call stays. |
| `line` | The line of the call goes to the next page with the footnote. |
| `block` | The paragraph of the call goes to the next page with the footnote. |

With `line` and `block`, a call on the first line of a page stays and its footnote breaks as with
`auto`, as no page would hold both; but where footnotes moved from the page before take the room,
the line or paragraph goes to the next page, which leaves a page of footnotes. A paragraph that
started on the page before cannot go to the next page whole any more: `block` then takes the line
of the call, as `line` does. In multi-column layouts, a page does not end where the line of a call
in a column does: there the footnote goes to the next page as with `auto`, whole. A footnote breaks
between its lines as a paragraph does, with `orphans` and `widows`. Once the content of the
document ends, the footnotes still waiting get pages of their own.

With `auto`, the text after the call stays on the page where it fits. WeasyPrint takes the next
line to the next page as well, because the area it keeps for the moved footnote still takes the
room of its top margin; that is not something `auto` asks for.

**Footnotes side by side.** `footnote-display: inline` puts footnotes on the lines of the footnotes
beside them, `compact` does so with those that fit on one line, and `block` (default) gives each
footnote lines of its own. Footnotes on shared lines break between those lines too.

**Footnotes in columns.** In a multi-column layout, footnotes go into the footnote area of the
page, which spans the page area. With `float-reference: column` they go to the bottom of the
column their call is in instead, as Prince prints them (Prince does so by default, Fagus keeps the
initial value of CSS):

```css
.text { columns: 2; }
.note { float: footnote; float-reference: column; }
```

Each column with calls has a footnote area of its own at its bottom, as wide as the column and
styled by `@footnote` as the page's; the column's content takes the room above it. Balanced
columns are balanced with their footnotes in them, so the columns end at the same height with
their areas at the bottom; columns filled in turn (`column-fill: auto`) end where their footnotes
start. A footnote that does not fit below the content up to its call takes the line of its call
to the next column, whatever its `footnote-policy`, and a footnote in a column does not break:
a column's footnotes are short by design, and one split across columns would be read in two
places side by side.
Where a footnote does not fit even below the first line of a column, it goes into the footnote
area of the page. The footnotes number on through the document, those of the columns and of the
page alike.

**Tagged PDF.** Each footnote is a `Note` in the structure of the document, after the text with its
call and with its marker as a label, as PDF/UA asks (see [PDF output](pdf.md#tagged-pdf)). The
call is a `Link` with its annotation, and so is the marker in the label.

**Details:**

- `float` does not apply to absolutely positioned boxes and to flex and grid items: there,
  `float: footnote` makes no footnote, as other `float` values make no float.
- A call in a table header that repeats on every page calls its footnote once, on the first page.
- `float-reference` (CSS Page Floats 3) takes `inline` (the initial value), `region` and `page`:
  with all three a footnote goes into the footnote area of its page, as Fagus has no regions and
  `inline` refers to the line of the call, which is on that page. With `column`, a footnote called
  in a column of a multi-column layout goes to the bottom of that column, as Prince puts it (see
  *Footnotes in columns* above); called outside columns, it goes into the page's
  area.
- Footnotes cost a few extra layouts of each page that holds calls: a page is laid out, the calls
  on it are found and it is laid out again with room for their footnotes. The result is the same
  every time.

Endnotes work with links and `target-counter()`. The note references show the numbers of the list
items they point to:

```html
<!doctype html>
<html lang="en">
<head>
<style>
  a.note { text-decoration: none; }
  a.note::after {
    content: target-counter(attr(href), list-item);
    vertical-align: super;
    font-size: 70%;
  }
</style>
</head>
<body>
  <p>Delivery within 14 days<a class="note" href="#n1"></a> at a flat rate<a class="note" href="#n2"></a>.</p>
  <h2>Notes</h2>
  <ol>
    <li id="n1">Within Germany.</li>
    <li id="n2">Up to 30 kg.</li>
  </ol>
</body>
</html>
```

## Differences from Chrome

| Topic | Chrome | Fagus |
|---|---|---|
| Default page size | the printer's or region's (Letter in the US) | A4 |
| `bleed`, `marks` | ignored | supported |
| `target-counter()`, `target-text()`, `leader()` | not supported | supported |
| `break-before: left`, `right`, `recto`, `verso` | like `page` | a blank page where needed |
| `@page :blank` | rule ignored | matches the inserted blank pages |
| `@page :nth()`, page groups | rule ignored | supported |
| Running elements (`position: running()`) | not supported | supported |
| Footnotes (`float: footnote`, `@footnote`) | not supported, the element stays in the text | supported |
| Fonts | installed fonts of the machine | only the fonts you configure, unless system fonts are on (see [Fonts](fonts.md)) |
| Texts of form controls | the language of Chrome's user interface | the control's `lang`, else the engine's `locale` |

Everything else on this page, including the parts that are not supported, behaves as in Chrome's
print.
