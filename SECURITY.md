# Security policy

Fagus renders HTML and CSS that may come from untrusted sources. Security reports are taken seriously.

## Supported versions

Security fixes go into the latest minor version, as a patch release. Once a new major version is out, the last minor version of the previous major gets security fixes for six more months; older minor versions get none, since an update within a major needs no changes ([stability](docs/stability.md)). Versions before 1.0 get no security fixes: [UPGRADING.md](UPGRADING.md) covers the update.

| Version | Security fixes |
|---|---|
| 1.1.x | Yes |
| 1.0.x | No (update to 1.1) |
| < 1.0 | No (update to 1.1) |

After a new major version, the table reads like this, for example after 2.0.0 and 2.1.0:

| Version | Security fixes |
|---|---|
| 2.1.x | Yes |
| 2.0.x | No (update to 2.1) |
| 1.9.x, the last 1.x | For six months after the release of 2.0.0 |
| older | No |

## Reporting a vulnerability

Please do not open a public issue. Report it privately through GitHub: [report a vulnerability](https://github.com/nforst/fagus/security/advisories/new) (the *Security* tab of the repository). If you cannot use GitHub, send the report to kontakt@niklasforst.de.

Include a description, the affected version and, if possible, a document that reproduces the problem. You will get an answer within a week. Once a fix is released, the report is published as a security advisory, crediting you if you wish.

## Scope

In scope: code execution, reading files or network resources that the caller did not allow, crashes and unbounded resource use caused by input documents, and incorrect output in PDF/A or PDF/UA mode that a validator rejects.

## Threat model

Fagus is built to render documents whose HTML, CSS, fonts and images come from people you do not
trust, such as invoice templates of customers or user content. The caller, the program that
starts the binary or the worker and hands it documents, is trusted: it chooses the root
directory, the fonts, the options and the resources.

What Fagus promises for untrusted documents:

- **No network.** The engine has no network code. Resources come from `data:` URLs, from the
  caller (`resources`, a loader, the worker's `?` messages) or from files inside the root.
- **No files outside the root.** Relative URLs resolve against the base directory and are read
  only if they lie inside the root after resolving `..`, percent-encoding and symbolic links.
  URLs with a scheme or a host, absolute paths, drives and network shares are refused. Images
  inside SVG documents must be `data:` URLs.
- **No code execution.** Fagus runs no scripts, and its own code has no `unsafe` but one call of
  glibc's `malloc_trim` in the command line, which takes no arguments from the document (the crates
  it depends on do have `unsafe`).
- **Bounded work.** The limits below keep a document from taking unbounded memory or time.
  Where the limit of a part is reached, the part is left out (often with a diagnostic) and the
  rest renders. Where the memory budget or the time limit of the whole document is reached, the
  document ends with an error (`limit.memory`, `limit.time`) and the engine goes on with the
  next one.

On Linux, the binary renders in a [sandbox](docs/cli.md#sandbox) (Landlock and seccomp) that
backs the first two up: should a bug in Fagus or in a crate it uses be exploited, the process
still cannot open sockets, start programs, write files or (with `fagus render`, or a worker with
`--allow-read`) read files outside its directories. There is no sandbox on macOS, FreeBSD and Windows.

### Limits

| What | Limit |
|---|---|
| Nesting of elements | 256 levels laid out; deeper elements are left out (parsing stops at 512) |
| Nesting of CSS rules, `var()` | 256 levels; `var()` 32 levels and 65,536 tokens per value |
| Pages | 10,000 per document |
| Grid tracks, columns | 1,000 tracks per axis and 10,000 rows; 1,000 columns |
| Table spans | `colspan` 1,000, `rowspan` 65,534 |
| Frames (`<iframe>`, `<object>`) | 8 levels, 256 frames, 8 MiB of markup |
| `@import` | 8 levels |
| Raster images | 200 million pixels each |
| PDF images | 128 MiB of data each, and 128 MiB of page content once decoded (content streams compressed other than with Flate: 32 KiB compressed) |
| SVG images | 128 MiB of data each (also after inflating SVGZ), 1 million elements after `<use>` (usvg), entities that add at most 16 MiB of text and hold no markup, 64 primitives per filter, SVG images in SVG images 3 levels deep |
| Filters, shadows, masks | rasterized at most 12 million pixels per region; blurs at most 25 million |
| Fonts | 64 MiB unpacked from WOFF or WOFF2 (the sizes the font declares are checked before unpacking); 256 MiB of document fonts kept per engine |
| Worker frames | 1 GiB |
| Memory of a document | 2 GiB by default ([`--memory-budget`, `memoryBudget`](docs/cli.md#limits)), an estimate of what its large parts hold at once: the document tree, computed styles, layout boxes and fragments, table cells with the rows they span, display lists, decoded images (4 bytes a pixel, held or needed at once), pixels rasterized for filters, shadows and masks, `@font-face` fonts unpacked, and resource bytes. A file larger than what is left is not read |
| Time of a document | none by default for the binary and Rust, a tenth less than `timeout` in PHP ([`--time-limit`, `timeLimit`](docs/cli.md#limits)); wall-clock time, checked for each element, box, line, page, image and resource |
| Resources | each content once per document, however many URLs name it; each file read once |

### Remaining risks

These are known and not fixed in the engine; deploy accordingly:

- **The budget is an estimate.** The memory budget counts the large parts of a document, not
  what the process takes: the engine's own fonts and caches, smaller allocations and what the
  allocator keeps come on top, and a few steps run to their end before the clock is looked at
  again (parsing the markup, writing and compressing the PDF). Set the budget to what the
  process can spare, and still give the process limits of its own: `--memory-limit` and
  `--cpu-limit` on Linux, a container's memory limit, the PHP engine's `timeout`. A worker that
  runs out of memory is ended by the system; the PHP package starts a new one for the next
  document.
- **Everything inside the root can be read.** A document may show any file in its root in the PDF
  (as text with `<object type="text/plain">`, as an image, as a font). Make the root a directory
  of assets only, never one with configuration or secrets (`.env`), and never a user's home or
  `/`. The base directory is the root when no root is given.
- **The caller is trusted.** The worker believes the caller's `base`, `root` and data, and a
  frame may hold up to 1 GiB. Do not expose the worker's input to untrusted parties.
- **Third-party parsers.** HTML, CSS, fonts, images, SVG and PDF files shown as images are parsed
  by Rust crates (html5ever, lightningcss, skrifa, HarfRust, png, zune-jpeg, image-webp, gif,
  rav1d, usvg, resvg, krilla, hayro-syntax, wuff); a bug in one of them is a bug in Fagus.
  hayro-syntax reads the cross references, objects and page tree of a PDF image in full as it
  opens the file, before the limits of the page are checked; the file's size bounds that work. rav1d, which decodes the AV1 pictures of AVIF
  images, is a port of dav1d's C code and has unsafe code; Fagus reads the container of AVIF
  images itself. They are fuzzed through the targets in `fuzz/`.
- **Without the sandbox** (macOS, Windows, Linux without Landlock or seccomp, `--no-sandbox`),
  only the checks of the engine stand between a document and the file system.

## Verify a binary

The release workflow builds the binaries on GitHub Actions and attests each of them: GitHub
signs, through Sigstore and without a key of ours, which workflow run of which commit built a
file with a given SHA-256. Each release also holds `SHA256SUMS` and a CycloneDX SBOM of every
binary (`fagus-<target>.cdx.json`), which lists the crates it is built from.

```sh
gh release download v1.0.0 -p fagus-x86_64-unknown-linux-gnu -p SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify fagus-x86_64-unknown-linux-gnu --repo nforst/fagus \
  --signer-workflow nforst/fagus/.github/workflows/release.yml
```

The Composer package goes further: it installs only the binary of its own release whose SHA-256
it lists (`php/fagus/src/Internal/Release.php`), and those sums are written after the
attestations were checked ([releasing.md](docs/releasing.md)).
