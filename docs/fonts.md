# Fonts

Fagus uses only the fonts you give it. It does not look at the fonts installed on the machine
unless you ask it to, so a document prints the same on your laptop, in CI and on the production
server. This page describes where fonts come from, how CSS finds them, and what Fagus embeds in the
PDF.

Fonts come from four places, in this order:

1. `@font-face` rules of the document.
2. Fonts you configure: font directories and fonts added with `addFont()`.
3. The fonts installed on the system, only if you turn them on.
4. Inter, built into Fagus, for anything no other font covers.

## The built-in font

Fagus brings Inter in regular and bold (SIL Open Font License 1.1). A document renders without any
fonts of your own: text in a family that no font has, and characters that no other font covers, are
set in Inter. Italics are made up by slanting the regular face.

Inter covers Latin, Greek and Cyrillic. Characters it lacks (Chinese, Japanese, Arabic, emoji …)
show as a "no glyph" box unless you add a font that has them. A normal PDF gets no warning for this;
PDF/A refuses such a document ("Inter has no glyph for `日`, and no other font has one").

A font of the family Inter that you add (in a font directory, with `addFont()` or `@font-face`)
replaces the built-in faces.

## Add font directories

A font directory makes its fonts available to every document by their family names, as installed
fonts are in a browser.

```php
use Fagus\Engine;
use Fagus\Fagus;

Fagus::useEngine(new Engine(
    fontDirectories: [__DIR__.'/resources/fonts'],
));
```

```html
<style>
  body { font-family: "Source Sans 3", sans-serif; }
</style>
```

In Laravel, list the directories under `fonts` in `config/fagus.php`. The default is
`resources/fonts`, so fonts you put there work without configuration. Directories that do not exist
are skipped.

```php
// config/fagus.php
'fonts' => [
    resource_path('fonts'),
    storage_path('app/fonts'),
],
```

On the command line, `--font-dir` adds a directory and can be repeated:

```sh
fagus render invoice.html -o invoice.pdf --font-dir resources/fonts
```

Details:

- Fagus reads files ending in `.ttf`, `.otf`, `.ttc`, `.otc`, `.woff` and `.woff2`, in the
  directory and its subdirectories. Files that are not fonts are skipped.
- The family name is the one stored in the font file (the name your operating system's font viewer
  shows), not the file name. All weights and styles of a family can be in one directory; CSS font
  matching picks the right face.
- The worker reads the directories when it starts. Fonts added to a directory later are seen by the
  next worker (after `close()`, a deploy, or a restart of the queue worker).
- In PHP, every directory in `fontDirectories` must exist: before it starts the worker, the engine
  throws an `EngineException` that names a missing one ("The font directory `…` does not exist.").
  The Laravel integration skips directories of its `fonts` configuration that do not exist, since
  its default, `resources/fonts`, is optional.

## Add a font from memory

`addFont()` adds a font from its bytes, for all later documents of the engine. It returns the number
of faces in the file (a collection has several).

```php
use Fagus\Fagus;

$faces = Fagus::engine()->addFont(file_get_contents(__DIR__.'/resources/fonts/HouseFont.otf'));
```

In Laravel, use the facade's engine: `Fagus::engine()->addFont($bytes)`. The engine keeps the font
and hands it to a new worker whenever one starts. Prefer font directories for fonts you always need:
`addFont()` sends the font through the pipe to the worker each time a worker starts.

## Load fonts with @font-face

