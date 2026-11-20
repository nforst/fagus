# Deployment

Fagus needs its `fagus` binary next to PHP on every server that renders PDFs. The Composer plugin
installs it with `composer install`; this page covers builds, servers and processes where that
needs a thought.

## Install the binary in CI and Docker builds

The plugin runs after `composer install` and `composer update` and installs the binary for the
platform Composer runs on, checked against the SHA-256 that the package lists for its release.
Builds run Composer without asking, so two things must be in place:

1. **The plugin is allowed in `composer.json`.** Composer asks interactively the first time; in a
   build it fails instead:

   ```text
   nforst/fagus contains a Composer plugin which is blocked by your allow-plugins config. You may add it to the list if you consider it safe.
   ```

   Commit the entry that answering `y` writes, or add it yourself:

   ```sh
   composer config allow-plugins.nforst/fagus true
   ```

2. **The build can reach GitHub**, where the binaries are attached to the release. Installing
   from a private fork also needs a token that can read it: Composer's
   (`github-oauth.github.com` in `auth.json` or `COMPOSER_AUTH`), else `FAGUS_GITHUB_TOKEN` or
   `GITHUB_TOKEN`.

`--no-scripts` does not stop the plugin; `--no-plugins` does. After `--no-plugins`, run
`vendor/bin/fagus-install`.

### Fail the build without the binary

If the binary cannot be installed, the plugin prints a warning and Composer succeeds; the engine
then fails at the first PDF. To let a build or deployment fail at `composer install` instead,
set `FAGUS_INSTALL_STRICT=1` in Composer's environment:

```sh
FAGUS_INSTALL_STRICT=1 composer install --no-dev --no-interaction
```

```text
fagus: There is no fagus binary for OpenBSD on amd64. Build it from source with `cargo build --release` and set FAGUS_BINARY to it.
fagus: Composer fails because FAGUS_INSTALL_STRICT is set. `vendor/bin/fagus-install` tries again.
```

Composer then exits with status 1, also for a development version without binaries. To make it
the rule for a project, put it into the root `composer.json`; `FAGUS_INSTALL_STRICT=0` turns it
off again, for example on a machine that uses a binary of its own:

```json
"extra": {
    "fagus": {
        "install-strict": true
    }
}
```

The variable belongs to the environment that runs Composer (the build or deployment script), not
to Laravel's `.env`, which Composer does not read.

The plugin keeps downloaded binaries in Composer's cache directory (`<cache-dir>/fagus/<version>/`),
so a build with a cached Composer directory downloads each binary once.

### Docker

Run Composer in the image that runs the application, or install the binary there afterwards. A
binary installed in another image is for that image's platform, which may differ in architecture
or C library from the image that runs PHP.

```dockerfile
FROM php:8.4-fpm

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

WORKDIR /app
COPY . .
RUN composer install --no-dev --no-interaction --optimize-autoloader
```

With a multi-stage build, where Composer runs in a stage of its own, install the binary in the
final stage:

```dockerfile
FROM composer:2 AS vendor
WORKDIR /app
COPY composer.json composer.lock ./
RUN composer install --no-dev --no-interaction --no-plugins --optimize-autoloader

FROM php:8.4-fpm-alpine
WORKDIR /app
COPY --from=vendor /app/vendor ./vendor
COPY . .
RUN vendor/bin/fagus-install
```

`vendor/bin/fagus-install` keeps a binary that is already there with the right SHA-256 and
replaces one of another platform.

## Alpine and other musl systems

Linux with musl has binaries of its own (`x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`), linked statically. The installer picks them when it finds
`/lib/ld-musl-*.so.1` or `/etc/alpine-release`; nothing needs to be configured. The glibc
binaries are linked against glibc 2.17, so they run on every maintained glibc distribution.

The binary needs no system libraries and no installed fonts. With `systemFonts: true`, it reads
the font directories that fontconfig's configuration lists, else `/usr/share/fonts`,
`/usr/local/share/fonts` and `~/.local/share/fonts`; install the font packages you want there
(see [fonts.md](fonts.md)).

## Servers without access to GitHub

Put the files of the release somewhere the server can reach and point `FAGUS_DOWNLOAD_URL` at it:
a directory or a URL that holds the files under their names in the release, `fagus-<target>`:

```text
fagus-x86_64-unknown-linux-gnu
fagus-aarch64-unknown-linux-gnu
fagus-x86_64-unknown-linux-musl
fagus-aarch64-unknown-linux-musl
fagus-x86_64-apple-darwin
fagus-aarch64-apple-darwin
fagus-x86_64-pc-windows-msvc.exe
fagus-x86_64-unknown-freebsd
```

```sh
FAGUS_DOWNLOAD_URL=https://artifacts.example.com/fagus/0.2.0 composer install
FAGUS_DOWNLOAD_URL=/mnt/artifacts/fagus/0.2.0 vendor/bin/fagus-install
```

The binary is installed only if its SHA-256 is the one the package lists for its release,
whatever its source. `FAGUS_DOWNLOAD_URL` applies to the plugin, `vendor/bin/fagus-install` and
`php artisan fagus:install`.

`vendor/bin/fagus-install` takes a directory as its argument to install somewhere else than into
the package; name that path in `FAGUS_BINARY` then:

```sh
vendor/bin/fagus-install /opt/fagus
# The fagus binary is installed at /opt/fagus/fagus.
```

## Use a binary of your own

`FAGUS_BINARY` names a binary that the engine uses instead of the installed one, for example one
built from source:

```sh
cargo build --release -p fagus-cli
export FAGUS_BINARY=/path/to/fagus/target/release/fagus
```

The engine looks for the binary in this order:

1. The `binary` argument of `Engine` (in Laravel, the `binary` key of `config/fagus.php`).
2. `FAGUS_BINARY`.
3. The binary the installer put into the package (now `vendor/nforst/fagus/php/fagus/bin/fagus`,
   `fagus.exe` on Windows; the place is not API and may change in a minor release, so ask
   `Installer::install()` for it or set `FAGUS_BINARY`).
4. `fagus` (`fagus.exe`) on the `PATH`.

A binary named in the first two must exist and be executable, otherwise the engine does not fall
back but throws:

```text
The fagus binary `/usr/local/bin/fagus` could not be started. It does not exist or is not executable.
```

