# Releasing

A release consists of the `fagus` binary for every platform, attached to a GitHub release of `nforst/fagus`, and the Composer package `nforst/fagus`, which installs the binary of its release and accepts no other: `php/fagus/src/Internal/Release.php` lists the SHA-256 of each binary.

1. **Prepare.** Set the version in `Cargo.toml` (`[workspace.package]`), move the entries under `[Unreleased]` in `CHANGELOG.md` to the new version, update the table of supported versions in `SECURITY.md` (a new minor version replaces the previous one of its major; with a new major version, the last minor version of the previous major stays, with security fixes until six months after the release), run `tools/notices.sh` (it rewrites `THIRD-PARTY-NOTICES` with the licenses of the crates the binary is built from; it needs `cargo install --locked cargo-about --features cli`) and commit. Push the commit and wait until the workflows *CI* and *Release gates* have passed on it (`gh run list --commit "$(git rev-parse HEAD)"`); the [release gates](#release-gates) are the checks of the corpus that a release relies on.

   Check the entries against [stability.md](stability.md) first. From 1.0 on, a minor release has nothing under *Removed* and no breaking change under *Changed*; everything it deprecates is under *Deprecated*, with its replacement. A major release removes only what a released minor version deprecated, and `UPGRADING.md` gets a section for it with every breaking change and what to do: the entries collected under *Next major* move into it. A raised minimum (PHP, Laravel, Rust, glibc) is listed under *Changed*. A security fix for the last minor version of the previous major is released from a branch of its tag (`1.9`), as a patch release of that minor version, with the binaries pinned as below.
2. **Build.** Run the workflow *Release* (`.github/workflows/release.yml`) on that commit with the version, for example `gh workflow run release.yml -f version=0.1.0`. It builds the binaries for Linux (x64 and arm64, glibc 2.17 and musl), macOS (x64 and arm64), Windows (x64) and FreeBSD (x64, in a FreeBSD 14 VM on a Linux runner, with [vmactions/freebsd-vm](https://github.com/vmactions/freebsd-vm)), checks that those the runners can run render a document (the FreeBSD binary in its VM, and in a FreeBSD 15 VM before the release is drafted), and attaches them with their sums (`SHA256SUMS`) and a CycloneDX SBOM of each (`fagus-<target>.cdx.json`, made with `cargo-cyclonedx`) to a draft release `v0.1.0`. Each binary, its SBOM and `SHA256SUMS` get an [artifact attestation](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations): SLSA build provenance signed through Sigstore with the identity of the workflow run, without a key or secret; the binaries an attestation of their SBOM as well. The same workflow builds the experimental [C library](ffi.md) for Linux (x64 and arm64, glibc 2.17), macOS (x64 and arm64) and Windows (x64), not for musl, whose static targets make no shared library: `fagus-ffi-<target>.tar.gz` holds the shared library (on Windows with its import library), `fagus.h` and the licenses, with an SBOM (`fagus-ffi-<target>.cdx.json`), a sum in `SHA256SUMS` and the same attestations; a C program built against the header renders a document on Linux x64 and macOS arm64. `tools/pin-binaries.php` leaves the archives out, as the PHP package does not use them.
3. **Pin.** Fetch the files of the release and write the sums into the PHP package:

   ```sh
   gh release download v0.1.0 -D /tmp/fagus-0.1.0
   php tools/pin-binaries.php 0.1.0 /tmp/fagus-0.1.0/SHA256SUMS
   git commit -am "chore: pin the binaries of 0.1.0"
   ```

   With the binaries next to `SHA256SUMS`, the script checks each against its sum and with `gh attestation verify` against its attestation (built by `.github/workflows/release.yml` of this repository) before it pins anything.

   This commit changes nothing but `Release.php`, so the binaries are those of the code it is tagged with.
4. **Check.** Install the binary as a user would, from the files of the draft: GitHub does not hand out a draft by its tag, so fetch them (`gh release download v0.1.0 -D /tmp/fagus-0.1.0`) and set `FAGUS_DOWNLOAD_URL=/tmp/fagus-0.1.0`, which the installer checks against the pinned sums like a download. The validation, the determinism, the speed and the comparison of the corpus with Chrome were checked by the release gates of the commit; what remains by hand is a look at the documents that do not match Chrome, which the job summary lists.
5. **Tag and publish.** Tag the pin commit `v0.1.0`, push the tag, then publish the draft release, which takes the existing tag. The Composer package is released with the same tag: Packagist reads `composer.json` at the root of this repository. [fagus.niklasforst.de/docs](https://fagus.niklasforst.de/docs) shows the documentation of the new tag within about ten minutes.

## Release gates

The workflow *Release gates* (`.github/workflows/gates.yml`) runs on every push to `main` and every pull request. It builds the binary on Linux x64, Linux arm64 and macOS arm64 and checks:

- **PDF validation** (`tools/validate.sh`): every page of `tests/corpus` rendered plain, encrypted (checked by qpdf with its password), as PDF/A-2a, 2b, 2u, 3a, 3b and 3u, and as PDF/UA-1 (also encrypted, without and with a user password; veraPDF checks the latter decrypted by qpdf, once qpdf has shown that it allows the extraction for accessibility), the invoices as Factur-X (PDF/A-3b, and PDF/A-3a with PDF/UA-1), and the pages with raster images with their images as JPEGs (`--image-quality 80`: plain, as PDF/A-1b, 2b and 3u and as PDF/UA-1). Every file passes `qpdf --check`, every PDF/A and PDF/UA file passes veraPDF against the profile of its level, and every invoice passes Mustang. Pages that Fagus refuses for a level (characters the test fonts lack, images without alternative text) are listed and left out; any other failure to write a file fails the check.
- **Determinism** (`tools/determinism.sh`): every page rendered plain, as PDF/UA-1 and as PDF/A-3u (the pages with raster images also with `--image-quality 80`, plain and as PDF/A-1b) with a fixed `--created`, once in a new engine and once in a warm one, gives the same bytes both times; and the SHA-256 of every file is the same on Linux x64 and arm64, macOS arm64 and FreeBSD x64 (job *same bytes on every platform*; FreeBSD's PDFs are rendered in a FreeBSD VM).
- **Chrome** (`tools/chrome-corpus.sh`): every page of `tests/corpus` printed by Chrome for Testing (the version the script pins, on Linux x64) and by Fagus, both with the same fixed set of fonts from Ubuntu's packages, both drawn by `pdftoppm` at 50 dpi. A document matches when both have the same number of pages and on no page more than 0.5 % of the pixels differ beyond antialiasing; every document must match except those listed in `tools/chrome-expectations.txt`, with the reason they differ, and those that use print features Chrome lacks (running elements, named strings, cross references, leaders, footnotes, blank pages for left and right page breaks), Fagus' layers, TIFF images or web fonts with `font-display: optional`. A listed document that matches now gives a warning: take it off the list (`tools/chrome-corpus.sh OUT_DIR --update`). The job summary lists the documents that differ and the share that match; [compatibility.md](compatibility.md) describes the comparison.
- **Speed** (`tools/bench-smoke.py`): the benchmark invoice of `tools/gen.py` with 0, 3, 150 and 1,700 lines, and one page with the start of a new process, against three times the budgets (2.5 ms, 5 / 35 / 300 ms for 1 / 10 / 100 pages, 10 ms with the process start). That only catches gross regressions on a shared runner; the job summary shows the numbers, and [bench/README.md](../bench/README.md) measures properly.

The scripts run locally too, after `cargo build --release -p fagus-cli`:

```sh
tools/validate.sh /tmp/fagus-validate         # Java 11 or later; --subset for about 20 pages
tools/determinism.sh /tmp/fagus-determinism
python3 tools/bench-smoke.py /tmp/fagus-bench
CHROME=/path/to/chrome tools/chrome-corpus.sh /tmp/fagus-chrome   # pdftoppm; Linux x64 downloads Chrome
```

`tools/validate.sh` downloads veraPDF, Mustang and qpdf at the versions it names into `target/validate-tools` and checks them against their SHA-256; `VERAPDF`, `MUSTANG` and `QPDF` choose others. To update a tool, change its version and sum in the script.

The workflows name the actions they use by commit, with the version in a comment (`uses: actions/checkout@<sha> # v4.4.0`), so that a tag moved to other code changes nothing here; Dependabot (`.github/dependabot.yml`) proposes the commits of new versions every week.

## Composer

The Composer package `nforst/fagus` is this repository: `composer.json` at the root, the PHP code in `php/fagus` (`Fagus\`) and `php/laravel` (`Fagus\Laravel\`, registered by Laravel's package discovery). `.gitattributes` leaves the engine's sources, tests and tools out of the archives Composer downloads, so an installation holds only the PHP code and the binary.

Packagist lists the package from `https://github.com/nforst/fagus` and updates it on every push and tag through GitHub's webhook. Before a release is tagged, a project can try a branch through a `vcs` repository:

```json
"repositories": [{ "type": "vcs", "url": "https://github.com/nforst/fagus" }]
```

A branch keeps the pinned binaries of the last release: the plugin installs that release's binary and warns that it may not know what the branch added. Set `FAGUS_BINARY` to a binary built from the same commit. Only a copy whose `Release::BINARIES` is empty has no binary to install.
