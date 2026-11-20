# Command line

The `fagus` binary renders HTML files to PDF on the command line. It is the same binary the PHP
package runs as its worker, so it renders exactly as the PHP package does. Use it to try a
template, in shell scripts and build jobs, or from languages other than PHP.

```sh
fagus render invoice.html -o invoice.pdf
```

## Get the binary

Composer installs the binary of the package's release into the package's `bin` directory
(`vendor/nforst/fagus/php/fagus/bin/fagus`, `fagus.exe` on Windows); see
[getting-started.md](getting-started.md). A checkout of the repository builds it with
`cargo build --release` as `target/release/fagus`.

```sh
fagus --version     # fagus 0.2.0
fagus --help
fagus render --help
```

## Render an HTML file

```sh
fagus render <INPUT> --output <OUTPUT> [OPTIONS]
```

`fagus render` reads the HTML file `INPUT` and writes the PDF to `OUTPUT`. Relative URLs in the
document (style sheets, images, fonts) resolve against the directory of the input file, and files
are only read from inside that directory, or from inside `--root`. Nothing is loaded from the
network.

A file ending in `.xhtml` or `.xht` is parsed as XML, as browsers parse XHTML files; any other
file as HTML.

| Option | Default | Description |
|---|---|---|
| `-o`, `--output <FILE>` | required | The PDF file to write |
| `--font-dir <DIR>` | none | A directory with font files (TTF, OTF, TTC, OTC, WOFF, WOFF2), read with its subdirectories; repeatable |
| `--system-fonts` | off | Use the fonts installed on the machine too, as Chrome does; the output then depends on the machine |
| `--locale <TAG>` | none | The language of the texts form controls show (button labels, date formats) where the document gives no `lang`, e.g. `de-DE`; without it they are American English. Text without `lang` is shaped in it, and it is the PDF's language when `<html>` has no `lang` |
| `--sans-serif <FAMILY>` | none | The family used for `sans-serif` |
| `--serif <FAMILY>` | none | The family used for `serif` |
| `--monospace <FAMILY>` | none | The family used for `monospace` |
| `--root <DIR>` | the input's directory | A larger directory the document may read files from, which contains the input's directory; URLs starting with `/` resolve against it |
| `--pdfa <LEVEL>` | none | Write PDF/A: `1b`, `2b`, `2u`, `2a`, `3b`, `3u`, `3a`, `4`, `4e` or `4f` |
| `--untagged` | off | Write a PDF without tags (the logical structure screen readers read) |
| `--accessible` | off | Write PDF/UA-1; needs a title, a language and alternative text for images |
| `--forms` | off | Make the form controls fields of an interactive form, which readers fill in ([pdf.md](pdf.md#fill-in-forms)) |
| `--attach <FILE>` | none | Attach a file; repeatable |
| `--factur-x <FILE>` | none | Make a ZUGFeRD / Factur-X invoice with this XML |
| `--profile <PROFILE>` | the XML's | The invoice profile: `minimum`, `basicwl`, `basic`, `en16931`, `extended` or `xrechnung` |
| `--factur-x-version <VERSION>` | `1.0` | The `Version` of the Factur-X XMP metadata |
| `--factur-x-description <TEXT>` | `Factur-X/ZUGFeRD invoice` | The description of the attached invoice XML |
| `--user-password <PASSWORD>` | none | Encrypt the PDF (AES-256) with the password needed to open it; also from `FAGUS_USER_PASSWORD` |
| `--user-password-file <FILE>` | none | Read the user password from the first line of a file |
| `--owner-password <PASSWORD>` | none | Encrypt the PDF with the password that lifts the restrictions of `--permissions`; also from `FAGUS_OWNER_PASSWORD` |
| `--owner-password-file <FILE>` | none | Read the owner password from the first line of a file |
| `--permissions <LIST>` | everything | What an encrypted PDF allows without the owner password, separated by commas: `print`, `print-low-quality`, `modify`, `copy`, `annotate`, `fill-forms`, `assemble`, or `none` |
| `--created <DATE>` | now | The creation date, e.g. `2026-11-13T12:00:00Z` or `2026-11-13T14:00:00+02:00`, for reproducible files |
| `--sign-p12 <FILE>` | none | Sign the PDF (PAdES) with the key and certificates of this PKCS#12 file; the password comes from `FAGUS_SIGN_PASSWORD` or `--sign-password-file` |
| `--sign-password-file <FILE>` | none | A file whose first line is the password of the PKCS#12 file |
| `--sign-name <NAME>` | none | The name of the signer, as the signature shows it |
| `--sign-reason <TEXT>` | none | Why the document is signed |
| `--sign-location <TEXT>` | none | Where the document is signed |
| `--sign-contact <TEXT>` | none | How to reach the signer |
| `--sign-certify <CHANGES>` | none | Certify the PDF: the signature allows `no-changes`, `fill-forms` or `annotate` later |
| `--sign-element <ID>` | none | Show the signature in the box of the element with this id, with the element's content as its appearance |
| `--strict` | off | Fail on any diagnostic (not only warnings), without writing the PDF |
| `--layout-json <FILE>` | none | Write where the elements with an `id` ended up (pages, boxes, the rows of tables) to this file as JSON |
| `--timings` | off | Print the page count and the time of each phase |
| `--repeat <N>` | `1` | Render N times and report the median time (for benchmarks) |
| `--memory-budget <MIB>` | `2048` | The [memory budget](#limits) of the document: one that needs more fails with `limit.memory`; `0` for none |
| `--time-limit <SECONDS>` | none | The [time limit](#limits) of the document, such as `30` or `2.5`: one that takes longer fails with `limit.time` |
| `--no-sandbox` | off | Render outside the [sandbox](#sandbox) (Linux only; there is none elsewhere) |
| `--memory-limit <MIB>` | none | End the process when its memory (address space) would exceed this many MiB (Linux only) |
| `--cpu-limit <SECONDS>` | none | End the process after this many seconds of processor time (Linux only) |
| `-h`, `--help` | | Print help |

The options mean what the methods of the PHP package mean; [pdf.md](pdf.md) describes PDF/A,
PDF/UA, attachments, invoices, signatures and reproducible output in detail.

Some details of the command line:

- `--attach` attaches the file under its file name, with the file name as description and the
  relationship `Supplement`. The MIME type comes from the extension: `xml` (`text/xml`), `pdf`,
  `csv`, `json`, `txt`, `png`, `jpg` and `jpeg`; any other file is `application/octet-stream`.
- `--profile`, `--factur-x-version` and `--factur-x-description` only apply together with
  `--factur-x`.
- `--created` reads the date, the time and an offset from UTC (`Z`, `+01:00`, `-0530`) and
  writes the date in UTC: `2026-11-13T12:00:00+02:00` becomes `2026-11-13T10:00:00Z`. A date
  without an offset is taken as UTC; a fraction of a second is dropped. Anything else after the
  seconds makes the date invalid.
- `--untagged` has no effect with `--accessible` or with `--pdfa 2a` and `3a`, which need tags.
- With `--repeat`, the PDF of the last run is written.
- `--sign-p12` takes the password from the environment variable `FAGUS_SIGN_PASSWORD`, or from
  the first line of `--sign-password-file`, never from the command line, where other users of
  the machine could see it. The signature's time is the creation date (`--created`, or now).
  `--sign-name`, `--sign-reason`, `--sign-location`, `--sign-contact`, `--sign-certify`,
  `--sign-element` and `--sign-password-file` need `--sign-p12`. With `--user-password` or
  `--owner-password`, the encrypted PDF is signed. The command line does not timestamp
  signatures and adds no validation data ([pdf.md](pdf.md#sign-the-pdf)).
- `--layout-json` writes the same JSON as the worker's `layout` ([protocol.md](protocol.md#layout)):

  ```sh
  fagus render invoice.html -o invoice.pdf --layout-json layout.json
  jq '.elements[] | select(.id == "total") | .boxes' layout.json
  ```

  It is written after the PDF, and not where `--strict` fails.

## Protect the PDF with a password

A user password, an owner password or both encrypt the PDF with AES-256 (see
[pdf.md](pdf.md#protect-the-pdf-with-a-password)). The user password is needed to open it; the
owner password lifts the restrictions of `--permissions`. Without a user password, the PDF opens
for everyone with only the permissions given; without an owner password, nobody can lift them.

```sh
# needs "open sesame" to open; may be printed and copied from, nothing else
fagus render salary.html -o salary.pdf \
  --user-password-file secrets/user.txt --owner-password-file secrets/owner.txt \
  --permissions print,copy

# opens for everyone, but cannot be printed, copied from or changed
FAGUS_OWNER_PASSWORD=change-me fagus render terms.html -o terms.pdf --permissions none
```

Other users of a machine can see the arguments of a process (`ps`), so a password in
`--user-password` or `--owner-password` is visible to them while Fagus runs. The file options
read the first line of a file (without its line break), and the environment variables
`FAGUS_USER_PASSWORD` and `FAGUS_OWNER_PASSWORD` stand in for the options; `--help` does not show
their values. A file option and the option it reads for cannot be given together; either wins
over the environment variable. An empty password counts as none;
`--permissions` without a password is an error.

An encrypted PDF differs from run to run, also with `--created`: its keys are random. PDF/A and
Factur-X cannot be encrypted: `--pdfa` or `--factur-x` with a password fail with exit code 1.
`--accessible` goes with a password: PDF/UA allows encryption, and assistive technology may
always extract the text. Passwords are prepared with SASLprep, as PDF 2.0 requires (see
[pdf.md](pdf.md#protect-the-pdf-with-a-password)); one with a control character fails with exit
code 1.

## Read and write

The input is a file path; `fagus render` does not read from standard input. Write the HTML to a
file first, or use the worker or the PHP package.

On Linux and macOS, the PDF can go to standard output through `/dev/stdout`:

```sh
fagus render invoice.html -o /dev/stdout | aws s3 cp - s3://invoices/2026-0042.pdf
```

Diagnostics and timings go to standard error, one per line:

```text
<style> 1:12:3: warning[css.unknown-property]: unknown property `colr`; the declaration is ignored
resources: warning[resource.not-loaded]: image `logo.png` could not be loaded
```

Each line has the source (a file name, `<style> 1` for the first `<style>` element, `resources`,
`pdf`), the line and column where known, the severity, a stable code and the message. The page
is written in spite of warnings unless `--strict` is given, which fails on any diagnostic
("1 diagnostic in strict mode"). When a document is refused (PDF/A, PDF/UA), the diagnostics found
before are printed too, ahead of the error.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The PDF was written, or help or the version was printed |
| `1` | Rendering failed: the input or a font directory could not be read, the document was refused (PDF/A, PDF/UA), it exceeded its memory budget or time limit, `--strict` found diagnostics, `--created` is not a date, the PKCS#12 file or its password is wrong, or the PDF could not be written or signed. The reason goes to standard error, after `Error:` |
| `2` | The command line is wrong: an unknown option, a missing `--output` or input, a value that is not allowed (`--pdfa 5b`) |

```text
$ fagus render letter.html -o letter.pdf --accessible
Error: the document does not meet PDF/UA-1: the document has no title (add a <title>); the document has no language (add `lang` to <html>)
$ echo $?
1
```

## Examples

A PDF with fonts of your own:

```sh
fagus render invoice.html -o invoice.pdf --font-dir resources/fonts
```

A document whose style sheets and images lie in a parent directory (`../css/print.css`):

```sh
fagus render resources/pdf/invoice.html -o invoice.pdf --root resources
```

An archivable PDF with a file attached:

```sh
fagus render report.html -o report.pdf --pdfa 3b --attach data.csv
```

A ZUGFeRD / Factur-X invoice that is also accessible, with a fixed date:

```sh
fagus render invoice.html -o invoice.pdf \
  --factur-x invoice.xml --pdfa 3a --accessible \
  --created 2026-11-01T08:00:00Z
```

A signed invoice, with the password of the PKCS#12 file in the environment:

```sh
FAGUS_SIGN_PASSWORD="$(cat /run/secrets/signing-password)" \
  fagus render invoice.html -o invoice.pdf --pdfa 3b \
  --sign-p12 /run/secrets/signing.p12 --sign-reason Rechnung --sign-location Berlin
```

A check in a build job that fails on any ignored CSS or missing resource:

```sh
fagus render template.html -o /tmp/template.pdf --strict
```

How long each phase takes, as the median of 20 runs:

```sh
$ fagus render invoice.html -o invoice.pdf --timings --repeat 20
1 pages, median 1.77 ms (parse 0.03, style 0.79, layout 0.24, paint 0.02, pdf 0.49)
```

The phase times are those of the last run. Each `fagus render` starts a new process that loads
its fonts first; the PHP package keeps one worker running, which is faster when you render many
documents.

## Threads

The binary compresses the PDF on two threads while it draws the pages. The environment variable
`RAYON_NUM_THREADS` sets another number:

```sh
RAYON_NUM_THREADS=1 fagus render invoice.html -o invoice.pdf
```

## The worker

```sh
fagus worker [OPTIONS]
```

`fagus worker` renders documents for another program: it reads requests from standard input and
writes PDFs to standard output, in the binary frames of [protocol.md](protocol.md), until its
input closes. The PHP package starts it and talks to it; it is not meant to be used by hand. Write
a client of your own only to use Fagus from another language.

| Option | Default | Description |
|---|---|---|
| `--font-dir <DIR>` | none | A directory with font files, as for `render`; repeatable |
| `--system-fonts` | off | Use the fonts installed on the machine too |
| `--locale <TAG>` | none | The language of the texts of form controls, as for `render` |
| `--sans-serif <FAMILY>` | none | The family used for `sans-serif` |
| `--serif <FAMILY>` | none | The family used for `serif` |
| `--monospace <FAMILY>` | none | The family used for `monospace` |
| `--allow-read <DIR>` | every file | Let the [sandbox](#sandbox) read only the files in this directory, besides the fonts; repeatable (Linux only) |
| `--memory-budget <MIB>` | `2048` | The [memory budget](#limits) of each document, unless a request sets its own (`memoryBudget`); `0` for none |
| `--time-limit <SECONDS>` | none | The [time limit](#limits) of each document, unless a request sets its own (`timeLimit`) |
| `--no-sandbox` | off | Render outside the sandbox, as for `render` |
| `--memory-limit <MIB>` | none | End the worker when its memory would exceed this many MiB, as for `render` |
| `--cpu-limit <SECONDS>` | none | End the worker after this many seconds of processor time in all, as for `render` |

Everything else (PDF/A, attachments, resources, the creation date, passwords) comes with each
request; the worker does not read `FAGUS_USER_PASSWORD` and `FAGUS_OWNER_PASSWORD`. The
worker ends with exit code 0 when its input closes, and with 1 when it reads a frame that breaks
the protocol.

## Limits

Each part of a document has a limit of its own (pixels per image, pages, nesting; see
[SECURITY.md](../SECURITY.md#limits)). The memory budget and the time limit bound the document
as a whole: a document that needs more ends with an error and a diagnostic, without a PDF, and
the worker goes on with the next one.

```text
$ fagus render report.html -o report.pdf --memory-budget 512
document: error[limit.memory]: the document needs more memory than its budget of 512 MiB (styles 433 MiB, document tree 68 MiB)
Error: the document needs more memory than its budget of 512 MiB (styles 433 MiB, document tree 68 MiB)
$ fagus render report.html -o report.pdf --time-limit 10
document: error[limit.time]: the document took longer than its time limit of 10.0 s
Error: the document took longer than its time limit of 10.0 s
```

- **`--memory-budget`** (2048 MiB unless set) counts what the large parts of a document hold, as
  an estimate, not as what the allocator counts: the document tree, computed styles, layout boxes
  and fragments, table cells (with the rows each one spans), display lists, decoded images (four
  bytes a pixel: held until the PDF is written for the BMP, TIFF and filtered images that Fagus
  decodes itself, while other images only need that much room, since they are decoded one after
  another as the PDF is written), the pixels rasterized for filters, shadows and masks while
  they are drawn, the fonts of `@font-face` (unpacked) and the bytes of resources (a file larger
  than what is left is not read). Parts are counted while they are
  held: a layout that is done again replaces the one before. The process needs more than the
  budget: the engine's fonts and caches and what the allocator keeps, so leave room above it
  (or set `--memory-limit` higher still, as a last resort).
- **`--time-limit`** is wall-clock time from the start of the render, including the time a
  worker waits for the caller's resources (`?`). The engine looks at the clock for each element,
  box, line, page, image and resource, so a document ends shortly after its time is up; parts
  that the engine cannot interrupt (parsing the markup, compressing the PDF) finish first.

Neither changes the output: a document that stays within both renders to the same bytes as
without them. A resource is held once per document however many URLs it has
(`logo.png?v=1`, `logo.png?v=2`, …): the same content is decoded and embedded once, and the same
file is read once.

`--memory-limit` and `--cpu-limit` below are limits of the process, which the system enforces
by ending it; the budget and the time limit end one document and keep the process.

## Sandbox

On Linux, `fagus render` and `fagus worker` render in a sandbox. Rendering needs no network,
starts no programs and writes no files of its own, so once the input, the attachments, the
password files and the fonts are read and the output files (the PDF and the `--layout-json`) are
open, the process gives up all of that, as far as the kernel allows:

- **Files** ([Landlock](https://docs.kernel.org/userspace-api/landlock.html), Linux 5.13 and
  later): `fagus render` may read only the files of `--root` (or the input's directory) and the
  directories of the system fonts. `fagus worker` may read every file, since each request names
  its own `base` and `root`; `--allow-read` limits it to the directories given (and the system
  fonts, and `/proc/self/status` for the memory it reports), and a document's files elsewhere
  are then not found (`resource.not-loaded`). Neither may write, create, remove (but for an
  output file it created, if rendering fails) or execute files (nor truncate them, from Linux
  6.2 on). From Linux 6.7 on, TCP is refused as well; from 6.12 on, connecting to abstract Unix
  sockets and signalling other processes.
- **System calls** (seccomp): the calls of networking (`socket`, `connect`, …), of starting
  programs and processes (`execve`, `fork`, `clone` without a thread), of reading other
  processes (`ptrace`, `process_vm_readv`) and of kernel interfaces that rendering never uses
  (`io_uring`, `bpf`, `mount`, `unshare`, `keyctl`, modules, …) fail with `EPERM`. This is a list
  of what is refused, not of what is allowed, so that it holds with glibc, musl and new Rust
  releases alike.

The sandbox only guards in depth: the engine reads no files outside the root and has no network
code of its own. It is a best effort. A kernel without Landlock (older than 5.13, or with
Landlock not enabled at boot) or a container that refuses the calls renders without that part;
`FAGUS_LOG=debug` prints which parts are in force to standard error, and the worker says so in
its `H` frame ([protocol.md](protocol.md#messages)). On macOS, FreeBSD and Windows there is no
sandbox, and `--no-sandbox` changes nothing.

`--memory-limit` and `--cpu-limit` set the resource limits of the process (`RLIMIT_AS`,
`RLIMIT_CPU`), with or without the sandbox, and only on Linux; elsewhere they are an error. The
memory limit counts the address space, which is larger than the memory in use: leave room, at
least 1024 MiB. The processor time of a worker adds up over all its documents; the PHP package
starts a new worker when one has ended.

To render without the sandbox, for example to debug a document that reads files through a
symbolic link out of its root, give `--no-sandbox` (in PHP, `new Engine(sandbox: false)`).
