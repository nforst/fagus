# Benchmarks

Measurements that Fagus is held against. Memory and processor time are in [Memory and CPU time](#memory-and-cpu-time-memory), the budgets of the plan in [Budgets](#budgets-memorybudgetspy); the [results of 2026-11-01](#results-of-2026-11-01) are at the end of this file; the comparison with the renderers PHP projects use today is in the [README](../README.md#compared-with-other-renderers).

## Against Dompdf, mPDF, Chrome and WeasyPrint (`compare/`)

The same invoices through Fagus (from PHP, through the worker), Dompdf, mPDF, Browsershot (a new Chrome per PDF), Chrome kept running (Puppeteer, one tab per PDF, as Gotenberg does) and WeasyPrint. `gen-classic.py` writes an invoice with CSS 2.1 only, so that every engine lays it out, with 3, 150 and 600 lines (1, 10 and 40 pages in Chrome); `tools/gen.py` writes the same invoice with flex, grid and margin boxes. Each engine renders each invoice in three new processes, once and then repeatedly; the medians are reported. The documents in `probes/` each test one feature (flex, grid, margin boxes, running elements, named strings, `target-counter()`, repeated table headers, footnotes, the outline, hyphenation), judged from the text of the PDF and its position.

```sh
cargo build --release -p fagus-cli && composer install   # in the repository
cd bench/compare
composer install
PUPPETEER_SKIP_DOWNLOAD=1 npm install                    # uses the Chrome that is installed (CHROME=… to choose)
python3 -m venv .venv && .venv/bin/pip install -r requirements.txt   # WeasyPrint needs Pango (brew install pango)
PYTHON=.venv/bin/python3 ./run.sh
```

The PDFs land in `out/`, for comparing them by eye. Results of 2026-11-14 on an Apple M2 Pro, macOS 27, PHP 8.5, Chrome 157 (milliseconds; *first* is the first PDF of a new process, the other columns further PDFs in the same process):

| Engine | 1 page, first | 1 page | 10 pages | 40 pages | modern CSS, 1 page | modern CSS, 10 pages |
|---|--:|--:|--:|--:|--:|--:|
| Fagus | 8.6 | 2.3 | 17.0 | 62.5 | 2.5 | 17.5 |
| Chrome, kept running | 103.4 | 74.6 | 121.5 | 217.3 | 81.4 | 122.7 |
| mPDF 8.3.1 | 63.1 | 24.9 | 232.8 (8 pages) | 897.8 (31 pages) | | |
| Dompdf 3.1.6 | 70.2 | 47.6 | 348.7 (12 pages) | 2,550.0 (44 pages) | | |
| WeasyPrint 66.0 | 158.2 | 105.0 | 667.5 | 2,596.4 | 134.1 | 714.7 |
| Browsershot 5.5.0 | 611.2 | 600.9 | 649.8 | 750.9 | | |

mPDF reads `size: A4` in `@page` wrongly (it then emits hundreds of empty pages), which is why the classic invoice gives the size in millimetres. mPDF does not load `@font-face` and sets the text in DejaVu Sans Condensed.

## Memory and CPU time (`memory/`)

The engines and invoices of `compare/`, with the memory and processor time of everything the renderer runs: PHP or Node and the processes they start (the Fagus worker, Chrome and its helpers). `driver.py` samples the process tree about every millisecond through `proc_pid_rusage` (macOS only) and sums the resident size (RSS) and the physical footprint of its processes. The footprint is what Activity Monitor shows as memory and what the system charges a process; RSS also counts the pages of the binary and of shared libraries and memory the allocator has handed back but the system has not reclaimed yet. Each engine renders each invoice in three new processes: one PDF, two seconds of rest, 20 more PDFs, three seconds of rest; the medians are reported. CPU time includes the time of helpers that have already ended.

```sh
cargo build --release -p fagus-cli && composer install   # in the repository
(cd bench/compare && composer install && PUPPETEER_SKIP_DOWNLOAD=1 npm install)
cd bench/memory
python3 -m venv .venv && .venv/bin/pip install -r requirements.txt
./run.sh                 # all engines, then four at once; the tables go to out/summary.md
.venv/bin/python3 budgets.py   # Fagus alone: the budgets below
```

Results of 2026-11-18 on an Apple M2 Pro (10 cores, 32 GB), macOS 27, PHP 8.5, Node 26, Chrome 157, Python 3.13 with WeasyPrint 66.0, mPDF 8.3.1, Dompdf 3.1.6, Browsershot 5.5.0. Peak footprint of the whole process tree over all 21 PDFs (peak RSS in parentheses):

| Engine | 1 page | 10 pages | 40 pages | modern CSS, 1 page | modern CSS, 10 pages |
|---|--:|--:|--:|--:|--:|
| Fagus (PHP and the worker) | 22 MB (48) | 33 MB (58) | 60 MB (90) | 22 MB (48) | 33 MB (58) |
| Chrome, kept running (Node and Chrome) | 253 MB (826) | 305 MB (861) | 402 MB (956) | 259 MB (813) | 313 MB (868) |
| Browsershot (PHP, Node and a Chrome per PDF) | 254 MB (807) | 286 MB (846) | 377 MB (936) | | |
| mPDF | 74 MB (90) | 88 MB (104) | 79 MB (95) | | |
| Dompdf | 36 MB (52) | 63 MB (79) | 301 MB (317) | | |
| WeasyPrint | 74 MB (106) | 127 MB (174) | 191 MB (228) | 68 MB (99) | 127 MB (174) |

Processor time per further PDF, all processes together, and of the first PDF of a new process including the start of PHP, Node or Python (milliseconds):

| Engine | 1 page, first | 1 page | 10 pages | 40 pages | modern CSS, 1 page | modern CSS, 10 pages |
|---|--:|--:|--:|--:|--:|--:|
| Fagus | 69 | 3.7 | 20.0 | 70.6 | 4.2 | 20.3 |
| Chrome, kept running | 1,196 | 163.4 | 210.9 | 332.3 | 162.9 | 214.1 |
| Browsershot | 1,336 | 1,263.1 | 1,309.7 | 1,452.8 | | |
| mPDF | 123 | 25.6 | 231.9 | 903.9 | | |
| Dompdf | 130 | 48.5 | 353.3 | 2,695.1 | | |
| WeasyPrint | 287 | 95.4 | 460.2 | 1,643.3 | 120.2 | 495.5 |

Chrome uses 1.5 to 2 times as much processor time as wall time, spread over its helper processes; mPDF and Dompdf set the 10 and 40 pages on other page counts, as above. At rest after the 21 PDFs, the Fagus worker alone holds 10 MB (20 MB RSS) after the 1-page invoice and 31 MB (61 MB RSS) after the 40-page one, and uses no processor time; Chrome holds 161 MB (about 570 MB RSS) whatever it rendered and uses 13 to 22 ms of processor time per second while idle.

Four at once, on the 10-page invoice (four PHP or Python processes; one Chrome with four tabs at a time):

| Setup | Peak footprint (RSS) | PDFs per second | Processor time per PDF |
|---|--:|--:|--:|
| Fagus, 4 processes | 147 MB (250) | 224.3 | 20.3 ms |
| Chrome, 1 browser, 4 tabs | 484 MB (1,400) | 24.2 | 190.0 ms |
| mPDF, 4 processes | 304 MB (368) | 16.3 | 241.0 ms |
| Dompdf, 4 processes | 255 MB (320) | 9.9 | 386.3 ms |
| WeasyPrint, 4 processes | 395 MB (586) | 7.4 | 537.0 ms |
| Browsershot, 4 processes | 898 MB (2,851) | 3.5 | 1,531.1 ms |

## Budgets (`memory/budgets.py`)

The budgets of the plan, measured on Fagus alone: the invoices of `compare/` with 3, 150 and 1,580 lines (classic) or 1,680 lines (modern CSS), which give 1, 10 and 100 pages. Render times come from `fagus render --repeat` (one warm engine, the median of 200, 50 or 10 renders; the median of three processes), peak memory from the kernel's peak footprint and `wait4`'s peak RSS of the fagus process, the start from Python starting `fagus worker` and speaking its protocol. The script waits for a load average below `MAX_LOAD` (4 by default) and records the load it measured under.

Results of 2026-11-19 on the same Apple M2 Pro, macOS 27, at commit 36c4647 (Fagus 0.2.0), with other jobs on the machine; the load average of the last minute is given:

| Budget | Target | Measured | Load |
|---|--:|--:|--:|
| Invoice with 1 page | ≤ 5 ms | 2.2 ms (modern CSS: 2.4 ms) | 1.1–1.4 |
| Invoice with 10 pages | ≤ 35 ms | 16.0 ms (16.4 ms) | 1.1–2.0 |
| Invoice with 100 pages | ≤ 300 ms | 172 ms (188 ms) | 1.1–2.0 |
| Each further page (from 10 to 100 pages) | ≤ 3 ms | 1.7 ms (1.9 ms) | 1.1–2.0 |
| Start of a new process until it is ready (`fagus worker` to its first message) | ≤ 3 ms | 3.3 ms, of which 1.8 ms start any process (`/usr/bin/true`) | 2.4 |
| The same, until the first byte of a trivial PDF, minus the render | | 3.5 ms | 2.4 |
| Invoice with 1 page, with the start of the process | ≤ 10 ms | `fagus render` 9.1 ms, the worker 8.5 ms, from PHP (new `Engine`) 8.4 ms | 2.3–2.4 |
| Peak memory at 100 pages, one render | ≤ 64 MB | **50 MB** footprint, 80 MB RSS (modern CSS: 60 MB, 90 MB); before computed values were shared 96 MB, 121 MB RSS (102 MB, 129 MB), before the earlier changes below 98 MB, 127 MB RSS (108 MB, 135 MB) | 17–21 |
| Peak memory at 100 pages, 21 renders by one worker | ≤ 64 MB | **103 MB** footprint (112 MB), freed memory that macOS's allocator keeps ([below](#memory-a-worker-keeps)); on Linux with glibc 68 MB RSS (modern CSS), as the worker hands it back | 20–21 |
| Binary size (macOS, arm64) | ≤ 30 MB | 25.3 MB | |

The two rows of peak memory were measured again after the changes described in [Where the memory goes](#where-the-memory-goes), on 2026-11-19 on the same machine under heavy load (other builds, a load average of 17 to 21), which changes memory little: one render with `budgets.py memory` (the median of three; seven renders of each binary in turn gave the same, 51 and 60 MB against 96 and 102 MB), 21 renders with the worker driven from PHP (`budgets.py memory`, the median of three). At 1 and 10 pages the peak is 9 and 14 MB footprint (20 and 25 MB RSS; before, 9 and 19 MB). One render of 100 pages is within the budget now. 21 renders in one worker are not on macOS, but what grows there is free memory that the allocator keeps, not memory in use: with `MallocLargeCache=0` in the worker's environment the 21 renders peak at 46 MB (56 MB), with `MallocSpaceEfficient=1` at 41 MB (51 MB). On Linux with glibc, where the worker hands free memory back after each document, `tools/worker-memory.py` measured a peak of 68 MB RSS over 21 renders of the modern invoice, 125 MB without it ([below](#memory-a-worker-keeps)); RSS also counts the binary's pages.

### Where the memory goes

Measured on the 100-page classic invoice (the modern one in parentheses; `budgets.py` writes both to `compare/docs/`) with `memory/heap`, a small program around the `fagus` crate with [dhat](https://docs.rs/dhat) as its global allocator, and `memory/timeline.py`, which samples the footprint of `fagus render` every few tenths of a millisecond and places it against the render's `--timings`:

```sh
cd bench/memory
(cd heap && cargo run --release -- peak ../../compare/docs/classic-1580.html ../out/heap.json)   # minutes
python3 heap/peak.py out/heap.json          # what the heap holds at its peak, by the code that allocated it
(cd heap && cargo run --release -- phases ../../compare/docs/classic-1580.html)                # the heap per phase
python3 timeline.py ../compare/docs/classic-1580.html                                          # the footprint per phase
```

The heap peaks at 42 MB (50 MB) while the pages are laid out; before computed values were shared, it peaked at 78 MB (84 MB):

| Held at the peak | Heap | Before |
|---|--:|--:|
| Computed styles: 14,278 elements (13,488), each with a `ComputedStyle` of 8 bytes that shares its `ComputedValues` (2,712 bytes, 250 properties) with the elements of the same values | 0.6 MB (0.6 MB) | 37 MB (35 MB) |
| Styles that layout makes: the anonymous block around the text of each row of the modern invoice gets values of its own | – (4.4 MB) | – |
| The DOM | 6 MB (6 MB) | 6 MB (6 MB) |
| The fragments of the pages, the box tree and what table layout keeps until the table is done | 35 MB (38 MB) | 35 MB (43 MB) |

The footprint, which also counts memory the allocator holds after freeing it, reached 96 MB during layout and 100 MB while the PDF was written: the DOM and the computed styles were still held then, and the display lists of all pages until the end. Now the PDF's links, bookmarks, structure tree and metadata are taken from the DOM before the pages are painted, so that the DOM and the styles go with the laid-out pages before the PDF is written (while it is written, the heap peaked at 47 MB instead of 64, the footprint at 83 MB instead of 100); and the display list of a page goes once the page is written, unless the document has images (an image that turns out not to decode makes every page be drawn again). The PDFs are the same bytes (`tools/determinism.sh` and the benchmark invoices), and the render times did not change beyond the noise: `fagus render --repeat 10` of both binaries in turn, six times each under load, gave medians of 0.97 and 1.03 times those before.

The peak was then that of layout, 96 MB footprint for the classic invoice and 102 MB for the modern one, most of it computed styles: 2.7 KB for each element, although the elements of a long document are mostly of a few kinds (the rows of a table, their cells, the items of a list) and come out of the cascade with the same values. The cascade already found them (a style was built once for each parent's class and list of declarations, and copied for the other elements), but each element kept a copy, because layout tells boxes apart by the address of their style. Now `ComputedStyle` is a handle of 8 bytes on `ComputedValues` that it shares copy on write, as Blink shares `ComputedStyleBase`'s groups: each element keeps a handle of its own, the elements of a class share one copy of the values, and a change to a style (layout makes styles for anonymous boxes, pseudo-elements and turned writing modes) copies the values first. Splitting the values into groups that are shared separately, as Blink does, would save more where styles differ in a few properties only, but in these invoices the values are shared whole. The heap at its peak fell from 78 to 42 MB (84 to 50 MB), the footprint from 96 to 51 MB (102 to 60 MB) and RSS from 120 to 79 MB (129 to 89 MB), all medians of seven renders of each binary in turn. The PDFs are the same bytes (`tools/determinism.sh`, 574 files, and the benchmark invoices of 1, 10 and 100 pages). The renders take less processor time, as 2.7 KB per element are no longer copied: with one thread (`RAYON_NUM_THREADS=1`), `fagus render --repeat 10` of both binaries in turn, nine times each at a load average of 19 to 28, took 0.85 times the processor time before for the classic invoice, 0.95 for the modern one and 0.96 for the 10-page invoice (wall time 0.74, 0.88 and 0.84).

What is left is mostly layout: the fragments of the pages (their children's vectors alone are 6.5 MB), the line pieces of inline layout (4.4 MB cloned when lines are emitted) and the block containers of table cells. The next step for the modern invoice would be to share the values of the anonymous blocks that layout makes (4.4 MB, one for each row); and to drop the fragments of the pages as they are painted rather than when the PDF is written.

### Memory a worker keeps

These numbers were taken before computed values were shared; after it, a worker keeps 47 MB (76 MB RSS) at rest after one 100-page invoice and 105 MB (155 MB RSS) after 21 (before: 55 and 115 MB, measured the same way). After the 100-page invoice, a worker kept a footprint of 52 MB (125 MB RSS) at rest, and it stays there when small documents follow; after 20 more 100-page invoices it keeps 107 MB (198 MB RSS). It is not the engine's caches: after one or after 60 renders of the 100-page invoice, `vmmap -summary` counts the same 3.4 MB in 19,400 allocations in use (fonts, stylesheets, shaped text, prepared images). The rest is free memory that macOS's allocator keeps for later: empty regions of its small allocations, and its cache of freed large allocations ("Malloc Large (empty)" in `vmmap`), which grows with every large document. In one worker that rendered the 100-page invoice again and again, the footprint at rest was 54 MB after 1 render, 119 MB after 20, 241 MB after 60 and 264 MB after 150; after 20 renders it fell from 108 to 84 MB within 30 seconds of rest and to 45 MB after four minutes. RSS is higher still, because it also counts freed pages the system has not taken back yet.

| Setting in the worker's environment | At rest after 1 × 100 pages | After 20 × 100 pages | Peak |
|---|--:|--:|--:|
| none | 52 MB (125 MB RSS) | 107 MB (198 MB RSS) | 107 MB |
| `MallocLargeCache=0` | 14 MB (120 MB RSS) | 12 MB (121 MB RSS) | 91 MB |
| `MallocSpaceEfficient=1` | 6 MB (119 MB RSS) | 6 MB (119 MB RSS) | 86 MB |

`MallocNanoZone=0`, `MallocMaxMagazines=1` and `RAYON_NUM_THREADS=1` change nothing. With either setting above, the render times differed by no more than the noise of the busy machine they were compared on (load 10 to 13). On a quiet machine (load 3 to 9) they do cost time: with computed values shared, 21 renders of the modern 100-page invoice by one worker driven from PHP (as `budgets.py memory` drives it, the settings in turn, four rounds, medians) took 168 to 171 ms each and peaked at 116 to 120 MB footprint (173 to 176 MB RSS), kept at rest; with `MallocSpaceEfficient=1` they took 197 to 199 ms (16 to 19 % more) and peaked at 51 MB (92 MB RSS), 5 MB at rest; with `MallocLargeCache=0` 178 to 186 ms (1 to 9 % more), 56 MB (94 MB RSS), 12 MB at rest. `MallocSpaceEfficient=0` is the same as no setting.

`MallocLargeCache=0` was measured again on 2026-11-19, because a cost of a few percent would have been worth its memory: the same 21 renders (`engine.php` under `driver.py`, as `budgets.py memory` runs them), no setting and `MallocLargeCache=0` in alternation, 26 rounds with the order turned each round, under a load average of 5.5 to 23.5 (other builds; the machine was not quiet at any time). Per round, `MallocLargeCache=0` took 4.2 % more wall time per render in the median (quartiles −9 % and +30 %, so the wall time says little at this load) and 4.6 % more processor time (quartiles +0.4 % and +13 %), which depends less on how busy the machine is. The memory did not vary: a peak of 56 MB (93 MB RSS) and 12.6 MB at rest (87 MB RSS), against 119 MB (176 MB RSS) peak and at rest with no setting. More than the 3 % that would have made it the default; with the measurement before (1 to 9 % on a quiet machine), it points to a cost of about 4 to 5 %. So the PHP engine sets neither: where memory matters more than time on macOS, set one in the environment of the PHP process, which the worker inherits, or replace workers with `maxDocuments` and `maxMemory`.

On macOS the worker does not hand freed memory back itself. `malloc_zone_pressure_relief(NULL, 0)`, which is to give back the free pages of all malloc zones, releases nothing on macOS 27: it returns 0 at once, and the footprint stays where it is, after the 100-page invoice in the worker as in a C program that frees a million small or forty 2 MB blocks first (the settings above give the large blocks back). A worker that has grown is replaced instead: the PHP engine starts a new one after `maxDocuments` documents or once the worker reports more than `maxMemory` megabytes ([deployment.md](../docs/deployment.md#restart-workers)); the start costs 3 to 4 ms.

On Linux, where Fagus runs in production, glibc's allocator also keeps freed memory in its arenas (one per thread that allocates) and gives it back by itself only from the top of the heap; `malloc_trim(0)` gives back the free pages of all arenas. There is no maintained crate that wraps it safely, so `fagus worker` calls it through `libc` in the one place of the workspace that allows unsafe code (`crates/fagus-cli/src/memory.rs`, only for `target_env = "gnu"`), after it has sent the answer to each document, so that the caller does not wait for it. `FAGUS_MALLOC_TRIM=0` turns it off. `tools/worker-memory.py` renders the modern invoice with 1,700 lines (100 pages) and the 1-page one through the worker and prints the resident size the worker reports with each answer (`rss`, `maxRss`, [protocol.md](../docs/protocol.md)), with the trim and without; the benchmark job of CI runs it on x86-64 and puts the table into the job summary. Measured on 2026-11-19 in Debian 12 (glibc 2.36, arm64) in a Linux virtual machine on the Mac (OrbStack), with three processors, in MB of RSS (`maxRss` in parentheses):

| Worker | With the trim | `FAGUS_MALLOC_TRIM=0` |
|---|--:|--:|
| New worker, 1-page invoice | 16 (16) | 17 (17) |
| 100-page invoice | 63 (64) | 63 (64) |
| then the 1-page invoice | 18 (64) | 63 (64) |
| then 20 × the 100-page invoice | 68 (68) | 125 (125) |
| then the 1-page invoice | 20 (67) | 125 (125) |
| Time of a 100-page invoice, request to answer | 180 ms | 177 ms |

Without the trim, what the worker keeps after 20 large documents varied between runs (79 and 125 MB); with it, the worker keeps about as much as a new one. The time includes the trim after the document before and does not differ beyond the noise. Fixing glibc's thresholds instead (`MALLOC_TRIM_THRESHOLD_=131072`, `MALLOC_MMAP_THRESHOLD_=131072`, which keep glibc from raising them after large blocks are freed) did not help: 63 MB after the first 100-page invoice and 124 MB after 20 more. With musl (the static binaries for Alpine, built and run in Alpine), the worker keeps next to nothing without any call, 14 to 21 MB RSS after 20 large documents, but renders the 100-page invoice in 238 ms instead of 180.

## The worker and the C library

What the worker process costs PHP against the experimental C library ([docs/ffi.md](../docs/ffi.md), [ADR 8](../docs/adr/0008-c-library-experimental.md)), with the scripts in `ffi/`: the modern invoices of `tools/gen.py` with 3, 150 and 1,680 lines (1, 10 and 100 pages) rendered by `fagus render --repeat` (Rust, one warm engine), from PHP through the package and its worker, and from PHP through the library and PHP's FFI with the request the package sends. Warm is the median of 200, 50 or 10 renders after one; "new" a new engine and its first document, timed together as each PHP-FPM request pays it (the median of 30, 15 or 5). Each in a process of its own, in seven rounds with the order turned every other round; the medians of the rounds.

```sh
cargo build --release -p fagus-cli -p fagus-ffi && composer install   # in the repository
cd bench/ffi
python3 run.py times             # render times
python3 run.py memory            # memory of PHP and the worker, 100 pages (PAGES=1 for 1 page; macOS)
```

Results of 2026-11-20 on an Apple M2 Pro, macOS 27, PHP 8.5, the binary of commit 21ac2c0 and the library as it was added after it, with other builds on the machine at a load average of 3.8 to 4.7, in milliseconds:

| Invoice | Rust, in-process | PHP, worker | PHP, library | New worker and its first document | New library engine and its first document |
|---|--:|--:|--:|--:|--:|
| 1 page | 3.08 | 3.12 | 3.12 | 11.13 | 5.73 |
| 10 pages | 20.61 | 20.95 | 21.60 | 31.97 | 26.28 |
| 100 pages | 212.75 | 214.12 | 225.21 | 233.90 | 239.05 |

- The worker costs PHP 0.04, 0.34 and 1.37 ms more than the same render in Rust (1.3, 1.6 and 0.6 %): the pipe, the frames and the copies of the request and the PDF both ways. That is the most a library can save on a warm engine.
- The library did not save it: from PHP it was as fast at 1 page and 3 and 5 % slower at 10 and 100 pages, in every round. Called from a small C program, it rendered the 100-page invoice within the noise of `fagus render` (218 to 226 ms against 210 to 225 ms, three runs each), so the difference lies in rendering inside the PHP process, not in the library; it was not looked into further.
- What the library saves is the start of the worker: 5.4 ms at 1 page and 5.7 ms at 10 pages for each new engine, which PHP-FPM pays in each request that renders. Loading the declarations with `FFI::cdef()` in each request, as without preloading, added nothing measurable once the library was loaded.

Memory, 21 renders of one invoice by one engine (`run.py memory`, five rounds, medians): the peak physical footprint, and the resident size after the renders in parentheses, in MB.

| Invoice | PHP, with the worker | The worker | Together | PHP with the library |
|---|--:|--:|--:|--:|
| 1 page | 12.4 (27.8) | 10.3 (21.5) | 22.7 (49.5) | 19.9 (45.8) |
| 100 pages | 14.6 (30.0) | 118.2 (174.3) | 132.8 (204.3) | 126.3 (194.9) |

One process instead of two saves 3 to 7 MB of footprint (4 to 9 MB RSS), about what a second process costs on its own. What the engine needs moves into PHP's process, and stays there: on macOS the allocator keeps what the large documents freed ([above](#memory-a-worker-keeps)) in the PHP process instead of in a worker that the package can replace.

## Remote resources (`resources/`)

A document with 50 images at `https://` URLs, answered from PHP by a callback that waits 20 ms for each request, as a server some way off answers. `resolver()` is asked for one URL after the other and waits for each. `resolveMany()` gets the URLs of a batch together and waits once, as a callback does that fetches them concurrently (`Http::pool()`, `curl_multi`); the wait stands in for the network, so the numbers show what the engine and the protocol add, not what a real server does. The first row hands the images over with `resource()`, with nothing to fetch. The script checks that all three PDFs are the same bytes.

```sh
cargo build --release -p fagus-cli && composer install   # in the repository
php bench/resources/bench.php                            # 50 images, 20 ms each
php bench/resources/bench.php 500 20 3                   # images, milliseconds, renders
FAGUS_BINARY=/path/to/fagus-1.1.0 php bench/resources/bench.php   # an older binary: the time before
```

Results of 2026-11-25 on an Apple M2 Pro, macOS 27, PHP 8.5, on a busy machine (milliseconds, the median of 5 renders in a warm worker):

| | `resource()` | `resolver()` | `resolveMany()` | Calls of `resolveMany()` |
|---|--:|--:|--:|--:|
| 50 images, binary of 1.1.0 | 10.1 | 1,515 | 1,329 | 50 |
| 50 images | 4.7 | 1,245 | 32.4 | 1 |
| 500 images (median of 3) | 33.4 | 11,327 | 211 | 8 |

A binary of 1.1.0 knows no batches and asks `resolveMany()` for each URL on its own, as it asks `resolver()`. With batches, the 50 images cost one wait of 20 ms and about 10 ms more than handing them over; 500 images come in 8 batches of at most 64.

## Fagus

```sh
cargo build --release -p fagus-cli
./target/release/fagus render tests/corpus/invoice/invoice-3.html -o /tmp/invoice.pdf \
  --font-dir tests/fonts --timings --repeat 100
```

`--repeat` renders the document repeatedly with one warm engine and prints the median.

## Other engines (`competitors/`)

PlutoBook, fullbleed, fulgur and ironpress, through their Python packages in a Linux container. The image is x86-64 because PlutoBook only ships x86-64 wheels for Linux.

```sh
cd bench/competitors
mkdir -p fonts && cp ../../tests/fonts/Inter-Regular.ttf ../../tests/fonts/Inter-Bold.ttf fonts/
docker build --platform linux/amd64 -t fagus-competitors .
# Scaling (0 to 300 invoice lines): documents n0.html … n300.html, generated with tools/gen.py
docker run --rm --platform linux/amd64 -v "$PWD:/bench" fagus-competitors python /bench/scale.py pluto
```

| Script | Measures |
|---|---|
| `bench.py` | first render and warm median of one document |
| `scale.py` | render time over 0 to 300 invoice lines (fixed cost and cost per page) |
| `ablate.py`, `ablate2.py` | what the fixed cost per document consists of |
| `one.py` | renders a single document with one engine (for visual comparison) |
| `run-engines.sh` | all of the above inside a plain `python:3.13-slim` container, pinned to one CPU |

## Process overhead (`ipc/`)

What it costs PHP to call a native engine: a new process per call (`proc_open`), a persistent worker, or FFI. `engine.c` is a stand-in engine that only copies bytes.

```sh
cd bench/ipc
docker build -t fagus-ipc .
docker run --rm -v "$PWD:/bench" fagus-ipc sh -c \
  'gcc -O2 -static -o /tmp/engine engine.c && gcc -O2 -shared -fPIC -DLIBRARY -o /tmp/libengine.so engine.c && php bench.php /tmp/engine /tmp/libengine.so 20000 70000 300'
```

## Results of 2026-11-01

Taken before Fagus had any code, as the bar to clear. "Server" is an AMD EPYC-Rome virtual machine, pinned to one core; "Mac" is an Apple M3.

### Calling a native engine from PHP 8.4

The engine is the stand-in from `ipc/`, so only the cost of the call is measured.

| Way | Mac, Linux VM (20 KB / 1.5 MB input) | Server (20 KB / 1.5 MB input) |
|---|---|---|
| New process per call (`proc_open`) | 0.17 / 0.72 ms | 0.54 / 1.59 ms |
| Persistent worker | 0.06 / 0.21 ms | 0.20 / 0.66 ms |
| FFI | 0.04 / 0.06 ms | 0.16 / 0.23 ms |

### Engines on the server (test invoice with standard CSS)

| Engine | 1 page, warm | 1 page, new process | 150 lines (10 pages) |
|---|---|---|---|
| PlutoBook (C++) | 27 ms | 40–56 ms | 55 ms |
| fullbleed (Rust) | 21 ms | 27–29 ms | 694 ms |
| fulgur (Rust) | 46 ms | 45–65 ms | 242 ms |
| ironpress (Rust) | 45 ms | 170–230 ms | 731 ms |
| mPDF (PHP), production invoice template | 80 ms | ~80 ms | 1.8 s (120 lines) |
| Chrome 156, command line (Mac) | ~500 ms per document | | ~520 ms |

### Scaling (x86-64 under Rosetta on the Mac, about as fast as the server)

| Lines (pages) | PlutoBook | fullbleed | fulgur | ironpress |
|---|---|---|---|---|
| 0 (1) | 19.6 ms | 14.0 ms | 29.5 ms | 22.9 ms |
| 30 (3) | 31.3 ms | 54.2 ms | 62.5 ms | 113.7 ms |
| 150 (9–10) | 51.0 ms | 401.1 ms | 153.7 ms | 482.3 ms |
| 300 (18–19) | 73.1 ms | 1,447.9 ms | 291.3 ms | 992.3 ms |

### What the fixed cost per document consists of (invoice without lines)

| Engine | Total | Cause | Without the cause |
|---|---|---|---|
| PlutoBook | 19.8 ms | reloading the `@font-face` fonts for every document | 2.4 ms |
| fullbleed | 13.5 ms | embedding the font (~12 ms) | – |
| fulgur | 30.7 ms | second pass for margin boxes with page numbers | 7.4 ms |

### Modern CSS (24 features used by Tailwind v4, `tests/corpus/modern-css.html`)

| Engine | Supported | Notes |
|---|---|---|
| Chrome | 24 | reference |
| ironpress | ~21 | the table layout of the test page falls apart |
| fullbleed | 18 | no nesting, `inset`, `aspect-ratio`, transforms, shadows or `min()` with percentages |
| fulgur | 15 | wrong colors for `oklch()`, `oklab()` and `color-mix()`; no `:has()` or `@property` |
| PlutoBook | 7 | no CSS Color 4 colors, `@layer`, nesting, grid, `calc()` with percentages, `@property`, multiple backgrounds, `aspect-ratio`, individual transforms or shadows |
