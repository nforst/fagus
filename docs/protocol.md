# Worker protocol

`fagus worker` renders documents for a caller over standard input and output. One process serves
any number of documents one after another, with fonts loaded and caches warm, so a document costs
the time it takes to render it and nothing more. The PHP package uses this protocol; any other
language can too.

## Frames

Every message is a frame:

| Bytes | Content |
|---|---|
| 4 | length of the rest of the frame, unsigned, little-endian |
| 1 | message type |
| 4 | length of the JSON header, unsigned, little-endian |
| n | JSON header (UTF-8) |
| … | data blocks, one after another |

The header's `blobs` array lists the lengths of the data blocks that follow it; frames without
data have no `blobs`. Data travels as raw bytes, never base64. Frames are at most 1 GiB. A frame
the worker cannot read (too long, a header that is not JSON, data blocks longer than the frame)
ends the worker with exit code 1 and the reason on its error output.

## Messages

| Type | Direction | Header | Data |
|---|---|---|---|
| `H` hello | worker → caller | `protocol`, `version`, `sandbox` | – |
| `R` render | caller → worker | the document, see below | HTML, stylesheets, resources, attachments, invoice XML |
| `?` ask | worker → caller | `id`, `url` | – |
| `r` resource | caller → worker | `found` | the resource, if found |
| `P` PDF | worker → caller | `id`, `pages`, `diagnostics`, `timings`, `encrypted`, `signed`, `forms`, `layout`, `rss`, `maxRss` | the PDF |
| `E` error | worker → caller | `id` (if known), `message`, `diagnostics`, `rss`, `maxRss` | – |
| `F` font | caller → worker | – | a font file (TTF, OTF, TTC, WOFF, WOFF2) |
| `K` done | worker → caller | `faces` | – |
| `S` sign | worker → caller | `id`, `algorithm` | the digest to sign |
| `s` signed | caller → worker | `error` (if refused) | the signature |
| `T` timestamp | worker → caller | `id` | the digest to timestamp |
| `t` timestamped | caller → worker | `error` (if refused) | the timestamp |
| `V` revocation | worker → caller | `id`, `ocspUrls`, `crlUrls` | the certificate, its issuer's certificate, the OCSP request |
| `v` revocation data | caller → worker | `error` (if refused) | OCSP responses, CRLs and certificates, any number |

