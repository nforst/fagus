# CSS reference

Fagus lays out HTML and CSS the way Chrome prints it (Chrome 157 on Linux is the reference). This
page lists what Fagus supports, grouped by topic, with the details that matter in print and the
places where Fagus differs from Chrome or has no support. Page size, margins, headers, footers and
page breaks have a page of their own: [Paged media](paged-media.md). Fonts are described in
[Fonts](fonts.md).

A rule of thumb: if Chrome's print preview shows it, Fagus renders it the same way. Where this
page says "as in Chrome", Fagus follows Chrome's behavior in print, which is not always what the
specification says.

## Find out what was ignored

A declaration that Fagus cannot use is dropped and reported as a diagnostic with its position in
the stylesheet, like the console of a browser. Declarations in `style` attributes are reported too,
with the line of the element in the HTML, once for each different attribute:

```php
use Fagus\Fagus;

$pdf = Fagus::html($html)->render();

foreach ($pdf->diagnostics() as $diagnostic) {
    echo $diagnostic, "\n";
    // <style> 1:12:3: warning[css.unknown-property]: unknown property `paint-order`; the declaration is ignored
    // html:40: warning[css.unknown-property]: `style` attribute of <p>: unknown property `colr`; the declaration is ignored
}
```

`->strict()` turns these warnings into a `RenderException`, which is useful in tests. The codes you
will see for CSS:

| Code | Meaning |
|---|---|
| `css.unknown-property` | A property Fagus does not know. |
| `css.invalid-value` | A known property with a value Fagus does not accept (as Chrome would not either). |
| `css.parse-error` | A rule or at-rule that does not parse, such as an unknown at-rule. |
| `css.unparsable` | A value that could not be read safely and was left out. |
| `css.too-deep` | A stylesheet nested more than 256 levels deep. |
| `resource.not-loaded` | A stylesheet, image or font that could not be loaded. |

