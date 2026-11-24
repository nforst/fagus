# C library (experimental)

`crates/fagus-ffi` builds Fagus as a C library: a shared library (`libfagus_ffi.so`,
`libfagus_ffi.dylib`, `fagus_ffi.dll`) and a static one, with the header
[`fagus.h`](https://github.com/nforst/fagus/blob/main/crates/fagus-ffi/include/fagus.h). It renders
documents in the caller's process, for languages that call C through FFI, instead of the
[worker process](protocol.md).

**Experimental.** The library is not part of the [stable API](stability.md): its functions and
options may change in any release, also a minor or a patch release, and `fagus_abi()` returns 0
while that is so. The PHP package does not use it: for PHP, the package and its worker stay the
supported way, and once the worker runs, it renders as fast as the library ([Speed](#speed)).
Why the library is experimental, and what it leaves out, is in
[ADR 8](adr/0008-c-library-experimental.md).

## Get it

Each [release](https://github.com/nforst/fagus/releases) since 1.0.0 has the library as
`fagus-ffi-<target>.tar.gz` for Linux (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
glibc 2.17 or newer), macOS (`x86_64-apple-darwin`, `aarch64-apple-darwin`) and Windows
(`x86_64-pc-windows-msvc`), with the header `fagus.h` and the licenses. Its SHA-256 is in
`SHA256SUMS`, and `gh attestation verify fagus-ffi-<target>.tar.gz --repo nforst/fagus` checks
that the release workflow built it. There is none for musl (Alpine): build it there.

To build it yourself, from a checkout of the release:

```sh
cargo build --release -p fagus-ffi
```

This needs Rust 1.97 or newer and writes `target/release/libfagus_ffi.so` (`.dylib` on macOS,
`fagus_ffi.dll` with `fagus_ffi.dll.lib` on Windows) and the static `libfagus_ffi.a`
(`fagus_ffi.lib`). On musl, build the shared library with
`RUSTFLAGS="-C target-feature=-crt-static"`. The header is `crates/fagus-ffi/include/fagus.h`.

## Render a document

A document is the render request of the [worker protocol](protocol.md#rendering): the JSON header
of `R`, with `blobs` for the lengths of its data blocks, and the data blocks one after another.
The answer is the header of `P` or `E` as JSON, and the PDF. So the options are those of the
protocol, with the same names and defaults.

```c
#include <stdio.h>
#include <string.h>
#include "fagus.h"

int main(void) {
    char *error = NULL;
    FagusEngine *engine = fagus_engine_new(NULL, 0, &error);   /* the default options */
    if (engine == NULL) {
        fprintf(stderr, "%s\n", error);
        fagus_string_free(error);
        return 1;
    }
    const char *html = "<h1>Rechnung</h1>";
    char request[256];
    snprintf(request, sizeof request,
             "{\"id\":1,\"html\":0,\"base\":\"/app/templates\",\"blobs\":[%zu]}", strlen(html));
    FagusResult *result = fagus_render(engine, request, strlen(request), html, strlen(html));
    size_t length;
    if (fagus_result_ok(result)) {
        const uint8_t *pdf = fagus_result_pdf(result, &length);
        FILE *file = fopen("invoice.pdf", "wb");
        fwrite(pdf, 1, length, file);
        fclose(file);
    } else {
        /* {"id":1,"message":"…","diagnostics":[…]} */
        fprintf(stderr, "%s\n", fagus_result_header(result, &length));
    }
    fagus_result_free(result);
    fagus_engine_free(engine);
    return 0;
}
```

Link it with `-lfagus_ffi`. An engine keeps its fonts and caches for every document, as a worker
does, so make one and keep it. It may render documents on several threads at once.

| Function | Does |
|---|---|
| `fagus_version()` | The version of Fagus, such as `1.0.0` |
| `fagus_abi()` | The version of this interface: 0 while it is experimental |
| `fagus_protocol()` | The worker protocol whose requests it takes: 1 |
| `fagus_engine_new(options, len, &error)` | A new engine, or NULL and a message in `error` |
| `fagus_engine_free(engine)` | Ends an engine that renders nothing at the time |
| `fagus_render(engine, request, len, data, len)` | Renders a request; never NULL |
| `fagus_result_ok(result)` | 1 for a PDF, 0 for an error |
| `fagus_result_header(result, &len)` | The header of `P` or `E`: JSON, ending with NUL |
| `fagus_result_pdf(result, &len)` | The PDF, or NULL for an error |
| `fagus_result_free(result)` | Gives the answer back, with its header and PDF |
| `fagus_string_free(error)` | Gives a message of `fagus_engine_new` back |

Every function checks its pointers for NULL and catches panics, which come back as errors
(`internal error: …`); none of them ends the process on bad input. Memory the library hands out
is given back with the function for it, never with `free()`.

### Options of the engine

`fagus_engine_new` takes JSON with the options of [`fagus worker`](cli.md#the-worker), or NULL
for the defaults. An option it does not know is refused, so that a misspelt one does not go
unnoticed.

| Option | Default | Description |
|---|---|---|
| `fontDirs` | none | Directories with font files |
| `systemFonts` | `false` | Use the fonts installed on the machine too |
| `locale` | none | The language of the texts of form controls, such as `"de-DE"` |
| `sansSerif`, `serif`, `monospace` | none | The families of the generic families |
| `memoryBudget` | 2 GiB | The [memory budget](cli.md#limits) of each document in bytes, unless a request sets its own; `0` for none |
| `timeLimit` | none | The [time limit](cli.md#limits) of each document in milliseconds, unless a request sets its own; `0` for none |

### What the library leaves out

The library asks its caller nothing while it renders. A request with `ask: true`, or with a
`signature` that has `certificates`, `timestamp` or `revocation`, is answered with an error
(`` `ask` needs a caller that answers questions, and there is none ``); `askMany` alone has no
effect. Resources come from the
request's `resources`, as `data:` URLs or as files of `base` and `root`; signatures with a PKCS#12
file work. The answer has no `rss` and `maxRss`: the memory is that of your own process.

## From PHP

PHP's [FFI extension](https://www.php.net/manual/en/book.ffi.php) loads the library. `FFI::cdef()`
takes the declarations of the header without its comments and preprocessor lines;
[`tools/ffi.php`](https://github.com/nforst/fagus/blob/main/tools/ffi.php) does that and calls
every function.

```php
$ffi = FFI::cdef($declarations, '/opt/fagus/libfagus_ffi.so');
$engine = $ffi->fagus_engine_new(null, 0, null);

$request = json_encode(['id' => 1, 'html' => 0, 'base' => '/app/templates', 'blobs' => [strlen($html)]]);
$result = $ffi->fagus_render($engine, $request, strlen($request), $html, strlen($html));
$header = json_decode($ffi->fagus_result_header($result, null), true);   // a const char * is a PHP string
if ($ffi->fagus_result_ok($result) === 1) {
    $length = $ffi->new('size_t');
    $pdf = FFI::string($ffi->fagus_result_pdf($result, FFI::addr($length)), $length->cdata);
}
$ffi->fagus_result_free($result);
```

- PHP's default `ffi.enable=preload` allows `FFI::cdef()` on the command line only. In PHP-FPM and
  other web servers, the declarations must be loaded by the [preload
  script](https://www.php.net/manual/en/opcache.preloading.php) (`opcache.preload`) with
  `FFI::load()` and taken with `FFI::scope()` in the request, or `ffi.enable=1` allows FFI
  everywhere, which PHP advises against. `FFI::load()` reads no `#include` or `#ifndef`: give it
  a file of its own with `#define FFI_SCOPE "fagus"`, `#define FFI_LIB "/opt/fagus/libfagus_ffi.so"`
  and the declarations of `fagus.h`. Docker's official PHP images build the extension only
  with `docker-php-ext-install ffi`.
- Make the engine in the request, or keep it in a long-running process (a queue worker, Octane),
  never in the preload script: PHP-FPM starts its workers by forking the process that preloads,
  and the threads of an engine do not survive that.

## Next to the worker

What the [worker process](deployment.md#worker-processes) gives that a library in your process
cannot:

- **A crash ends only the worker.** A bug that ends the process (a stack overflow, running out
  of memory, a fault in a dependency's unsafe code) ends the PHP-FPM worker or your program
  with the library, and the request with it; with the worker, PHP gets an `EngineException` and
  the next document starts a new one. Panics are caught in both.
- **The [sandbox](cli.md#sandbox).** On Linux, the worker gives up the network, other programs
  and files it does not need. A library cannot do that to the process that loads it.
- **Memory back after large documents.** The worker hands freed memory back to the system after
  each document on Linux, and the PHP package replaces a worker that has grown (`maxMemory`).
  In-process, freed memory stays with your process's allocator until the process ends (for
  PHP-FPM, `pm.max_requests`).
- **Limits of the process.** `--memory-limit` and `--cpu-limit` end a worker, not your process.
  The memory budget and the time limit of each document work in both.
- **One binary, one version.** The PHP package checks that the worker speaks its protocol and
  is of its release. The library is whatever file you load: compare `fagus_version()` with what
  you expect.

The library starts the same threads as a worker, with the first document: one that renders (more
while several of your threads render at once, up to one per processor) and two that compress the
PDF, unless `RAYON_NUM_THREADS` says otherwise.

## Speed

Once a worker runs, the process, the pipe and the copies between PHP and the worker cost little
beside the render. What the library saves is the start of the worker, which PHP-FPM pays in each
request that renders. Measured from PHP on an Apple M2 Pro with the invoices of `tools/gen.py`,
in milliseconds, medians of seven rounds on a busy machine
([bench/README.md](https://github.com/nforst/fagus/blob/main/bench/README.md#the-worker-and-the-c-library)):

| Invoice | Rust, in-process | PHP, worker | PHP, library | New worker and its first document | New library engine and its first document |
|---|--:|--:|--:|--:|--:|
| 1 page | 3.08 | 3.12 | 3.12 | 11.1 | 5.7 |
| 10 pages | 20.6 | 21.0 | 21.6 | 32.0 | 26.3 |
| 100 pages | 213 | 214 | 225 | 234 | 239 |

In a queue worker, under Octane or in a long-running script, the worker is started once, and
the library gains nothing. Memory is about the same: PHP and the worker together peaked at 23 MB
for the 1-page invoice and 133 MB for the 100-page one, PHP with the library at 20 MB and 126 MB.
