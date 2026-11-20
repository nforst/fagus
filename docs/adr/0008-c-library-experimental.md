# 8. An experimental C library beside the worker

- Status: accepted
- Date: 2026-11-20

## Context

The PHP package runs Fagus as a worker process: it starts `fagus worker` with the first document,
sends requests through a pipe in the frames of the [worker protocol](../protocol.md) and keeps
the process for the next documents. [Issue #3](https://github.com/nforst/fagus/issues/3) asks for a
C shared library instead (a `cdylib` with something like `fagus_render` and `fagus_free`), so that
PHP calls Fagus in its own process through [FFI](https://www.php.net/manual/en/book.ffi.php),
without the extra process, the pipe and the framing, and so that other languages can use it the
same way.

What the worker costs was measured on 2026-11-20 on an Apple M2 Pro (macOS 27, PHP 8.5, a busy
machine), with the invoices of `tools/gen.py`, in Rust, from PHP through the package and its
worker, and from PHP through a C library built for the purpose with the same request
([bench/README.md](../../bench/README.md#the-worker-and-the-c-library)), in milliseconds:

| Invoice | Rust, in-process | PHP, worker | PHP, library | New worker and its first document | New library engine and its first document |
|---|--:|--:|--:|--:|--:|
| 1 page | 3.08 | 3.12 | 3.12 | 11.1 | 5.7 |
| 10 pages | 20.6 | 21.0 | 21.6 | 32.0 | 26.3 |
| 100 pages | 213 | 214 | 225 | 234 | 239 |

- Once the worker runs, the process, the pipe and the framing cost 0.04 ms for 1 page, 0.3 ms
  for 10 and 1.4 ms for 100 (0.6 to 1.6 %) against the render in Rust. That is all a library could
  save per document, and from PHP it did not: it was as fast at 1 page and 3 to 5 % slower at 10
  and 100 pages, in every round (from a C program it was within the noise of the binary in three
  runs, so this seems to be about rendering inside PHP's process; it was not looked into
  further).
- What a library does save is the start of the worker, about 5.5 ms in each PHP-FPM request that
  renders, where the worker lives as long as the request. In queue workers, under Octane and in
  long-running scripts the worker starts once.
- Memory: one process instead of two saves 3 to 7 MB of a peak of 23 MB (1 page) or 133 MB (100
  pages). The engine's memory moves into the PHP process.

So the overhead the issue names is real but small; the arguments for a library are the start of
the worker under PHP-FPM, setups where PHP may not start processes, and languages that would
rather call a function than speak the protocol.

The worker gives things that a library in the caller's process cannot:

- **Crash isolation.** Panics are caught in both: the engine catches them around every document.
  But what ends a process (a stack overflow in a deep document, an allocation that fails, on
  which Rust aborts, a fault in the unsafe code of a dependency such as an image decoder or the
  compression) ends the worker, and the PHP package answers with an `EngineException` and starts
  a new one. In-process it ends the PHP-FPM worker, or the Octane or queue worker, with the
  request and whatever else it was doing.
- **The sandbox.** On Linux, `fagus worker` gives up the network, starting programs and writing
  files (Landlock, seccomp) and can be held to some directories (`--allow-read`). A library
  cannot do that to the process that loads it, which needs all of these.
- **Memory.** The worker hands freed memory back after each document on Linux (`malloc_trim`),
  and the PHP package replaces a worker that has grown (`maxMemory`, `maxDocuments`). In-process,
  what the allocator keeps after a large document stays in the PHP process until it ends
  (`pm.max_requests`). `--memory-limit` and `--cpu-limit` limit a worker, not PHP. The memory
  budget and the time limit of each document work in both, since the engine checks them itself.
- **Threads.** The engine renders on a thread of its own with a large stack and compresses the
  PDF on two more. In-process these are threads in every PHP process that renders, and threads do
  not survive a `fork()`: PHP-FPM forks its workers from the process that runs the preload script.
- **Callbacks.** The protocol lets the worker ask its caller for resources (`?`), signatures
  (`S`), timestamps (`T`) and revocation data (`V`) while it renders. Through FFI these would be
  C function pointers into PHP closures, called on the engine's threads; PHP, outside its
  thread-safe builds, cannot run PHP code on another thread.
- **Version skew.** The PHP package installs the binary of its own release and checks the
  protocol and the version in the worker's hello. A library is whatever file the caller loads.

On the PHP side, PHP's default `ffi.enable=preload` allows `FFI::cdef()` on the command line only.
In web requests the declarations must come from the preload script (`opcache.preload` with
`FFI::load()`, taken with `FFI::scope()`), or `ffi.enable=1` allows FFI everywhere, which PHP
advises against. Shared hosting, where a library would help most, seldom lets users set either,
and the FFI extension is not always there: the official Docker images build it only on request
(`docker-php-ext-install ffi`). Hosts that forbid `proc_open()` tend to forbid FFI as well. The
issue's author is on Laravel 9; the package needs PHP 8.3 and Laravel 13
([ADR 5](0005-public-api-and-deprecations.md)), and a library would let older PHP call Fagus
directly, without the package.

On the Rust side, every library crate forbids `unsafe` (`#![forbid(unsafe_code)]`) and the
workspace denies it; the one exception is the call of `malloc_trim` in
`crates/fagus-cli/src/memory.rs`. A C interface cannot be written without unsafe code: exported
symbols (`#[unsafe(no_mangle)]`), raw pointers from the caller, memory handed out and given back.

A C interface would also be public API in the sense of ADR 5: the signatures of its functions,
the layout of whatever is passed by value, who frees what. Once 1.0 promises stability, every
mistake in it stays for the whole major.

## Decision

- Fagus offers a C library from 1.0.0 on, **experimental**: the crate `crates/fagus-ffi`, a shared
  library (`cdylib`) and a static one (`staticlib`) with the header
  `crates/fagus-ffi/include/fagus.h`, written by hand and checked against the code by a test. It
  is not covered by ADR 5 and the 1.x stability promise: `fagus_abi()` returns 0, and any
  release may change it. A later record makes it stable (and `fagus_abi()` 1), if people use it
  and its shape holds up.
- The release workflow builds the shared library for Linux (x64 and arm64, glibc 2.17), macOS
  (x64 and arm64) and Windows (x64), as `fagus-ffi-<target>.tar.gz` with `fagus.h` and the
  licenses, with an SBOM, a sum in `SHA256SUMS` and the attestations of the binaries. Not for
  musl: its static targets make no shared library, and there it is built from source with
  `-crt-static` off.
- The PHP package does not use it. The worker stays the way the package runs Fagus: warm, the two
  render as fast, and the worker's isolation, sandbox and memory handling are worth more than the
  start it costs under PHP-FPM, which a queue worker or Octane avoids.
- The interface takes the render request of the worker protocol: the JSON header of `R` and its
  data blocks one after another, and answers with the header of `P` or `E` as JSON and the PDF.
  The options are the protocol's, kept in one place: `request.rs` of `fagus-cli` turns a request
  into a document and the result into an answer, for the worker and, included as it is, for the
  library (as the fuzz target includes the worker). A new protocol field reaches both at once.
  The engine takes the options of `fagus worker`, as JSON; one it does not know is refused.
- The surface is small and opaque: `fagus_version`, `fagus_abi`, `fagus_protocol`,
  `fagus_engine_new`, `fagus_engine_free`, `fagus_render`, `fagus_result_ok`,
  `fagus_result_header`, `fagus_result_pdf`, `fagus_result_free`, `fagus_string_free`. Engines and
  answers are opaque pointers read through functions, never structs whose layout would be API.
  What the library hands out goes back to it through the function for it. Every function checks
  for null and catches panics (`catch_unwind`), so that none unwinds into C; a render never
  returns null, only an answer that is an error.
- No callbacks in this version. `ask`, and signatures with `certificates`, `timestamp` or
  `revocation`, are answered with an error. Resources come with the request (`resources`, `data:`
  URLs) or from the files of `base` and `root`; signatures with a PKCS#12 file work.
- The unsafe code is in one file, `crates/fagus-ffi/src/abi.rs`, allowed there with a reason. The
  crate denies it everywhere else, every other crate keeps forbidding it, and each unsafe block
  says what makes it sound. A crate of its own keeps the exception out of the engine, which stays
  free of unsafe code for every user of the `fagus` crate.
- The library starts its thread pool with the first document, not with the first engine, so that
  an engine made before a `fork()` does not leave the children a pool without threads.
- Tests call the interface through raw pointers from Rust and build and run a C program against
  the header; `tools/ffi.php` calls every function through PHP's FFI where PHP has it, which a CI
  job requires. The release workflow runs the C program against each library its runners can
  run.

## Consequences

People who want Fagus in-process, from PHP or from another language, can download it and try it,
and what they report decides what it becomes. Under PHP-FPM it saves the start of the worker in
each request; nothing else measured favours it.

The unsafe code of the workspace grows from one call to one file, which a change to `abi.rs`
gets read for with care. The protocol code of the worker is shared now: the worker implements
its `Caller` with its connection, the library has none.

Each release carries five more archives, about the size of the binary each (28 MB for the
library on macOS arm64). The static library (220 MB on macOS, every dependency in full) is not
released; C programs that want it build it.

What would make it more than experimental: users who build on it; callbacks a caller can answer
safely, which needs the engine to render on the thread that called `fagus_render` so that a C
function pointer is called there; a file of declarations that PHP's `FFI::load()` reads (it takes
no `#include` or `#ifndef`); and, if the PHP package should use it, an opt-in engine with the same
`Document` and `Pdf`, which would make the interface API of the package and needs its own record.