Properties that have no effect in print are dropped without a diagnostic: `transition-*`,
`animation-*` (see [Animations](#animations-and-transitions)), `cursor`, `pointer-events`,
`user-select`, `touch-action`, `caret-color`, `scroll-*`, `overscroll-*`,
`view-transition-*`, `text-size-adjust`, Microsoft Office's `mso-*` properties,
and vendor-prefixed properties (`-webkit-`, `-moz-`, `-ms-`, `-o-`) other than the ones listed on
this page.

## Stylesheets

- `<style>` elements, `style` attributes and `<link rel="stylesheet">`.
- `@import`, also with media queries, `supports()` conditions and `layer()`, up to eight levels
  deep. A stylesheet that cannot be loaded is reported.
- Relative URLs in a linked or imported stylesheet resolve against that stylesheet, as in browsers.
- Stylesheets passed with `->stylesheet($css)` in PHP apply after the document's own styles.

Stylesheets, fonts and images come only from the places a document may read: `data:` URLs,
resources handed over with `->resource()`, the `->resolver()` callback, and files inside the base
directory or root. Fagus never loads anything from the network (see [Resources](resources.md)).

### HTML and its presentational attributes

Documents are parsed as HTML5, as browsers parse them. A document without a doctype is rendered in
quirks mode, with the quirks Chrome applies (line height quirk, table and form rules, full-height
`html` and `body`, hashless colors and unitless lengths where Chrome takes them). XHTML is parsed as
XML with `Fagus::xhtml()` (or a `.xhtml` file on the command line).

The presentational attributes of older templates work as in Chrome and lose against any CSS:
`width`, `height`, `align`, `valign`, `bgcolor` and `nowrap` on tables and their parts;
`cellspacing`, `cellpadding`, `border`, `bordercolor`, `frame` and `rules` on tables; `color`,
`face` and `size` on `<font>`; `size`, `width`, `color` and `noshade` on `<hr>`; `align`, `border`,
`hspace` and `vspace` on images; `align` on blocks and `<center>`; `type` on lists; `bgcolor`,
`text` and the margin attributes of `<body>`; `<br clear>`. Templates written for dompdf or mPDF
with table layouts render as in Chrome.

## Selectors and the cascade

**Works:**

- All selectors of CSS Selectors 4 that make sense in a static document: type, class, id,
  universal, attribute selectors with all operators and the `i`/`s` flags, all combinators,
  `:is()`, `:where()`, `:not()`, `:has()` (also with sibling and child relations),
  `:nth-child(… of S)`, `:nth-last-child()`, `:nth-of-type()`, `:first-child`, `:last-child`,
  `:only-child`, `:empty`, `:root`, `:scope`, `:lang()`, `:dir()`, `:link`, `:any-link`, `:defined`.
- Form states: `:checked`, `:default`, `:disabled`, `:enabled`, `:required`, `:optional`,
  `:read-only`, `:read-write`, `:placeholder-shown`, `:indeterminate`, `:open`, `:closed`, and
  `:valid`, `:invalid`, `:in-range`, `:out-of-range` with real constraint validation (including
  `pattern`).
- Pseudo-elements: `::before`, `::after`, `::marker`, `::first-letter`, `::first-line`,
  `::placeholder`, the `::-webkit-` parts of progress bars, meters and range sliders, and
  `::scroll-marker`, `::scroll-marker-group` and `::scroll-button()` as far as Chrome prints them,
  and `::footnote-call` and `::footnote-marker` of footnotes.
- CSS nesting (`&`, nested rules and nested at-rules).
- The cascade with origins, `!important`, specificity, order, inline styles, cascade layers
  (`@layer`, also anonymous and nested, with `revert-layer`) and `@scope` (with `:scope`, limits,
  proximity, and the implicit scope of a `<style>` element).
- CSS-wide keywords `initial`, `inherit`, `unset`, `revert`, `revert-layer`, and `all`.

**Differs or not supported:**

- User-action and time states never match: `:hover`, `:focus`, `:focus-visible`, `:active`,
  `:visited`, `:target` (except in SVG fragments), `:playing` and the like.
- Shadow DOM selectors (`:host`, `::slotted()`, `::part()`) match nothing; there is no shadow DOM
  without scripts.

## Values and units

**Works:**

- Custom properties and `var()` with fallbacks; `@property` with syntax checking of registered
  properties, `inherits` and `initial-value`.
- Math functions: `calc()`, `min()`, `max()`, `clamp()`, `round()` (with `nearest`, `up`, `down`,
  `to-zero`), `mod()`, `rem()`, `abs()`, `sign()`, `hypot()`, `pow()`, `sqrt()`, `exp()`, `log()`,
  trigonometric functions, division of typed values, and `progress()`. A calculation that comes to
  NaN is 0.
- `calc-size()` for widths and heights, and the sizing keywords `min-content`, `max-content`,
  `fit-content`, `stretch` (also `-webkit-fill-available`).
- Newer functions computed per element: `if()` with `media()`, `supports()` and `style()`; typed
  `attr()`; `sibling-index()`, `sibling-count()`; `env()`.
- Absolute units: `px`, `pt`, `pc`, `in`, `cm`, `mm`, `Q`. One CSS pixel is 1/96 inch.
- Font-relative units: `em`, `rem`, `ex`, `rex`, `ch`, `rch`, `cap`, `rcap`, `ic`, `ric`, `lh`,
  `rlh`, measured in the first available font of the element's language, as in Chrome. `ex` and
  `cap` are the x-height and cap height of the font's OS/2 table; a TrueType font without them
  (such as DejaVu) takes the top of its "x" and "H" in whole pixels from 8px on, where FreeType's
  hinting puts them for Chrome on Linux (8px for DejaVu Sans Mono at 14px, not 7.66px).
- Viewport units (`vw`, `vh`, `vmin`, `vmax`, `vi`, `vb` and the `s`, `l`, `d` variants) refer to
  the page area of the first page.
- Container query units (`cqw`, `cqh`, `cqi`, `cqb`, `cqmin`, `cqmax`).
- `zoom`, as Chrome implements it, including on the root element.

**Details:** Lengths are cut to Chrome's layout unit of 1/64 pixel and border widths to whole
pixels, so a border of `0.5pt` is drawn 1px wide and `1.5px` is 1px, as in Chrome. A unitless zero
next to lengths in `min()`, `max()` and `clamp()` is invalid, as in Chrome. More than eight nested
math functions make a value invalid.

## Colors

**Works:**

- Named colors, hex notation, `rgb()`, `hsl()`, `hwb()`, `lab()`, `lch()`, `oklab()`, `oklch()` and
  `color()` with the predefined spaces (`srgb`, `display-p3`, `display-p3-linear`, `a98-rgb`,
  `prophoto-rgb`, `rec2020`, `xyz` …).
- `color-mix()`, relative colors (`oklch(from var(--brand) l c h / 50%)`, also with `calc()` on the
  channels), `currentcolor`, `light-dark()`, `contrast-color()`, and the system colors.
- `color-scheme` (including `<meta name="color-scheme">`): a dark root gives a dark page and dark
  form controls, as in Chrome.

**Details:** Colors outside sRGB are clipped to sRGB channel by channel, as Chrome clips them for
print. `prefers-color-scheme` is `light`.

## The box model

**Works:** `margin`, `padding`, `border`, `width`, `height` with their minimums and maximums,
`box-sizing`, `aspect-ratio`, logical properties (`margin-inline`, `inset-block-start`,
`border-start-end-radius` …) in every writing mode, margin collapsing, `margin-trim` for block
containers, `outline` with `outline-offset` (and `outline-style: auto` as Chrome's focus ring),
`visibility` (including `collapse` on table rows and columns), `opacity`.

**Details:** Percentage heights resolve against the containing block where its height does not
depend on the content: the page area for the root element, so a page laid out with
`html, body { height: 100% }` fills the page. `border-style: solid` without a width is `medium`
(3px), as in Chrome.

## Display and layout

### Block and inline

**Works:** block and inline formatting contexts, `inline-block`, `flow-root`, `display: contents`,
`list-item` (also `inline list-item`), every `vertical-align` value, inline boxes with padding,
borders and backgrounds across lines, `box-decoration-break: clone` (and its `-webkit-` alias),
`justify-self`/`justify-items` on block-level boxes, and `display: -webkit-box` with the old
`-webkit-box-*` properties as Blink lays it out.

**Not supported:** `display: run-in` is invalid, as in Chrome.

### Floats

**Works:** `float` (`left`, `right`, `inline-start`, `inline-end`) and `clear` as in CSS 2.1, floats
that break across pages and columns, and `shape-outside` (basic shapes, `path()`, `xywh()`,
`rect()`, `shape()`, box keywords, images and gradients with `shape-image-threshold`, and
`shape-margin`).

**Footnotes:** `float: footnote` with `footnote-display`, `footnote-policy`, `::footnote-call`,
`::footnote-marker` and the `@footnote` area of `@page` (see
[Paged media](paged-media.md#footnotes)). Chrome has no footnotes. `float-reference` takes
`inline`, `region` and `page`, which all put a footnote into the footnote area of its page;
`column` is not supported.

### Flexbox

**Works:** all of CSS Flexbox: direction and wrapping (with `row-reverse`, `column-reverse`,
`wrap-reverse`), `flex-grow`, `flex-shrink`, `flex-basis` (with `content` and keywords), `order`,
`gap`, every alignment property with `safe` and `unsafe`, `baseline` and `last baseline`
alignment, auto margins, the automatic minimum size of items, and `flex-wrap: balance`. Flex
containers break across pages between lines and inside items.

### Grid

**Works:** CSS Grid 1 and 2: explicit and implicit tracks, `fr`, `minmax()`, `fit-content()`,
`repeat()` with `auto-fill` and `auto-fit`, named lines and areas (`grid-template-areas`,
`grid-template`, `grid`), auto-placement with `dense` and `grid-auto-flow: column`, `subgrid`
(with line names), gaps (also the old `grid-gap`), alignment including baselines, and absolutely
positioned items placed by grid lines. Grids break across pages between rows and inside items.

**Limits:** at most 1000 explicit tracks, line numbers and spans are clamped to 1000 (browsers
clamp too), and items are placed in at most 10,000 rows.

**Not supported:** `display: grid-lanes` (masonry) is invalid, as in Chrome 157 without
experimental features.

### Multi-column layout

**Works:** `columns`, `column-count`, `column-width`, `column-gap`, `column-rule`, `column-fill`
(balanced by default), `column-span: all`, `break-before`/`break-after: column`, `column-height`
and `column-wrap` (rows of columns), and nested multi-column containers. Columns continue on the
next page when they are full.

### Tables

**Works:** `table-layout: auto` and `fixed` with Chrome's column width algorithm, `rowspan` and
`colspan`, `<col>` and `<colgroup>` widths and backgrounds, `border-collapse` (collapsed borders
resolved as Chrome resolves them) and `border-spacing`, captions (`caption-side`), `empty-cells`,
`vertical-align` in cells, anonymous table boxes as Chrome generates them, and CSS table displays
on any element.

**In print:** `<thead>` and `<tfoot>` repeat on every page where the table breaks; rows that do not
fit break inside their cells; `counter-increment` and `string-set` on rows work. See
[Paged media](paged-media.md#repeat-table-headers-on-every-page) and the carry-over rows of
[Extensions](extensions.md).

**Differs:** cells with `vertical-align: baseline` sit at the top of their row, as in Chrome's print
of tables that may break across pages.

### Positioning

**Works:** `relative`, `absolute`, `fixed`, `sticky`, `z-index` and stacking contexts, inset
properties, `clip: rect()`, and anchor positioning (`anchor-name`, `position-anchor`, `anchor()`,
`anchor-size()`, `position-area`, `anchor-center`, `anchor-scope`, `@position-try`,
`position-try-fallbacks`, `position-try-order`, `position-visibility`).

**In print:** a `position: fixed` box repeats on every page, as in Chrome. Absolutely positioned
boxes break across pages. Sticky boxes stick in Chrome's print window, 380 pixels tall at the start
of the first page.

### Overflow and containment

**Works:** `overflow` (`visible`, `hidden`, `clip`, `scroll`, `auto`, also per axis),
`overflow-clip-margin`, `scrollbar-width`, `scrollbar-color`, `scrollbar-gutter`, `resize`, `text-overflow` (`ellipsis` and a string), `-webkit-line-clamp` (Tailwind's
`line-clamp-*`), `contain`, `content-visibility: hidden`, `contain-intrinsic-size`,
`container-type`, `container-name`, `will-change`.

**Details:** content of a scroll container is printed as far as the box shows it, without
scrolling. Its scroll bars are printed as Chrome on Linux prints them (Chrome on macOS overlays
scroll bars, which print nothing): `overflow: scroll` always has them, `overflow: auto` where the
content overflows the box (a long line in a `pre`, a wide table in a scrolling wrapper). A bar is
15px thick (10px with `scrollbar-width: thin`) and takes its room between the border and the
padding, on the left right to left; it has an arrow button at either end and, where the content
overflows, a thumb at the start, as the content is printed scrolled to its start. `scrollbar-color`
colors the thumb and the arrows, and the track. To print no scroll bars, and keep no room for
them, add `scrollbar-width: none` to the scrolling boxes, such as
`@media print { pre, .table-responsive { scrollbar-width: none } }`, or `overflow: hidden`.
`scrollbar-gutter: stable` keeps the room of a vertical bar in a box with `overflow` `hidden`,
`auto` or `scroll` whether it has a bar or not, so that its content does not move when a bar
comes; `stable both-edges` keeps as much on the other side too. The gutter stays empty without a
bar, and a bar lies in it. A scroll container that may be resized (`resize` other than `none`, as
a `<textarea>` by default) has Chrome's resize handle in its bottom corner, two diagonal lines:
over its content, or in the corner of its scroll bars, which then ends a single bar too. Content wider than the page area makes Fagus lay the page out again and scale it down,
as Chrome's shrink-to-fit does (to at most 1.5 times the page width).

**Not supported:** the unprefixed `line-clamp` is ignored, as Chrome 157 ignores it; use
`-webkit-line-clamp` with `display: -webkit-box`.

### Container queries

`@container` with size features (`width`, `height`, `inline-size`, `block-size`, `aspect-ratio`,
`orientation`, ranges) and style queries (`style(--x: y)`). Fagus finds the container sizes in a
first layout and lays the document out again with them.

## Backgrounds, borders and shadows

**Works:**

- Several background layers with `background-image`, `-size`, `-position`, `-repeat` (`space` and
  `round` too), `-origin`, `-clip` (including `text` and `border-area`), `-attachment`
  (`fixed` is placed as Chrome places it in print, in its window of 780 × 380 pixels at the top
  left of every page; `local` in the scrollable overflow) and
  `background-blend-mode`.
- Gradients: `linear-gradient()`, `radial-gradient()`, `conic-gradient()` and their repeating
  forms, with color hints and interpolation methods (`in oklch longer hue`); `image()`,
  `image-set()` (and `-webkit-image-set()`), and `light-dark()` images.
- Borders in all styles (`solid`, `dashed`, `dotted`, `double`, `groove`, `ridge`, `inset`,
  `outset`), drawn with Chrome's dash patterns, miters and 3D shades; `border-radius` with
  elliptical corners; `corner-shape` (with `superellipse()`); `border-shape`; `border-image` with
  all its longhands.
- `box-shadow` (outer and inset, blur and spread) and `text-shadow`.

**Details:** gradients are written as vector shadings in the PDF and stay sharp at any zoom. As in
Chrome's PDF output, the colors between two stops are interpolated in sRGB whatever color space the
gradient names, while color hints follow the named space. A background on the root element (or on
`body` when the root has none) covers the whole canvas on every page. Backgrounds of boxes broken
across pages continue where the page before left off; with `box-decoration-break: clone` each part
starts anew.

## Effects

**Works:**

- 2D and 3D transforms: `transform` with all functions, the individual `translate`, `rotate` and
  `scale`, `transform-origin`, `transform-box`, `transform-style: preserve-3d`, `perspective` and
  `perspective-origin`. 3D transforms are flattened onto the page as Chrome prints them.
- `filter` with all CSS filter functions and SVG filter references (`url(#filter)`).
- `mix-blend-mode`, `isolation`, `background-blend-mode`, as PDF blend modes.
- `mask` and its longhands (also `-webkit-mask-*`): gradient, raster and SVG image layers,
  `mask-mode`, `mask-composite`, `mask-type`, and SVG `<mask>` references.
- `clip-path` with `inset()`, `circle()`, `ellipse()`, `polygon()`, `path()`, `xywh()`, `rect()`,
  `shape()`, a box keyword, or an SVG `<clipPath>` reference.

**Differs from Chrome:** Chrome prints filtered content as an image; Fagus keeps text and shapes as
vectors where the filter allows it (color filters, `drop-shadow()`, `opacity()`), so the text stays
searchable. Blurs and SVG filter primitives without a CSS equivalent (turbulence, lighting,
morphology, displacement) are rasterized at 300 ppi, as Chrome prints them.

**Not supported:** `backdrop-filter` does not filter what lies behind the box; the box is only drawn
as an isolated group.

## Images and replaced content

**Works:**

- `<img>` with PNG, JPEG, GIF, WebP, AVIF, BMP and SVG, and TIFF, which Chrome does not show and
  Safari does. SVG stays vector graphics. JPEG, PNG, WebP and TIFF are turned as their EXIF or TIFF
  orientation says (`image-orientation: none` turns this off); AVIF as its `irot` and `imir`
  say, which `image-orientation: none` keeps, as in Chrome.
- `srcset`, `sizes` and `<picture>` with `media` and `type`, chosen as Chrome chooses them when it
  loads the page (on screen, before printing): in a window of 780 × 380 pixels, as Chrome on Linux
  opens it. `(min-resolution)` queries see 300 dpi.
- `object-fit` and `object-position`, `image-rendering: pixelated` and `crisp-edges`.
- Inline `<svg>`, drawn as vectors in the text's color (`currentColor`) and font, styled by the
  document's CSS (`fill`, `stroke`, Tailwind's `fill-current`), with `<use>` across documents,
  `<foreignObject>` HTML, masks, clips, filters, text on paths and `vector-effect`.
- `content: url(…)` on an element replaces it with the image (as print stylesheets swap logos);
  in `::before` and `::after` it shows the image.
- `<iframe>` (`srcdoc` or `src`), `<object>` and `<embed>` showing an HTML document, an image or a
  text file; frames are laid out at their own size, unpaginated and clipped, as Chrome prints them,
  at most 8 levels deep. `frame-sizing: content-height` sizes a frame to its document.
- `<video>` shows its poster, `<canvas>` is a transparent box.
- MathML Core: `<math>` with fractions, roots, scripts, operators stretched through the font's MATH
  table, and tables. The generic family `math` finds a math font.

**Differs:** JPEG XL is skipped in `<picture>` and `image-set()`; use one of the formats above.
TIFF sources are taken, as in Safari. A PDF file is an image too, as in Safari and Prince: a page of
it (the first, or the one `#page=N` names) is drawn as vector graphics, as large as its crop box, in
`<img>`, `<picture>` (`type="application/pdf"`), `<object>`, `<embed>`, backgrounds (also of
`@page`, for [page templates](paged-media.md#use-a-pdf-as-page-template)), border images, list
markers, `content` and `image-set()`. Chrome shows none. Filters, masks and `shape-outside` do not
use the pixels of a PDF image. An image that does not load shows its alt text after Chrome's broken-image icon. An image
whose data does not decode is left out with an `image.broken` warning.

**Not supported:** scripts in SVG and in frames do not run. Fagus loads no media other than images.

## Text

### White space and line breaking

**Works:** `white-space` with its longhands `white-space-collapse` and `text-wrap-mode`,
`text-wrap` (`wrap`, `nowrap`, `balance`, `pretty`, `stable`) with Chrome's line breakers,
`overflow-wrap` (`anywhere`, `break-word`, also as `word-wrap`), `word-break` (`break-all`,
`keep-all`, `auto-phrase` for Japanese, and the old `break-word`), `line-break`, `<wbr>`, soft
hyphens (`&shy;`), `tab-size`, and `hanging` and `each-line` in `text-indent`.

**Details:** lines break where Chrome breaks them: between ASCII characters by Blink's own table
(after `-` and `?`, not after the slashes and dots of a URL), elsewhere by the Unicode line breaking
algorithm. Thai, Lao, Khmer and Myanmar break between dictionary words.

### Hyphenation

`hyphens: auto` hyphenates with Chrome's dictionaries and rules on Linux, the same on every system,
in the language of the text's `lang` attribute. Without `lang` nothing is hyphenated. There are
dictionaries for Afrikaans, Assamese, Belarusian, Bengali, Bulgarian, Czech, Church Slavonic,
Welsh, Danish, German (1901, 1996 and Swiss), Greek, English (US and UK), Spanish, Estonian, Basque,
French, Irish, Galician, Gujarati, Hindi, Croatian, Italian, Georgian, Kannada, Latin, Lithuanian,
Malayalam, Mongolian, Marathi, Ethiopic scripts, Norwegian (Bokmål and Nynorsk), Dutch, Odia,
Punjabi, Portuguese, Slovak, Slovenian, Albanian, Swedish, Tamil, Telugu, Turkmen and Ukrainian.
Hungarian, Armenian, Latvian and Russian are left out for their licenses.

`hyphenate-character` and `hyphenate-limit-chars` work. As in Chrome, capitalized English words and
the last word of a paragraph are not hyphenated. The hyphen at a break is a soft hyphen in the
text of the PDF, so copied text and screen readers get the whole word.

### Alignment and spacing

**Works:** `text-align` (including `justify`, `start`, `end`, `match-parent`), `text-align-last`,
`text-justify` (`auto`, `none`, `inter-word`, `inter-character`), `text-indent`, `letter-spacing`
and `word-spacing` (also as percentages), `text-transform` (with language rules for Greek and
Turkish, and `math-auto`), `initial-letter`, `text-box-trim`, `text-box-edge` and `text-box`,
`text-fit`.

**Not supported:** `text-align: justify-all` is invalid, as in Chrome; use `text-align: justify`
with `text-align-last: justify`. `text-justify: distribute` is invalid.

### Hanging punctuation

`hanging-punctuation` lets punctuation at the edges of lines hang outside the line box, as CSS Text
3 specifies it and Safari does it (Chrome does not support it):

```css
blockquote { hanging-punctuation: first last; }  /* quotes outside the text's edge */
p { text-align: justify; hanging-punctuation: allow-end; }
```

- `first`: an opening bracket or quote (or an ideographic space) at the start of the first line of
  a block hangs before it, in the `text-indent` if there is one.
- `last`: a closing bracket or quote at the end of the last line of a block hangs after it.
- `force-end`: a stop or comma (`,` `.` `、` `。` and their fullwidth, small and Arabic forms) at the
  end of a line hangs.
- `allow-end`: such a stop or comma hangs only where it would not fit on the line otherwise, as far
  as it does not fit.

A hanging mark takes no room on the line: the line breaks, aligns and is justified as if it were not
there, and it does not count for the widths of boxes that size to their content. It reaches into the
block's padding or margin without making the page shrink to fit; leave room for it there, since
content past the page area is cut off at the page margins. At most one mark hangs at each edge of a
line, and a mark hangs only at the very edge: not behind the padding or border of an inline box,
and not where bidi reordering of mixed text moves it away from the edge. In right-to-left text,
`first` hangs on the right and the end of the line is on the left; in vertical text, they hang above
the top and below the bottom of the line.

### Decorations and emphasis

**Works:** `text-decoration` with `-line`, `-style` (`solid`, `double`, `dotted`, `dashed`, `wavy`),
`-color`, `-thickness`, `text-underline-offset`, `text-underline-position`,
`text-decoration-skip-ink`; `-webkit-text-fill-color`; emphasis marks (`text-emphasis` and its
longhands).

**Not supported:** `-webkit-text-stroke` and `paint-order` for HTML text.

### Writing modes and bidirectional text

**Works:** `writing-mode` (`horizontal-tb`, `vertical-rl`, `vertical-lr`, `sideways-rl`,
`sideways-lr`, the SVG names and `-webkit-writing-mode`), `text-orientation`,
`text-combine-upright: all`, `direction`, `unicode-bidi`, `dir` and `dir=auto`, `<bdi>` and `<bdo>`.
Logical properties, tables, flex, grid and columns follow right-to-left and vertical text. Hebrew
and Arabic copy out of the PDF in reading order.

### East Asian text and ruby

**Works:** `<ruby>`, `<rt>` and `<rp>` with `ruby-position` and `ruby-align`, nested and double-sided
ruby; `text-spacing-trim`, `text-autospace`, and justification between ideographs and kana, as in
Chrome. Japanese and Chinese stand upright in vertical text with the vertical forms of the font.

**Not supported:** `ruby-overhang`, `text-spacing`.

### Scripts

Text is shaped with HarfBuzz rules in its script and language: Arabic and other joining scripts join
(also across inline elements), Indic scripts form their clusters, and the `lang` attribute selects
a font's localized forms. Emoji sequences use a color font where one is available.

## Fonts

`@font-face` (TTF, OTF, WOFF, WOFF2, variable fonts, `unicode-range`, `local()`, `size-adjust` and
the metric overrides), `font-family`, `font-weight`, `font-style` (italic and oblique angles),
`font-stretch`, `font-size` (with `math`), `font-size-adjust`, `font-synthesis`, `font-kerning`,
`font-optical-sizing`, `font-variation-settings`, `font-feature-settings`, `font-variant` with all
its longhands, `font-palette` with `@font-palette-values`, and `text-rendering`. See
[Fonts](fonts.md) for where fonts come from and how fallback works.

**Not supported:** `@font-feature-values` (so `font-variant-alternates: styleset(name)` has no
named sets).

## Lists and counters

**Works:** `list-style-type` with all of Chrome's predefined counter styles, `list-style-position`,
`list-style-image`, `::marker`, `@counter-style` (all systems and descriptors), `counter-reset`,
`counter-increment`, `counter-set`, `counter()` and `counters()`, and the `start`, `reversed` and
`value` attributes.

**Details:** `disc`, `circle`, `square` and the disclosure markers are drawn as Chrome's shapes,
not as glyphs, at Chrome's distance from the text.

## Generated content

**Works:** `content` on `::before`, `::after` and `::marker` with strings, `attr()`, `counter()`,
`counters()`, `open-quote`/`close-quote` (with `quotes`, and `auto` quotes by language), images
(`url()`, gradients) and alternative text after `/`. In print, `content` also takes the functions of
CSS Generated Content for Paged Media: `string()`, `element()`, `target-counter()`,
`target-counters()`, `target-text()` and `leader()` (see [Paged media](paged-media.md)). `<q>`
shows quotation marks of the language around it.

## Form controls

Form controls print as Chrome prints them: text, password, number, email, date and time fields,
checkboxes and radio buttons (with `accent-color`), buttons, `<select>` and list boxes,
`<textarea>`, `<progress>`, `<meter>`, range sliders, color and file inputs, `<fieldset>` with
`<legend>`, `<details>` and `<summary>`, `<dialog>`. `appearance: none` or author borders and
backgrounds give the CSS look instead of the native one, as in Chrome. `field-sizing: content`
works. A text field of `size` characters is as wide as Chrome on Linux makes it, from the average
character width of the font's OS/2 table and the width of the box around its glyphs (`head`);
the spin button of a number field and the room for the arrow of a `<select>` are 15px wide.

**Differs from Chrome:** the texts controls show of their own (the labels of submit and reset
buttons, "Choose File", date placeholders such as `mm/dd/yyyy`, the decimal separator) are written
in the language of the control's `lang`, else the engine's `locale`, else American English. Chrome
writes them in the language of its user interface. See [Fonts](fonts.md#the-language-of-form-controls).

With `forms()`, the controls become fields that a reader fills in, and look the same
([The PDF](pdf.md#fill-in-forms)).

## Media queries and feature queries

Media queries are evaluated for print:

| Feature | Value in Fagus |
|---|---|
| media type | `print` (`screen` rules do not apply) |
| `width`, `height`, `aspect-ratio`, `orientation` | the page area of the first page |
| `resolution` | 300 dpi (96 dpi while choosing `<picture>` sources, as Chrome chooses them on screen) |
| `color` | 8; `monochrome`, `color-index` and `grid` are 0 |
| `color-gamut` | `srgb` |
| `dynamic-range` | `standard` |
| `overflow-block` | `paged` |
| `prefers-color-scheme` | `light` |
| `prefers-reduced-motion`, `prefers-contrast`, `prefers-reduced-transparency` | `no-preference` |
| `hover`, `any-hover` | `hover` |
| `pointer`, `any-pointer` | `fine` |
| `scripting`, `update`, `overflow-inline`, `forced-colors` | `none` |
| `display-mode` | `browser` |

A Tailwind document with `print:` variants or a stylesheet with `@media print { … }` therefore gets
its print styles.

`@supports` knows the properties and values Fagus parses (also its own, such as
`-fagus-repeat`, `-fagus-page-height` and `-fagus-layer`, and those of paged media that Chrome
lacks, such as `position: running(header)` and `float: footnote`), and answers `selector()`, `font-tech()`, `font-format()` and `at-rule()` as
Chrome does.

## Animations and transitions

Scripts do not run, so nothing moves. CSS animations print at time zero as in Chrome: a fade-in
prints transparent, a slide-in where it starts, a negative `animation-delay` that far into the
animation, with fill modes, direction and easing. Transitions have no effect.

If a page uses animations to reveal content, turn them off for print:

```css
@media print {
  * { animation: none !important; }
}
```

## Not supported

A short list of what Fagus does not do. Most of it is also absent in Chrome's print.

- **Scripts:** `<script>` does not run, in the document, in SVG or in frames. `<noscript>` content
  is shown. Content that JavaScript would build (charts, client-side rendering) must be rendered on
  the server first, for example as inline SVG.
- **Network:** nothing is loaded from `http:` or `https:` URLs; hand resources over instead (see
  [Resources](resources.md)).
- **Interaction:** `:hover`, `:focus`, `:active`, `:visited` never match; transitions do nothing;
  animations print their state at time zero.
- **Footnotes:** `float-reference: column` (footnotes go into the footnote area of the page, also
  in columns, see [Paged media](paged-media.md#footnotes)).
- **Layout:** `display: grid-lanes` (masonry), `display: run-in`, unprefixed `line-clamp`,
  `shape-inside`, `block-step`.
- **Text:** `text-align: justify-all`, `-webkit-text-stroke`, `paint-order` for HTML text,
  `text-spacing`, `ruby-overhang`, `@font-feature-values`.
- **Effects:** `backdrop-filter` (no filtering of the backdrop), `cross-fade()`, `element()` as an
  image.
- **Images:** JPEG XL and ICO.
- **At-rules:** `@function`, `@starting-style` and `@view-transition` have no effect.