A document can bring its own fonts. The URLs resolve like those of images: against the base
directory, from resources handed over, or through the resolver (see [Resources](resources.md)).
Fagus never loads fonts from the network, so a `<link>` to Google Fonts works only through a
resolver that fetches it ([Resources](resources.md#load-web-fonts-through-the-resolver)); for
output that stays the same, download the font files and serve them locally.

```html
<style>
  @font-face {
    font-family: "Brand";
    src: url(fonts/Brand-Regular.woff2) format("woff2");
    font-weight: 400;
  }
  @font-face {
    font-family: "Brand";
    src: url(fonts/Brand-Bold.woff2) format("woff2");
    font-weight: 700;
  }
  body { font-family: "Brand", sans-serif; }
</style>
```

```php
Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf')   // fonts/Brand-Regular.woff2 is read from here
    ->save('invoice.pdf');
```

What works, as in Chrome:

- **Formats:** TrueType, OpenType, WOFF, WOFF2 and collections. Sources are tried in order, and
  sources whose `format()` or `tech()` Chrome does not load (EOT, SVG fonts, Graphite,
  incremental) are skipped.
- **`local()`** finds a face by its PostScript or full name among the fonts of your font
  directories (and the system's, if system fonts are on).
- **Descriptors:** `font-weight`, `font-style` (`italic`, `oblique` with angles and ranges),
  `font-stretch` (with ranges), `unicode-range`, `size-adjust`, `ascent-override`,
  `descent-override`, `line-gap-override` and `font-feature-settings`. `font-display` has no effect
  on a document that is rendered once.
- **`unicode-range`:** rules of one family with the same descriptors make one face whose files
  cover characters by their ranges, as Google Fonts serves its fonts in pieces.
- **Variable fonts:** text is set at the coordinates its style asks for: the weight on `wght`,
  the width on `wdth`, the size on `opsz` (`font-optical-sizing`), then `font-variation-settings`.
  A weight range in the rule (`font-weight: 100 900`) limits the weights, without one the font's
  axis does.

A font that cannot be loaded is reported as `resource.not-loaded`, a file that is not a font Fagus
can read as `font.unsupported`; the next source or the next family in `font-family` takes over.
Fonts are cached by the engine, so a template that loads the same font for every document reads it
once.

## Fonts in headers and footers

Chrome loads the faces of `@font-face` rules while it lays out the document, and not again for the
page margins. A face that the content of the document never uses is therefore missing in the margin
boxes, and their text falls back to the next family in its `font-family`. Fagus does the same, so
that headers and footers look as they do in Chrome.

```html
<style>
  @font-face { font-family: Brand; src: url(fonts/Brand-Regular.woff2); }
  @font-face { font-family: Brand; src: url(fonts/Brand-Italic.woff2); font-style: italic; }
  @page {
    @top-center { content: "Annual report"; font-family: Brand; font-style: italic; }
  }
  body { font-family: Brand; }   /* no italic text in the content */
</style>
```

Here the running head is not set in Brand Italic. To fix it, either use the face somewhere in the
document's content, or load the font from a font directory or with `addFont()` instead of
`@font-face`: those fonts are always available, also in margin boxes. Running elements
(`position: running()`) are part of the content and are not affected.

## Set the generic families

The generic families `serif`, `sans-serif` and `monospace` have no fonts of their own in Fagus.
Tell Fagus which families they stand for:

```php
use Fagus\Engine;
use Fagus\Fagus;

Fagus::useEngine(new Engine(
    fontDirectories: [__DIR__.'/resources/fonts'],
    sansSerif: 'Inter',
    serif: 'Source Serif 4',
    monospace: 'JetBrains Mono',
));
```

```php
// config/fagus.php (Laravel)
'families' => [
    'sans-serif' => 'Inter',
    'serif' => 'Source Serif 4',
    'monospace' => 'JetBrains Mono',
],
```

On the command line, `fagus render` takes `--sans-serif`; the worker that PHP starts also takes
`--serif` and `--monospace`.

Where a family is not set, Fagus chooses:

| CSS | Font |
|---|---|
| no `font-family` at all | the `serif` family, as browsers use their serif font by default |
| `serif` | the `serif` family, else the first family added, else Inter |
| `sans-serif`, `system-ui` | the `sans-serif` family, else the first family added, else Inter |
| `monospace` | the `monospace` family, else Menlo, Consolas, DejaVu Sans Mono or Liberation Mono if added, else any fixed-pitch font added, else the first family added |
| `cursive`, `fantasy` | nothing; the next family in the list |
| `math` | a font with a MATH table (STIX Two Math, Cambria Math, Latin Modern Math …), else the default font |
| a family no font has | the `sans-serif` family, else the first family added, else Inter |

"The first family added" is the family of the first font file read from the font directories (in
alphabetical order of their paths), else of the first `addFont()`. Two consequences:

- Text without `font-family` is set in the `serif` family. If you set only `sansSerif`, give your
  `body` a `font-family` too.
- With several fonts in your directories, name the family in your CSS or set the generic families,
  rather than relying on the first family added.

Text in exactly `font-family: monospace` gets Chrome's default size of 13px instead of 16px, as in
every browser.

## Use the fonts installed on the system

With system fonts on, the fonts installed on the machine serve too, as in Chrome:

```php
new Engine(systemFonts: true);
```

```php
// config/fagus.php, or FAGUS_SYSTEM_FONTS=true in .env
'system_fonts' => true,
```

```sh
fagus render page.html -o page.pdf --system-fonts
```

Fagus then finds fonts where Chrome finds them:

- **macOS:** `/System/Library/Fonts`, `/Library/Fonts` and `~/Library/Fonts`.
- **Linux:** the directories fontconfig lists, else `/usr/share/fonts`, `/usr/local/share/fonts`
  and `~/.local/share/fonts`.
- **FreeBSD:** the directories that fontconfig's configuration in `/usr/local/etc/fonts` lists
  (`fonts.conf`, `conf.d` and `local.conf`), else those of Linux. Font packages such as
  `dejavu` or `noto` install into `/usr/local/share/fonts`.
- **Windows:** `%WINDIR%\Fonts` and the fonts installed for the user.

With system fonts:

- Families that documents name come from the system. Your font directories and `@font-face` still
  come first for their families; faces from a font directory join the installed faces of the same
  family.
- The generic families are those of Chrome on macOS where they are installed (Times, Helvetica,
  Menlo, San Francisco for `system-ui`, Apple Chancery, Papyrus), else those of Windows and Linux
  (Times New Roman, Arial, Consolas, Liberation, DejaVu, Noto …). For Japanese, Korean and Chinese
  text (by `lang`) they are the fonts Chrome takes for these languages (Hiragino, Apple SD Gothic
  Neo, PingFang, Yu Gothic, Malgun Gothic, Microsoft YaHei …). Chrome on Linux has none for these
  languages: there the text takes the default families, and its characters Noto Sans CJK.
- A family the system lacks takes an installed family with the same glyph widths, as fontconfig
  substitutes it on Linux and Chrome accepts it there: Liberation Sans (or Arimo) for Arial and
  Helvetica, Liberation Serif (or Tinos) for Times New Roman and Times, Liberation Mono (or
  Cousine) for Courier New and Courier, Carlito for Calibri and Caladea for Cambria. Form
  controls, whose font is Arial, get Liberation Sans on Linux.
- Text without `font-family` is set in the standard font of its language, as in Chrome.
- `ui-sans-serif`, `ui-serif`, `ui-monospace`, `ui-rounded` and `-apple-system` match nothing, as in
  Chrome, so Tailwind's default `font-sans` stack gets the system font.
- The built-in Inter no longer answers to the name Inter unless Inter is installed or added.
- Installed fonts are indexed when the worker starts and read only when a document uses them.
- A face whose glyphs Fagus cannot draw is passed over for the next font, with a `font.unsupported`
  warning.

The output then depends on the fonts of the machine. Leave system fonts off on servers, and use it
where documents must look like Chrome on the same machine, or for scripts that no font of yours
covers.

## Fall back for missing characters

When the font of the text lacks a character, Fagus looks for it as Chrome does:

1. in the other families of the element's `font-family` list, in order;
2. with system fonts on: in the standard font of the text's language, then in the fonts the system
   falls back to for the character's script (PingFang or Songti for Chinese, Hiragino for Japanese,
   Geeza Pro for Arabic, Apple Symbols for symbols … on macOS);
3. in the other fonts you added;
4. with system fonts on: in Noto Sans CJK of the text's language for Chinese, Japanese and Korean,
   then in the installed families of `sans-serif` (Helvetica, Arial, Liberation Sans, DejaVu Sans,
   Noto Sans), as fontconfig ranks them for Chrome on Linux, then in the first installed font that
   has the character. As fontconfig answers Chrome there, this takes the regular face, which bold
   or italic text makes bold or slanted (bold Persian text is DejaVu Sans made bold, not DejaVu
   Sans Bold), except for the characters of Japanese, Korean, Chinese and Hindi text, which take
   the face of their weight and style;
5. in the built-in Inter.

Spaces take the first font of the list that has them, and a letter with combining accents stays in
one font that can draw the whole cluster. With `line-height: normal`, a line with glyphs from a
fallback font grows to make room for them, as in Chrome.

A character that no font has is drawn as the empty box (`.notdef`) of the first font. Its text
stays in the PDF, so that it is copied and found as the character it is. The warning
`font.missing-glyph` names such characters, up to eight and the count of the rest, how often they
occur and the line of the element of the first:

```text
html:12: warning[font.missing-glyph]: no font has glyphs for 2 characters in 3 places: `漢` (U+6F22), `字` (U+5B57); they are drawn as empty boxes (add a font that has them)
```

PDF/A and PDF/UA do not allow such characters: the document is refused instead (see
[pdf.md](pdf.md)).

To print a script reliably, add a font for it and name it in the font list:

```css
body { font-family: "Inter", "Noto Sans JP", sans-serif; }
```

## Bold, italic and small capitals

Fagus picks the closest face of a family by width, style and weight, as CSS font matching
describes. Where a family has no bold or italic face, Fagus makes one up as Chrome does: bold by
stroking the outlines, italic by slanting them. `font-synthesis` controls this:

```css
.logo { font-synthesis: none; }          /* never make up bold, italic or small caps */
.label { font-synthesis: weight; }       /* only bold */
```

`font-variant-caps: small-caps` uses the font's small capitals (`smcp`); where the font has none,
Fagus makes them from capitals at 70 % of the size, as Chrome does.

## OpenType features

CSS controls the font's OpenType features as in Chrome:

- `font-variant-numeric`: `tabular-nums` (columns of amounts that line up), `oldstyle-nums`,
  `lining-nums`, `slashed-zero`, `diagonal-fractions`, `ordinal`.
- `font-variant-ligatures`, `-caps`, `-east-asian`, `-position`, and
  `font-variant-alternates: historical-forms`.
- `font-kerning`, `font-optical-sizing`, `font-variation-settings`.
- `font-feature-settings` for anything else (`"ss01"`, `"cv11"`), applied last, also as an
  `@font-face` descriptor.
- `font-palette` and `@font-palette-values` for color fonts.

```css
td.amount { font-variant-numeric: tabular-nums; text-align: right; }
```

As in Chrome, `letter-spacing` other than 0 turns off common and contextual ligatures.

## Languages

The `lang` attribute matters for fonts and text:

- Text is shaped in its language, so a font's localized forms (`locl`) apply: Bulgarian or
  Serbian Cyrillic letter forms, or EB Garamond's German `f`, which takes no `fi` ligature.
- `hyphens: auto` needs `lang` to hyphenate. Fagus has Chrome's hyphenation dictionaries for 48
  languages (see [CSS reference](css.md#hyphenation)).
- Quotation marks of `<q>` follow the language.
- With system fonts on, the generic families and fallback fonts depend on it.

Set the language on the root element. It also becomes the language of the PDF.

```html
<html lang="de">
```

## The language of form controls

Form controls show texts of their own: the labels of submit and reset buttons, "Choose File" and "No
file chosen" of file inputs, the placeholders and formats of date and time fields (`mm/dd/yyyy`,
12-hour time), the decimal separator of number fields, and the "Details" of a `<details>` without a
summary.

Chrome writes these in the language of its user interface. Fagus writes them as Chrome does in the
language of the control instead: its `lang` (inherited, as from `<html lang="de">`), else the
engine's `locale`, else American English.

```php
new Engine(locale: 'de-DE');
```

```php
// config/fagus.php, or FAGUS_LOCALE=de-DE in .env
'locale' => env('FAGUS_LOCALE'),
```

```sh
fagus render form.html -o form.pdf --locale de-DE
```

Fagus has Chrome's texts and formats for English (`en-US`, `en-GB`, `en-AU`, `en-IN`), German,
French (`fr`, `fr-CA`), Spanish (`es`, `es-419` and `es-MX`), Italian, Dutch, Portuguese (`pt-BR`,
`pt-PT`) and Polish. A `lang` of another language leaves the choice to the `locale`.

The `locale` also sets the language that text without `lang` is shaped in, as Chrome shapes text in
the language of its user interface. It does not turn on hyphenation: that needs `lang`. The PDF's
language falls back to the `locale` where the document has no `lang`.

## Fonts in the PDF

Every font a document uses is embedded in the PDF as a subset that holds only the glyphs used, so a
PDF with a large font stays small. Make sure the license of your fonts allows embedding: Fagus
embeds fonts whatever their license flags say, except in PDF/A, which refuses a font whose flags
restrict embedding.

- An instance of a variable font is embedded as a static font with the font's PostScript name, so
  every PDF viewer shows it.
- Text stays text: it can be searched, copied and read by screen readers. A hyphen added at a line
  break is a soft hyphen in the text, so copied words come out whole.
- Glyphs that a clip hides entirely are left out, so text extraction finds only what is visible.

Embedded subsets are also what PDF/A requires, so PDF/A needs no extra font settings (see
[PDF output](pdf.md)).

## Make output reproducible

With system fonts off (the default), the fonts of a document are exactly the built-in Inter, your
font directories, the fonts added with `addFont()` and the document's `@font-face` rules. Hyphenation
dictionaries and line breaking rules are built into Fagus. A document therefore renders the same PDF
on Linux, macOS, Windows and FreeBSD, as long as the fonts and the document are the same.

For byte-identical files, also fix the creation date:

```php
Fagus::html($html)->created(new DateTimeImmutable('2026-11-02 12:00'))->save('invoice.pdf');
```

Commit your fonts with the application (for example in `resources/fonts`) rather than relying on
fonts installed on the server.