The worker sends `H` when it starts; its `protocol` is 1 and its `version` that of the binary
([Versions](#versions)). `sandbox` says how much of the [sandbox](cli.md#sandbox) is in force:
`on`, `partial` (the kernel lacks Landlock or seccomp) or `off` (turned off, or not Linux). The
worker ends with exit code 0 when its input closes. A message of an unknown type gets an `E`
without `id`.

### Rendering

The caller sends `R` and reads frames until it gets `P` or `E`. Meanwhile the worker may ask for
resources it needs with `?`, if the request allows it (`ask`); the caller answers each one with
`r`, with the data or `found: false`. A request with a `signature` of the caller's own signer may
also bring `S` and `T`, and one with `revocation` brings `V` (see [Signatures](#signatures)),
each answered with `s`, `t` or `v`. Nothing else may come in between.

The header of `R`; fields other than `id` and `html` may be left out:

```json
{
  "id": 1,
  "html": 0,
  "xhtml": false,
  "base": "/app/resources/pdf",
  "root": "/app/resources",
  "stylesheets": [{ "name": "app.css", "blob": 1 }],
  "resources": [{ "url": "logo.png", "blob": 2 }],
  "ask": true,
  "conformance": "3b",
  "tagged": true,
  "accessible": false,
  "forms": false,
  "attachments": [{ "name": "data.csv", "mime": "text/csv", "relationship": "supplement", "description": "Rohdaten", "blob": 3 }],
  "facturX": { "blob": 4, "profile": "en16931", "version": "1.0", "description": "Factur-X/ZUGFeRD invoice" },
  "created": "2026-11-02T12:00:00Z",
  "layout": true,
  "encryption": { "userPassword": "open sesame", "ownerPassword": "change me", "permissions": ["print", "copy"] },
  "memoryBudget": 1073741824,
  "timeLimit": 54000,
  "blobs": [5120, 812, 20431, 96, 4187]
}
```

- `html`, `blob`: the index of a data block. The HTML and style sheets must be UTF-8.
- `xhtml`: whether the markup is XHTML, parsed as XML as browsers parse `.xhtml` files (`false`
  if left out).
- `base`: the directory that relative URLs resolve against; files are read from there (and from
  `root`, if given, which must contain it). URLs that start with `/` resolve against `root` (or
  `base`), as on a web server. Without `base` the worker reads no files.
- `resources`: data the document may use, by URL as written in the document. The documents of
  frames (`<iframe src>`, `<object data>`) ask for their resources by their URLs resolved against
  the frame's URL, relative to the document (`sub/style.css` for `style.css` in `sub/page.html`).
- `stylesheets`: style sheets added after those of the document; `name` is the source that
  diagnostics name.
- `ask`: whether the worker asks for resources it does not have (`false` if left out). A resource
  is looked for as a `data:` URL, in `resources`, by asking the caller, then as a file, in that
  order. The worker asks once for each URL in a render.
- `conformance`: `1b`, `2b`, `2u`, `2a`, `3b`, `3u`, `3a`, `4`, `4e` or `4f` for PDF/A (see
  [pdf.md](pdf.md#write-pdfa)).
- `tagged`: whether the PDF is tagged, with the logical structure of the document (`true` if
  left out).
- `accessible`: whether the PDF conforms to PDF/UA-1 (and is tagged). The document needs a
  title, a language and alternative text for every image; otherwise the answer is `E`.
- `forms`: whether the form controls become the fields of an interactive form (`false` if left
  out; see [pdf.md](pdf.md#fill-in-forms)). Added in 1.0.0: a worker of an older release
  ignores it and prints the controls as they look, which the caller notices by the missing
  `forms` in `P`.
- `attachments`: `name` and `mime` are required. `relationship` is `source`, `data`,
  `alternative`, `supplement` or `unspecified` (`unspecified` if left out). `description` may be
  left out (or be `null` or empty): PDF/A-3 and PDF/UA require one, so the worker then describes
  the file by its `name`, as `fagus render --attach` does.
- `facturX`: the invoice XML of a ZUGFeRD or Factur-X invoice; `profile` (`minimum`, `basicwl`,
  `basic`, `en16931`, `extended`, `xrechnung`) is read from the XML if left out (`en16931` if
  the XML declares none). `version` is the `Version` of the Factur-X XMP metadata (`1.0` if left
  out or `null`), `description` the description of the attached XML ("Factur-X/ZUGFeRD invoice"
  if left out or `null`). An empty `version` or `description`, or one with control characters,
  is answered with `E`.
- `layout`: whether the answer reports where the elements with an `id` ended up (`false` if
  left out), in the `layout` of `P` (see [Layout](#layout)). Finding it takes a little time, so it
  is only done when asked.
- `created`: the creation date, as `2026-11-02T12:00:00Z` or with an offset from UTC
  (`2026-11-02T13:00:00+01:00`), which the worker applies; the current time if left out. A date
  without an offset is taken as UTC.

- `encryption`: encrypts the PDF with AES-256 (see
  [pdf.md](pdf.md#protect-the-pdf-with-a-password)). `userPassword` is needed to open it,
  `ownerPassword` lifts the restrictions; either may be left out (or be `null` or empty), but
  not both. `permissions` lists what the PDF allows without the owner password: `print`,
  `print-low-quality`, `modify`, `copy`, `annotate`, `fill-forms` and `assemble`; everything if
  left out or `null`, nothing beyond reading if empty. The passwords are prepared with SASLprep
  (RFC 4013), as PDF 2.0 requires. An unknown permission, no password, a password that SASLprep
  refuses, and `encryption` together with `conformance` or `facturX` are answered with `E`;
  together with `accessible` it writes an encrypted PDF/UA file.
  Added in 1.0.0: an older worker ignores the field and writes the PDF unencrypted, which the
  caller notices by the missing `encrypted` in `P`.

- `memoryBudget`: the memory budget of the document in bytes, `0` for none; the worker's
  (`--memory-budget`, else 2 GiB) if left out or `null`. `timeLimit`: its time limit in
  milliseconds, from when the worker has read `R`, including the time it waits for answers to
  `?`; `0` for none, the worker's (`--time-limit`, else none) if left out or `null`. A document
  that needs more is answered with `E`, with the diagnostic `limit.memory` or `limit.time`, and
  the worker goes on with the next request ([cli.md](cli.md#limits) says what the budget counts).
  Added in 1.0.0: an older worker ignores both.

A request the worker cannot use (a missing data block, an unknown PDF/A level, relationship or
profile, an invalid date) is answered with `E` and its reason, as is a document that does not
meet PDF/A or PDF/UA.

The header of `P`:

```json
{
  "id": 1,
  "pages": 3,
  "diagnostics": [
    { "severity": "warning", "code": "css.unknown-property", "message": "unknown property `colr`; the declaration is ignored", "source": "<style> 1", "line": 12, "column": 3 }
  ],
  "timings": { "parse": 0.12, "style": 0.31, "layout": 0.44, "paint": 0.02, "pdf": 0.71 },
  "encrypted": false,
  "signed": false,
  "forms": false,
  "rss": 41943040,
  "maxRss": 102760448,
  "blobs": [48213]
}
```

`encrypted` is `true` when the PDF is encrypted (the request had `encryption`), `false`
otherwise; a caller that asked for encryption and does not get `true` must not use the PDF.
`signed` is the same for `signature`, `forms` for `forms: true`. Times
are in milliseconds. `severity` is `info`, `warning` or `error`; `line` and `column` are 0
when unknown. With `layout: true` in the request, the header has a `layout` too.

### Layout

The `layout` of `P` tells where the elements with an `id` ended up, in PDF points from the top left
corner of each page box, rounded to a hundredth of a point:

```json
{
  "pages": [{ "width": 595.28, "height": 841.89 }, { "width": 595.28, "height": 841.89 }],
  "elements": [
    { "id": "total", "tag": "p", "boxes": [{ "page": 2, "x": 56.69, "y": 412.5, "width": 481.89, "height": 15 }] },
    {
      "id": "items", "tag": "table",
      "boxes": [
        { "page": 1, "x": 56.69, "y": 120, "width": 481.89, "height": 665.2 },
        { "page": 2, "x": 56.69, "y": 56.69, "width": 481.89, "height": 340.5 }
      ],
      "rows": [
        { "section": "header", "index": 0, "boxes": [{ "page": 1, "x": 56.69, "y": 120, "width": 481.89, "height": 18 }, { "page": 2, "x": 56.69, "y": 56.69, "width": 481.89, "height": 18 }] },
        { "section": "body", "index": 0, "id": "line-1", "boxes": [{ "page": 1, "x": 56.69, "y": 138, "width": 481.89, "height": 18 }] }
      ]
    }
  ]
}
```

- `pages`: the size of each page box, without bleed and printer's marks, before
  `page-orientation` turns it.
- `elements`: every element with an `id`, in document order. `tag` is its tag name. `boxes` are
  its border boxes, one for each page it is on (and for each line of an inline element), with the
  page number from 1; empty for an element that is not rendered.
- `rows`: only for a table, its rows (`<tr>`) in document order. `section` is `header`, `body` or
  `footer`, `index` the row's place among the rows of that section (from 0), `id` its `id` if it
  has one; a row's box on a page is the area of its cells there.

`rss` and `maxRss` are the worker's resident memory after the document, in bytes: now and the
largest it has been since the worker started. On Linux the worker sends both (from
`/proc/self/status`), on macOS and FreeBSD only `maxRss` (from `getrusage`), on Windows neither. A caller can
replace a worker that has grown by closing its input and starting a new one, as the PHP package
does with `maxMemory` ([deployment.md](deployment.md#restart-workers)).

The header of `E` for a render request has the same `rss` and `maxRss`, the `message` and a
`diagnostics` array of the same form. For a document that exceeded its memory budget or time
limit, it holds one diagnostic, `limit.memory` or `limit.time`. For a document that could not be rendered (it does not
meet PDF/A or PDF/UA, or the PDF could not be written), it holds the diagnostics found until
then, which often explain the failure; for a request the worker cannot use, an `F` it cannot read
and a message of an unknown type, it is empty.

### Signatures

The `signature` of `R` signs the PDF (PAdES, see [pdf.md](pdf.md#sign-the-pdf)), either with a
PKCS#12 file or with a key the caller holds:

```json
{
  "pkcs12": 5,
  "password": "secret",
  "name": "Rechnungsstelle",
  "reason": "Rechnung",
  "location": "Berlin",
  "contactInfo": "rechnung@example.com",
  "time": "2026-11-02T12:00:00Z",
  "timestamp": false,
  "certify": "fill-forms",
  "element": "signature",
  "revocation": false,
  "archiveTimestamp": false
}
```

- `pkcs12`, `password`: the data block of a PKCS#12 file (`.p12`, `.pfx`) and its password
  (empty if left out).
- `certificates`: instead of `pkcs12`, the data blocks of the signer's certificate and those of
  its chain, signer first, each DER or PEM. The worker then asks the caller to sign with `S`.
- `name`, `reason`, `location`, `contactInfo`: what the signature says; may be left out.
- `time`: the time of signing, in the form of `created`; the creation date if left out.
- `timestamp`: whether the worker asks the caller for a timestamp with `T` (`false` if left
  out).
- `certify`: makes the signature a certification signature (DocMDP) that allows `no-changes`,
  `fill-forms` or `annotate`; left out, an approval signature.
- `element`: the `id` of the element in whose box the signature shows, with the element's
  content as its appearance. `area` instead: `{ "page": 1, "x": 340, "y": 700, "width": 200,
  "height": 60 }`, the number of the page (from 1) and an area in points from its top left
  corner. Without either, the signature is invisible.
- `revocation`: whether the worker asks the caller for the revocation status of the
  certificates with `V`, for PAdES B-LT (`false` if left out).
- `archiveTimestamp`: whether a document timestamp follows the validation data (B-LTA), asked
  for with `T`; needs `timestamp` and `revocation`.

`S` carries the request's `id`, the `algorithm` (`RSA-SHA256` or `ECDSA-P256-SHA256`, from the
certificate) and the SHA-256 digest (32 bytes) to sign as its data. The caller answers `s` with
the signature of that digest as its data: PKCS#1 v1.5 for RSA, DER or the 64 bytes of r and s for
ECDSA. `T` carries the `id` and the SHA-256 digest of the signature value; the caller answers `t`
with the answer of an RFC 3161 time-stamping authority (the timestamp token or the whole
response) as its data. A caller that cannot answer sends `s` or `t` with an `error` and no data;
the worker then answers the request with `E`. The worker checks signatures against the
certificate and timestamps against the digest, and answers `E` for those that do not match.
It may ask more than once for one document, should a timestamp need more room than expected.
An archive timestamp is asked for with `T` as well, after the validation data, with the SHA-256
digest of the file.

`V` comes once for each certificate of the signature and of its timestamp but the roots. It
carries the `id`, the URLs of the OCSP responders (`ocspUrls`) and of the CRLs (`crlUrls`) that
the certificate names, and three data blocks: the certificate, the certificate of its issuer
(both DER) and the OCSP request for it (DER, to POST to a responder with the content type
`application/ocsp-request`). The caller answers `v` with what the responder or the CRL
distribution point answered as its data blocks: OCSP responses and CRLs (DER), any number, and
certificates the check needs; no data blocks where the status cannot be had. An `error` makes
the worker answer the request with `E`, as does data that is neither.

A request with both `signature` and `encryption` signs the encrypted PDF. `signature`, `S`, `s`,
`T` and `t` were added in 1.0.0: an older worker ignores `signature` and writes the PDF unsigned,
which the caller notices by the missing `signed` in `P`.

### Fonts

`F` adds a font to the worker for all later documents; the worker answers with `K` and the number
of faces it found, or `E` (without `id`). Fonts can also be given on the command line
(`fagus worker --font-dir fonts`, and `--system-fonts` for the fonts installed on the machine) or
come with a document through `@font-face`. `--sans-serif`, `--serif` and `--monospace` name the
families of the generic families.

### Language of form controls

`fagus worker --locale de-DE` sets the language of the texts form controls show of their own
(button labels, the placeholders and formats of date fields) in documents that give a control no
`lang`; without it they are American English, as in Chrome by default. Text without `lang` is
shaped in that language as well, as Chrome shapes it in the language of its user interface.

## Versions

`protocol` changes only with a change that breaks callers or workers, which comes only in a major
release ([stability](stability.md)). Within one protocol, the two sides stay compatible in both
directions: a worker ignores fields of a request it does not know, and a caller ignores fields of
an answer it does not know. A worker of an older release therefore renders a document without
the options that are newer than itself, without a word.

A caller should therefore:

- stop when `protocol` is not the one it speaks, and say so;
- compare `version` with the release it was written for, and warn when they differ.

The PHP package does both: a binary of another protocol is refused with an `EngineException`
that names the binary, both versions and how to install the binary of the package's release; a
binary of another release with the same protocol renders, and the engine warns once per process,
through its `onWarning` callback or to PHP's error log ([PHP API](php.md#constructor)).

The fields that came after protocol 1 began, with the release that added them; a binary older
than that ignores them:

| Field | Message | Since |
|---|---|---|
| `facturX.version`, `facturX.description` | `R` | 0.2.0 |
| `layout` | `R`, `P` | 1.0.0 |
| `forms` | `R`, `P` | 1.0.0 |
| `encryption` | `R` | 1.0.0 |
| `encrypted` | `P` | 1.0.0 |
| `signature`; the messages `S`, `s`, `T`, `t` | `R` | 1.0.0 |
| `signature.certify`, `element`, `area`, `revocation`, `archiveTimestamp`; the messages `V`, `v` | `R` | 1.0.0 |
| `signed` | `P` | 1.0.0 |
| `rss`, `maxRss` | `P`, `E` | 1.0.0 |
| `sandbox` | `H` | 1.0.0 |
| `memoryBudget`, `timeLimit` | `R` | 1.0.0 |