The binary must speak the package's protocol; use the one of the same release. A binary of
another protocol is refused; one of another release with the same protocol works, but ignores
options newer than itself, and the engine writes a warning to the error log
([troubleshooting.md](troubleshooting.md#another-release)).

Under PHP-FPM the pool's environment is cleared by default (`clear_env = yes`), so a
`FAGUS_BINARY` from the shell does not reach PHP. Set it in the pool
(`env[FAGUS_BINARY] = /usr/local/bin/fagus`) or pass the path to the engine. In Laravel,
`FAGUS_BINARY` in `.env` works, also with a cached configuration, as it is read into
`config('fagus.binary')`.

## Windows

- The binary is `fagus.exe` for x64; Windows on ARM is not supported.
- There is no timeout: PHP cannot wait for a pipe with a time limit on Windows, so the engine
  waits for each answer as long as it takes.
- A running binary is locked. Stop the processes that run it (queue workers, Octane) before
  `composer update` replaces it; otherwise the installer reports that it could not write the
  binary.

## FreeBSD

- The binary is `fagus-x86_64-unknown-freebsd`, for amd64, built and tested on FreeBSD 14. It
  prints the same PDF as the other platforms. FreeBSD on arm64 and the other BSDs have no binary:
  build one from source (`cargo build --release -p fagus-cli`) and set `FAGUS_BINARY`.
- There is no [sandbox](cli.md#sandbox): the worker renders with the rights of the PHP process,
  so give documents a root of assets only ([below](#untrusted-documents)). `--memory-limit` and
  `--cpu-limit` are only supported on Linux; limit the process with the system's means instead,
  such as `limits(1)` or `rctl(8)`.
- The worker reports the largest resident size it has had, as on macOS, which `maxMemory`
  compares ([restart workers](#restart-workers)).
- The binary needs no installed fonts. With `systemFonts: true` it reads the directories that
  fontconfig's configuration in `/usr/local/etc/fonts` lists, else `/usr/local/share/fonts`
  and the other directories of Linux ([fonts.md](fonts.md#use-the-fonts-installed-on-the-system)).

On shared hosting:

- PHP must be allowed to start programs: the engine runs the binary with `proc_open()`, which must
  not be in `disable_functions`. `php -r 'var_dump(function_exists("proc_open"));'` prints
  `bool(true)` where it is allowed; ask the host otherwise.
- The binary must be executable where it lies. Where `vendor` is on a file system mounted
  `noexec`, install it into another directory with `vendor/bin/fagus-install /path/to/dir` and
  set `FAGUS_BINARY` to `/path/to/dir/fagus`.
- Where the server cannot reach GitHub, upload the binary of the release and point
  `FAGUS_DOWNLOAD_URL` at its directory ([above](#servers-without-access-to-github)).
- With `FAGUS_INSTALL_STRICT=1`, a deployment that runs `composer install` fails when the binary
  cannot be installed ([above](#fail-the-build-without-the-binary)).

## Worker processes

The PHP package starts the binary as a worker process with the first document and talks to it
through pipes ([protocol.md](protocol.md)). The worker keeps fonts and caches warm and renders one
document after another until its engine is closed or destroyed. HTML, fonts and PDFs travel
through the pipes as they are, of any size.

| Runtime | The worker serves |
|---|---|
| PHP-FPM, mod_php, FrankenPHP's classic mode | the documents of one request; the first document of each request starts it |
| Queue workers (`queue:work`, Horizon) | every document of the queue worker process |
| Octane (Swoole, RoadRunner, FrankenPHP) | every document of the Octane worker: the Laravel integration warms the engine ([laravel.md](laravel.md#octane-and-frankenphp)) |
| FrankenPHP's worker mode without Laravel, long-running scripts | every document, as long as the engine (for example `Fagus::engine()`) is kept |
| CLI scripts | the documents of the script |

Each PHP process that renders has its own worker process. One worker renders one document at a
time, as PHP sends them one after another.

`Engine::close()` stops the worker; the next document starts a new one, which gets the fonts of
`addFont()` again. When a worker times out or exits, the engine stops it and throws an
`EngineException`; the next document starts a new worker.

### Restart workers

A worker that has rendered a large document keeps much of the memory it needed: not in caches,
but in the allocator, which holds freed memory for later rather than handing it back to the
system. After a 100-page invoice, a worker on macOS keeps about 47 MB at rest, and more after many
of them ([bench/README.md](https://github.com/nforst/fagus/blob/main/bench/README.md#memory-a-worker-keeps)).
glibc's allocator on Linux keeps freed memory in its arenas too, but there the worker hands it
back after each document (`malloc_trim`): after 100-page invoices it keeps about 20 MB instead
of 60 to 125 MB. `FAGUS_MALLOC_TRIM=0` in the worker's environment turns that off; musl, as in
Alpine, gives freed memory back by itself. On macOS, `MallocSpaceEfficient=1` or
`MallocLargeCache=0` in the environment of the PHP process (which the worker inherits) keep a
worker at 5 to 12 MB at rest after 21 large invoices instead of about 120 MB, but render them
up to a fifth slower; the engine sets neither. In a queue worker or under Octane, where one worker
lives for hours, the engine can replace it with a new one, which starts small:

```php
new Engine(
    maxDocuments: 500,   // a new worker after 500 documents
    maxMemory: 256,      // or after a document that leaves the worker with more than 256 MB
);
```

In Laravel, set `max_documents` and `max_memory` in `config/fagus.php`, or `FAGUS_MAX_DOCUMENTS`
and `FAGUS_MAX_MEMORY`. The engine closes the worker after the document that reaches a limit;
the next document starts a new one, which gets the fonts of `addFont()` again; the start of a
process costs 3 to 4 ms once. Both are off by default.

`maxMemory` compares the resident size the worker reports with every document
([protocol.md](protocol.md#rendering)): on Linux its size after the document, on macOS and
FreeBSD the largest it has been since it started. The resident size counts the binary's own pages too,
about 20 MB; a limit below what one document needs replaces the worker after every document.
On Windows the worker does not report its memory and only `maxDocuments` applies.

## Speed and memory

Rendering times of invoices on an Apple M3, once the worker runs, unless noted:

| Document | Time |
|---|---|
| One-page invoice, tagged | about 1.5 ms |
| Ten-page invoice, tagged | about 12 ms |
| First document of a process, including the start of the worker | about 8 ms |

On an Apple M2 Pro, `fagus render` of the repository's corpus invoices peaks at about 22 MB of resident memory
for the one-page invoice and 34 MB for the ten-page invoice. The macOS binary itself is about 25 MB
on disk. Large images, many fonts and long documents need more.

Under PHP-FPM, every request that renders pays the start of the worker once. Where many requests
render PDFs, a queue worker or Octane keeps the worker running.

## Threads

The binary compresses the PDF on two threads of its own while it draws the pages, which makes
large documents a few percent faster. `RAYON_NUM_THREADS` sets how many there are:

```sh
RAYON_NUM_THREADS=1 php artisan queue:work
```

More than two gain nothing. With many workers side by side on few cores, one thread each saves
processor time. The worker inherits the environment of the PHP process that starts it; under
PHP-FPM, set the variable in the pool (`env[RAYON_NUM_THREADS] = 1`).

## Timeouts

The engine waits up to `timeout` seconds (60 by default) for each answer of the worker. A worker
that does not answer in time is stopped, and the engine throws:

```text
The fagus worker did not answer within 60 seconds and was stopped.
```

```php
new Engine(timeout: 120.0);   // in Laravel: 'timeout' => 120 in config/fagus.php
```

The limit applies to each answer, not to the whole document: time spent in your resolver
callback does not count. There is no timeout on Windows. Inside the worker, each document has a
time limit of its own, a tenth less than the timeout unless `timeLimit` says otherwise: a
document that takes longer in all ends with a `RenderException` (`limit.time`), and the worker
stays ([php.md](php.md#limits)). For requests, keep the timeout below the
limits of PHP-FPM (`request_terminate_timeout`) and of your web server, or render long documents
in a queue.

## Untrusted documents

Fagus is meant to render HTML, CSS, fonts and images from people you do not trust;
[SECURITY.md](../SECURITY.md#threat-model) says what it promises and what remains. In short:

- **Use a root of assets only.** A document can read every file inside its base directory or
  root and show it in the PDF. Point them at a directory of templates, styles, images and fonts,
  never at the project root, `storage` or a directory with `.env`.
- **Keep the sandbox on.** On Linux the worker renders in a [sandbox](cli.md#sandbox) without
  network, programs or writing files. `Engine::sandbox()` says whether it is in force (`on`,
  `partial` where the kernel lacks Landlock or seccomp, `off` elsewhere). In containers,
  Docker's and Podman's default seccomp profiles allow what the sandbox needs.
- **Limit memory and time.** Each document has a memory budget (2048 MB unless `memoryBudget`
  says otherwise) and a time limit (a tenth less than `timeout`); a document that exceeds one
  ends with a `RenderException` and the worker goes on ([php.md](php.md#limits)). Set the
  budget to what your workers can spare, and still give the worker a memory limit of the system
  (a container's, or `--memory-limit` when you start the binary yourself) above it: the budget is
  an estimate of the large parts, not the process's memory. A worker that is ended for its
  memory is replaced with the next document.

## Verify the binary

The Composer plugin installs only the binary whose SHA-256 the package lists for its release.
A binary you install yourself, from the release or a mirror (`FAGUS_DOWNLOAD_URL`), can be checked
against the release's `SHA256SUMS` and the attestation GitHub keeps of its build:

```sh
gh attestation verify fagus-x86_64-unknown-linux-gnu --repo nforst/fagus
```

Each release also has an SBOM of every binary (`fagus-<target>.cdx.json`, CycloneDX) for
scanners such as Grype or Dependency-Track. [SECURITY.md](../SECURITY.md#verify-a-binary)
describes the checks.

## Reproducible PDFs

The same document gives the same bytes when:

- the creation date is fixed with `created()` (the current time otherwise),
- the fonts are the same, which they are with fonts from `@font-face`, font directories and the
  built-in fonts, and not necessarily with `systemFonts: true`,
- the binary is the same release.

```php
$pdf = Fagus::html($html)
    ->created(new DateTimeImmutable('2026-11-02 12:00:00', new DateTimeZone('UTC')))
    ->render();
```

Every platform prints the same PDF from the same input, so a PDF made on a Mac matches the one
made on a Linux server.
