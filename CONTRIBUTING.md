# Contributing

Fagus is in early development. Design decisions are recorded in [docs/adr](docs/adr). The Composer package `nforst/fagus` is this repository: its `composer.json` is at the root and its PHP code in `php/`.

- Run `cargo fmt`, `cargo clippy --workspace --all-targets` and `cargo test --workspace` before sending changes.
- For changes to the PHP code, run `composer install` and `vendor/bin/phpunit` in the repository root (after `cargo build --release`, whose binary the tests use), and `vendor/bin/pint` for the code style of `pint.json`, which CI checks with `vendor/bin/pint --test` (the scripts in `bench/` are left out).
- No `unsafe` code. Every library crate forbids it; the exceptions are the call of glibc's `malloc_trim` in `crates/fagus-cli/src/memory.rs`, which has no safe wrapper, and the functions C calls in `crates/fagus-ffi/src/abi.rs`, the experimental C library ([ADR 8](docs/adr/0008-c-library-experimental.md)), where every unsafe block says what makes it sound.
- The workspace builds with the Rust of `rust-toolchain.toml`, and must also build and pass its tests with the oldest one `rust-version` in `Cargo.toml` names (1.97.0), which CI checks: `rustup toolchain install 1.97.0` and `cargo +1.97.0 test --workspace`. Use no standard library function newer than that.
- Trigonometry, powers, logarithms and the like come from the `libm` crate (`libm::sinf`, `libm::powf` …), not from the methods of `f32` and `f64`: those call the system's math library, whose results differ between platforms in the last bit, and the PDF must have the same bytes everywhere. `clippy.toml` refuses the methods.
- Behaviour follows the CSS specifications; appearance is compared with Chrome. Print features that Chrome lacks follow the specifications and WeasyPrint.
- Every bug fix comes with a test.
- Changes that users notice get an entry under `[Unreleased]` in `CHANGELOG.md`. What counts as public API, and what a minor release may change, is in [docs/stability.md](docs/stability.md) ([ADR 5](docs/adr/0005-public-api-and-deprecations.md)). From 1.0 on, breaking changes wait for a major release: deprecate first in a minor release, with the replacement and an entry under *Deprecated*, and add the planned removal to `UPGRADING.md` under *Next major*. A deprecation is reported where it is used: in PHP with `@deprecated since …` in the docblock and `@trigger_error(…, E_USER_DEPRECATED)` once per call site, in Rust with `#[deprecated(since = …, note = …)]`, for a command line option with a warning on standard error, for a CSS extension with a `css.deprecated` diagnostic ([ADR 5](docs/adr/0005-public-api-and-deprecations.md#deprecation)).
- In the crate `fagus`, a new public enum or struct with public fields that may grow is `#[non_exhaustive]`, and a struct users make gets a constructor; a trait users implement gets new methods only with a default implementation.
- An incompatible change to the worker protocol raises `PROTOCOL` in `crates/fagus-cli/src/worker.rs` and `Engine::PROTOCOL` together, and only in a major release; compatible changes (new optional fields) keep the number.
- Pull requests also run the [release gates](docs/releasing.md#release-gates): the corpus validated with veraPDF, qpdf and Mustang, rendered to the same bytes on every platform, and timed. After changes to the PDF output, run `tools/validate.sh --subset /tmp/fagus-validate` (it needs Java) and `tools/determinism.sh /tmp/fagus-determinism` locally.
- The documentation is in `docs/` and is published on [fagus.niklasforst.de/docs](https://fagus.niklasforst.de/docs) (`main` under `/docs/next`). Every page needs an entry in `docs/nav.yml`; run `php tools/check-docs.php` after changing the pages, which also checks the links between them.

## Conformance tests

`tools/wpt/run.sh` runs the reftests of [web-platform-tests](https://web-platform-tests.org/) in the directories of `tools/wpt/dirs.txt`: it checks out the tests at the commit `tools/wpt/fetch.sh` pins into `target/wpt`, renders each test and its reference with Fagus and compares the pages drawn by Poppler's `pdftoppm` (install `poppler-utils`). It fails if a test fails that `tools/wpt/expectations.txt` does not list. When a fix makes listed tests pass, `tools/wpt/run.sh --update --docs docs/compatibility.md` removes them from the list and updates the table of pass rates; commit both with the fix. A test that cannot pass for a reason other than a bug (it needs scrolling or user interaction, or takes minutes) goes into the list as `SKIP` with the reason as a comment. The workflow `wpt.yml` runs all directories each night; [compatibility.md](docs/compatibility.md) explains the comparison.

`tools/chrome-corpus.sh` prints the corpus with Chrome and with Fagus and compares the pages; the release gates fail when a document differs that `tools/chrome-expectations.txt` does not list.

## Fuzzing

`fuzz/` has [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) targets for the parts that read untrusted input. It is a workspace of its own and needs a nightly toolchain, which the engine's build and tests do not:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
fuzz/run.sh render 600    # a target, and for how many seconds
```

| Target | Input |
|---|---|
| `render` | HTML, or XHTML when it starts with `<?xml`, rendered to PDF |
| `invariants` | the same input as `render`, rendered twice and checked for what holds for every layout |
| `css` | a style sheet, parsed and cascaded over a fixed document |
| `font` | a font file (TrueType, OpenType, WOFF, WOFF2), shaped and used through `@font-face` |
| `image` | an image (PNG, JPEG, GIF, WebP, BMP, TIFF, AVIF) used by a fixed document in several ways |
| `avif` | an AVIF image (its container and AV1 pictures), as an element, filtered and in SVG |
| `pdf` | a PDF file (cross references, objects, page tree, encryption, content streams) as a page template, an element and a border image |
| `svg` | an SVG image, used as a file and inline in HTML |
| `worker` | the frames a caller sends `fagus worker` ([protocol](docs/protocol.md)) |

`fuzz/run.sh` seeds `fuzz/corpus/<target>` from `fuzz/seeds` and the documents, style sheets, fonts and images of the tests (`fuzz/seed.sh`), then runs the target with its input size, dictionary, timeout and memory limits; options after the seconds go to libFuzzer. The targets are built with overflow checks and debug assertions, set in the release profile of `fuzz/Cargo.toml` (cargo-fuzz builds with `-O`, which leaves them to the profile), but without AddressSanitizer, which makes rendering several times slower; the profile leaves the checks off only for the AV1 decoder `re_rav1d`, whose `extern "C"` functions abort on an overflow its C original wraps, until rav1d wraps there itself; `FUZZ_SANITIZER=address fuzz/run.sh …` turns it on for the unsafe code in dependencies. A crash, panic, timeout or running out of memory is saved in `fuzz/artifacts/<target>`; replay it with `cargo +nightly fuzz run -O <target> <file>` and shrink it with `cargo +nightly fuzz tmin -O <target> <file>`. A panic counts even where the engine would catch it and answer with an internal error. Fixes come with the shrunk input as a test, usually in `crates/fagus/tests/robustness.rs`. Corpora and artifacts stay out of the repository; only the small seeds in `fuzz/seeds` are committed.

The target `invariants` looks at the layout rather than for crashes. `crates/fagus-invariants` renders the document with the feature `inspect` of `fagus`, which keeps the parsed document and the pages of the last layout pass, and checks them: every number is finite, no fragment, page or reported box has a negative size (but text that negative `letter-spacing` pulls together), no placeholder of a positioned box is left, the page count is between 1 and 10,000 and the same in the output, the pages and the reported layout (`report_layout`), that layout has a box for each box of the elements with an `id`, and rendering again gives the same bytes. A document that runs out of its memory budget or time limit is not checked. `cargo test -p fagus-invariants` checks the same on every document of `tests/corpus`, there also against an engine that has rendered nothing before. A violation is a panic that names the invariant, the page and the fragment, with an outline of the page; shrink the input with `cargo +nightly fuzz tmin -O invariants <file>` and write the markup down; `cargo run -p fagus-invariants --example check <file>` checks a document without the fuzzer and prints every violation. It is then one of three things: a bug in the engine, fixed with the markup as a test in `crates/fagus/tests/robustness.rs`, or in `crates/fagus-invariants/tests/found.rs` where only the layout shows it; something the engine does on purpose, for which the invariant is narrowed in `crates/fagus-invariants/src/lib.rs` with a comment that says why; or unclear, which goes into an issue with the markup.

The workflow `fuzz.yml` runs every target for 10 minutes each night and on demand (with a choice of sanitizer), keeps each corpus in the cache between runs and uploads crashes as artifacts. CI checks on every change that the targets still build (`cargo +nightly check --bins` in `fuzz/`): the `worker` target includes `worker.rs` of fagus-cli and the modules it uses, so a module that `worker.rs` starts to use goes into the target as well.
