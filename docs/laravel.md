# Laravel

The package `nforst/fagus` includes its Laravel integration (`Fagus\Laravel\`, in
`php/laravel/src`). Laravel 13 discovers it on its own: `composer require nforst/fagus` registers
the service provider, the `Fagus` facade, the configuration and the `fagus:install` command.

```php
use Fagus\Laravel\Facades\Fagus;

// In a controller: shown in the browser.
return Fagus::view('invoices.show', ['invoice' => $invoice])->name('invoice.pdf');

// As a download, on a disk, or as bytes.
return Fagus::view('invoices.show', $data)->download('invoice.pdf');
Fagus::view('invoices.show', $data)->pdfA('3b')->store('invoices/2026-0042.pdf', 's3');
$bytes = Fagus::html($html)->render()->bytes();
```

Installation is described in [getting-started.md](getting-started.md).

## The facade

`Fagus\Laravel\Facades\Fagus` (also registered as the alias `Fagus`) resolves the service
`fagus`, an instance of `Fagus\Laravel\PdfFactory`. It has four methods:

| Method | Returns |
|---|---|
| `Fagus::view(string $view, array $data = [], array $mergeData = [])` | A `PendingPdf` from a Blade view, rendered with `view()->make($view, $data, $mergeData)`. |
| `Fagus::html(string $html)` | A `PendingPdf` from HTML. |
| `Fagus::xhtml(string $xml)` | A `PendingPdf` from XHTML, parsed as XML (see [php.md](php.md#fagus)). |
| `Fagus::engine()` | The application's `Fagus\Engine`, for `addFont()` and `version()`. |

Every `PendingPdf` starts with the base directory, the root and the tagging of the
configuration.

Do not confuse the facade with the plain PHP class `Fagus\Fagus`. The class does not read
`config/fagus.php` and renders with an engine of its own; in Laravel, import
`Fagus\Laravel\Facades\Fagus`.

You can inject the factory instead of using the facade:

```php
use Fagus\Laravel\PdfFactory;

final class InvoiceMailer
{
    public function __construct(private PdfFactory $pdfs) {}

    public function attachment(Invoice $invoice): string
    {
        return $this->pdfs->view('invoices.show', ['invoice' => $invoice])->render()->bytes();
    }
}
```

`Fagus\Engine` is bound as a singleton too, so `app(Engine::class)` is the same engine as
`Fagus::engine()`.

## `PendingPdf`

`Fagus\Laravel\PendingPdf` wraps a `Fagus\Document` together with the engine. Like the document,
it is immutable: every method returns a changed copy.

### Document methods

These forward to `Fagus\Document` and take the same arguments; [php.md](php.md#document)
describes each one:

`baseDirectory()`, `root()`, `stylesheet()`, `resource()`, `resolver()`, `pdfA()`, `tagged()`,
`accessible()`, `forms()`, `attach()`, `facturX()`, `encrypt()`, `created()`, `sign()`, `strict()`,
`reportLayout()`.

```php
use Fagus\Conformance;
use Illuminate\Support\Facades\Vite;

Fagus::view('invoices.show', ['invoice' => $invoice])
    ->stylesheet(Vite::content('resources/css/pdf.css'))
    ->resource('logo.png', $invoice->company->logo)
    ->pdfA(Conformance::PdfA3b)
    ->facturX($invoice->toFacturX());
```

A payslip that needs a password to open and may be printed, but not changed:

```php
use Fagus\Permission;

return Fagus::view('payslips.show', ['payslip' => $payslip])
    ->encrypt(userPassword: $employee->pdf_password, permissions: [Permission::Print])
    ->name("payslip-{$payslip->month}.pdf")
    ->download();
```

Add `accessible()` for a payslip that screen readers read too: PDF/UA allows encryption, and
assistive technology may always extract the text, whatever the permissions.

`sign()` takes a `Fagus\Signature` ([php.md](php.md#signature)); a key from a secrets store
comes as bytes with `Signature::fromPkcs12Data()`. A signed invoice, with the timestamp of a
public time-stamping authority through Laravel's HTTP client:

```php
use Fagus\Signature;
use Illuminate\Support\Facades\Http;

$signature = Signature::fromPkcs12(storage_path('app/private/signing.p12'), config('services.signing.password'))
    ->reason('Rechnung')
    ->timestamp(fn (string $digest): string => Http::withBody(Signature::timestampRequest($digest), 'application/timestamp-query')
        ->post('https://freetsa.org/tsr')
        ->throw()
        ->body());

return Fagus::view('invoices.show', ['invoice' => $invoice])
    ->pdfA('3b')
    ->sign($signature)
    ->download("rechnung-{$invoice->number}.pdf");
```

`certify()`, `visibleOn()`, `revocation()` and `archiveTimestamp()` make a certified, visible or
long-term signature ([pdf.md](pdf.md#sign-the-pdf)); the revocation callback can fetch OCSP
responses and CRLs with `Http` as well.

### `name(string $filename): PendingPdf`

The file name for the browser and for downloads. `document.pdf` by default.

### `inline(?string $filename = null): Response`

Renders the PDF and returns a response that shows it in the browser
(`Content-Disposition: inline`). The file name is `$filename`, else the one from `name()`.

### `download(?string $filename = null): Response`

Renders the PDF and returns a response that downloads it (`Content-Disposition: attachment`).

```php
return Fagus::view('invoices.show', $data)->download("invoice-{$invoice->number}.pdf");
```

Both responses are `Illuminate\Http\Response` with `Content-Type: application/pdf`. File names
with characters outside ASCII are sent as `filename*=utf-8''…` with an ASCII fallback, so
`Übersicht März.pdf` arrives as written.

### `toResponse($request): Response`

`PendingPdf` implements `Responsable`: returned from a controller or route, it renders the PDF and
shows it inline, as `inline()` does.

```php
Route::get('/invoices/{invoice}/pdf', fn (Invoice $invoice) => Fagus::view('invoices.show', [
    'invoice' => $invoice,
])->name("invoice-{$invoice->number}.pdf"));
```

### `store(string $path, ?string $disk = null): Pdf`

Renders the PDF and writes it to a filesystem disk, the default disk if `$disk` is `null`.
Returns the `Fagus\Pdf`.

```php
$pdf = Fagus::view('invoices.show', $data)->store("invoices/{$invoice->number}.pdf", 's3');
$pdf->pages();
```

### `save(string $path): Pdf`

Renders the PDF and writes it to a local path.

### `render(): Pdf`

Renders the PDF and returns the `Fagus\Pdf` with its bytes, page count, diagnostics and timings
(see [php.md](php.md#pdf)).

```php
$pdf = Fagus::view('reports.monthly', $data)->render();

Mail::to($customer)->send(new MonthlyReport($pdf->bytes()));
```

With `reportLayout()`, the PDF also tells where the elements with an `id` ended up: their pages
and boxes, and which rows of a table went on which page
([php.md](php.md#find-where-elements-landed)). Render twice to adjust a view to its own layout:

```php
$pdf = Fagus::view('invoices.show', ['invoice' => $invoice])->reportLayout()->render();
$totals = $pdf->layout()->element('totals');

// The totals start a page of their own: the view puts the last two lines on it as well.
if ($totals !== null && $totals->firstPage() > 1 && $totals->boxes[0]->y < 100) {
    $pdf = Fagus::view('invoices.show', ['invoice' => $invoice, 'keepLastLines' => 2])->render();
}

Storage::put("invoices/{$invoice->number}.pdf", $pdf->bytes());
```

### `document(): Document`

The underlying `Fagus\Document`, for code that works with the plain PHP API.

## Queues

Render PDFs in a job when they take long or when nobody waits for them:

```php
use Fagus\Laravel\Facades\Fagus;
use Illuminate\Contracts\Queue\ShouldQueue;
use Illuminate\Foundation\Queue\Queueable;

final class StoreInvoicePdf implements ShouldQueue
{
    use Queueable;

    public function __construct(public Invoice $invoice) {}

    public function handle(): void
    {
        Fagus::view('invoices.show', ['invoice' => $this->invoice])
            ->pdfA('3b')
            ->store("invoices/{$this->invoice->number}.pdf", 's3');
    }
}
```

The engine is a singleton. In `php artisan queue:work` (and in Horizon's workers), the worker
process starts with the first PDF and serves every following job of that queue worker, so a job
pays only for rendering. `php artisan queue:listen` boots the application for every job, which
starts a new worker process each time.

After deploying a new version of the package, restart the queue workers
(`php artisan queue:restart`) so that they use the new binary.

A worker process that has rendered large documents keeps much of the memory they needed. In a
long-running queue worker, let the engine replace it now and then with `FAGUS_MAX_DOCUMENTS` or
`FAGUS_MAX_MEMORY` ([deployment.md](deployment.md#restart-workers)).

## Octane and FrankenPHP

Under PHP-FPM, the engine lives as long as the request: the first PDF of each request starts the
worker process (a few milliseconds), and the request's end stops it.

Under Octane (Swoole, RoadRunner or FrankenPHP), the application boots once per Octane worker.
Octane gives each request a copy of the application's container and throws away what a request
resolved when it ends: an engine resolved in a request would start a worker process and stop it
again at the request's end. So when Octane is installed, the service provider adds
`Fagus\Engine` to the `warm` services of `config/octane.php`, which Octane resolves when its
worker boots and shares with every request. One engine, and its worker process, then serves every
PDF of the Octane worker; there is nothing to configure. Listing `\Fagus\Engine::class` in `warm`
yourself does no harm.

More on worker processes, memory and timeouts: [deployment.md](deployment.md#worker-processes).

## Use Fagus with spatie/laravel-pdf

An application that makes its PDFs with [spatie/laravel-pdf](https://github.com/spatie/laravel-pdf)
(version 2) can render them with Fagus and keep its code. When both packages are installed, the
service provider registers Fagus as laravel-pdf's `fagus` driver; there is nothing to configure.

```sh
composer require nforst/fagus spatie/laravel-pdf
```

Make it the driver of every PDF in `.env`, or choose it for single PDFs with `driver('fagus')`:

```sh
LARAVEL_PDF_DRIVER=fagus
```

```php
use Spatie\LaravelPdf\Facades\Pdf;

return Pdf::view('invoices.show', ['invoice' => $invoice])
    ->driver('fagus')
    ->format('a4')
    ->margins(20, 15, 20, 15)
    ->footerView('invoices.footer')
    ->name("invoice-{$invoice->number}.pdf");
```

`save()`, `disk()`, `download()`, `inline()`, `base64()`, `saveQueued()`, `cache()`, mail
attachments and `Pdf::fake()` belong to laravel-pdf and work as with any driver. The driver
renders with the application's engine and starts every document with the `base_directory`, `root`
and `tagged` of `config/fagus.php` (see [Configuration](#configuration)), so a view finds its
images and fonts as with `Fagus::view()`. laravel-pdf's `@inlinedImage` works too: it writes the
image into the view as a `data:` URL.

### Page size and margins

`format()`, `paperSize()`, `landscape()`, `portrait()` and `margins()` become an `@page` rule that
the driver adds after the document's own stylesheets, as `stylesheet()` does:

| laravel-pdf | `@page` |
|---|---|
| `format('a4')` | `size` with the dimensions laravel-pdf lists for the format: `letter`, `legal`, `tabloid`, `ledger` and `a0` to `a6`. Other names throw a `CouldNotGeneratePdf`. |
| `paperSize(57, 500, 'mm')` | `size: 57mm 500mm` |
| `landscape()`, `portrait()` | Turn the size of `format()` or `paperSize()`: landscape makes the page wider than tall. Alone, they write `size: landscape` or `size: portrait`, which turns the default page (A4), not a size the document sets. |
| `margins(20, 15, 20, 15)` | `margin: 20mm 15mm 20mm 15mm` |

The units are `px`, `in`, `cm` and `mm` (laravel-pdf's `Unit`), and `pt` and `pc`. Others throw a
`CouldNotGeneratePdf`.

**Precedence.** What the builder sets wins over the document's `@page` rule: the added rule comes
later and has the same specificity. What the builder does not set adds nothing, so the document's
`@page` rule applies. Rules of the document that select pages, such as `@page :first` or a named
page, are more specific and still win on their pages.

**Defaults.** Without `format()` and without a `size` in the document, the page is A4, where the
Chrome-based drivers print Letter; call `format('letter')` for US paper. Without `margins()` and
without a `margin` in the document, the margins are 28pt (about 1cm). See
[paged-media.md](paged-media.md#set-the-page-size-and-margins).

### Header and footer

`headerView()`, `headerHtml()`, `footerView()` and `footerHtml()` become running elements: the
driver puts the header and the footer at the start of the document's body and shows them in the
`@top-center` and `@bottom-center` margin boxes of every page, as wide as the page between the left
and right margins (see [paged-media.md](paged-media.md#put-html-in-the-header-and-footer)).
laravel-pdf's `@pageNumber` and `@totalPages` show the page number and the number of pages.

```blade
{{-- resources/views/invoices/footer.blade.php --}}
<div style="font-size: 8pt; text-align: right">Page @pageNumber of @totalPages</div>
```

Chrome renders the header and footer as documents of their own; Fagus renders them as part of the
document:

- They inherit the fonts and styles of the document. The explicit font size and the padding that
  the Chrome-based drivers need are not needed.
- Their `<style>` elements apply to the whole document: give their rules selectors that only match
  the header and footer. A header or footer view that is a whole document (`<!doctype html>`,
  `<head>`, `<body>`) is reduced to its content and styles.
- They sit in the page margins: make the top and bottom margins high enough for them, with
  `margins()` or the document's `@page` rule.

For more than one header or footer, such as a different one on the first page, write the margin
boxes in the document's CSS instead.

### Other options

| laravel-pdf | With the `fagus` driver |
|---|---|
| `tagged()` | Tags the PDF. Without it, the `tagged` key of the configuration decides; it is on by default. |
| `documentOutline()` | Not needed: Fagus always writes the outline from the headings (see [pdf.md](pdf.md)). |
| `meta()`, `encrypt()` | laravel-pdf writes them into the PDF after the driver, as for every driver. |
| `scale()` | Throws a `CouldNotGeneratePdf`. Use the CSS property `zoom` instead, such as `html { zoom: 0.8 }`. `scale(1)` is accepted. |
| `pageRanges()` | Throws a `CouldNotGeneratePdf`: Fagus renders every page. |
| `waitUntilReady()` | laravel-pdf throws a `CouldNotGeneratePdf`, as Fagus runs no JavaScript. |
| `withBrowsershot()`, `onLambda()` | Ignored, as laravel-pdf ignores them for every driver but Browsershot. |

The driver discards the diagnostics. While you build a template, render it with `Fagus::view()` to
see them (see [Diagnostics and errors](#diagnostics-and-errors)), and use `Fagus::view()` for what
laravel-pdf has no option for: PDF/A, PDF/UA, Factur-X and attachments.

## Configuration

Publish the configuration file to change it:

```sh
php artisan vendor:publish --tag=fagus-config
```

This writes `config/fagus.php`. Every key, with its environment variable and default:

| Key | Environment variable | Default | Meaning |
|---|---|---|---|
| `binary` | `FAGUS_BINARY` | `null` | The path of the `fagus` binary. If `null`: the one the package installed (with Composer or `php artisan fagus:install`), else the one on the `PATH`. A path that is set must exist. |
| `fonts` | – | `[resource_path('fonts')]` | Directories with fonts for every document: TTF, OTF, TTC, OTC, WOFF and WOFF2 files, in the directory and its subdirectories. Directories that do not exist are skipped, so the default works without a `resources/fonts`; a plain `Fagus\Engine` throws an `EngineException` for them instead. |
| `system_fonts` | `FAGUS_SYSTEM_FONTS` | `false` | Whether the fonts installed on the machine serve too, as in Chrome. Off, so that documents look the same on every machine. |
| `locale` | `FAGUS_LOCALE` | `null` | The language of the texts that form controls show of their own (button labels, date formats) and of the PDF where a document gives no `lang`, such as `de-DE`. American English if `null`. |
| `families` | – | `['sans-serif' => null, 'serif' => null, 'monospace' => null]` | The families that the generic families name. |
| `base_directory` | – | `public_path()` | The directory that relative URLs in documents resolve against. Only files inside it, or inside `root`, are read. |
| `root` | – | `null` | A larger directory that contains `base_directory`, for documents whose URLs lead out of it (`../fonts/…`). URLs that start with `/` resolve against it. |
| `tagged` | – | `true` | Whether PDFs are tagged with the logical structure that screen readers read. Single documents can say otherwise with `tagged()` or ask for PDF/UA with `accessible()`. |
| `timeout` | – | `60` | Seconds to wait for each answer of the worker before it is stopped. Not on Windows, where PHP cannot wait for a pipe with a time limit. |
| `max_documents` | `FAGUS_MAX_DOCUMENTS` | `null` | The documents a worker process renders before it is replaced with a new one. No limit if `null` ([deployment.md](deployment.md#restart-workers)). |
| `max_memory` | `FAGUS_MAX_MEMORY` | `null` | Megabytes of resident memory: a worker process that has more after a document is replaced with a new one (Linux, macOS and FreeBSD). No limit if `null`. |
| `memory_budget` | `FAGUS_MEMORY_BUDGET` | `null` | Megabytes one document may take, all its parts together; one that needs more throws a `RenderException` (`limit.memory`). 2048 if `null`, none if `0` ([php.md](php.md#limits)). |
| `time_limit` | `FAGUS_TIME_LIMIT` | `null` | Seconds one document may take in all; one that takes longer throws a `RenderException` (`limit.time`) and the worker goes on. A tenth less than `timeout` if `null`, none if `0`. |
| `sandbox` | `FAGUS_SANDBOX` | `true` | Whether the worker renders in its sandbox on Linux, without network, programs or writing files ([cli.md](cli.md#sandbox)). Turn it off only to debug. |

`binary`, `fonts`, `system_fonts`, `locale`, `families`, `timeout`, `max_documents`, `max_memory`, `memory_budget`, `time_limit` and `sandbox` configure the engine;
`base_directory`, `root` and `tagged` apply to each new `PendingPdf`.

```php
// config/fagus.php
return [
    'binary' => env('FAGUS_BINARY'),
    'fonts' => [resource_path('fonts')],
    'system_fonts' => env('FAGUS_SYSTEM_FONTS', false),
    'locale' => env('FAGUS_LOCALE', 'de-DE'),
    'families' => [
        'sans-serif' => 'Inter',
        'serif' => null,
        'monospace' => 'JetBrains Mono',
    ],
    'base_directory' => resource_path('pdf'),
    'root' => resource_path(),
    'tagged' => true,
    'timeout' => 30,
];
```

With `base_directory` at `public_path()`, a view refers to files of `public/` as a page of the
site would: `<img src="/images/logo.png">` reads `public/images/logo.png`. Absolute URLs from
`asset()`, `url()` or `@vite` start with `http` and are not loaded; see
[resources.md](resources.md#laravel-assets-and-vite) for how to use them.

## `php artisan fagus:install`

Installs the `fagus` binary of the package's release for the platform PHP runs on, as the
Composer plugin does after `composer install`:

```sh
php artisan fagus:install              # where the engine finds it
php artisan fagus:install /opt/fagus   # into another directory
```

It takes `FAGUS_DOWNLOAD_URL` and `FAGUS_GITHUB_TOKEN` (or `GITHUB_TOKEN`) from the environment,
like `vendor/bin/fagus-install`. A binary installed into another directory must be named in
`FAGUS_BINARY` (or the `binary` key) or lie on the `PATH`. See [deployment.md](deployment.md).

## Diagnostics and errors

`render()`, `store()` and `save()` return the `Fagus\Pdf`, whose `diagnostics()` lists CSS that
was ignored and resources that were not found. Log them while you build a template:

```php
$pdf = Fagus::view('invoices.show', $data)->render();

foreach ($pdf->diagnostics() as $diagnostic) {
    Log::warning((string) $diagnostic);
}
```

`inline()` and `download()` render and discard the diagnostics. Errors are exceptions
(`Fagus\Exception\RenderException`, `EngineException`), which Laravel reports as usual. Warnings
of the engine, such as a binary of another release than the package, go to Laravel's log. A
`RenderException` keeps the diagnostics found before the document was refused in its
`diagnostics()`. See [troubleshooting.md](troubleshooting.md).

## Testing

The package has no fake. PDFs render in a few milliseconds, so tests can render them with the real
binary: the Composer plugin installs it with `composer install` on the developer's machine and in
CI, as long as `allow-plugins` permits it.

Assert on the response of a route:

```php
public function test_the_invoice_pdf_is_shown(): void
{
    $invoice = Invoice::factory()->create();

    $response = $this->get("/invoices/{$invoice->id}/pdf");

    $response->assertOk();
    $response->assertHeader('Content-Type', 'application/pdf');
    $this->assertStringStartsWith('%PDF-', $response->getContent());
}
```

Fake the disk to check `store()`:

```php
Storage::fake('s3');

StoreInvoicePdf::dispatchSync($invoice);

Storage::disk('s3')->assertExists("invoices/{$invoice->number}.pdf");
```

Make a template fail on every ignored declaration and missing resource with `strict()`:

```php
public function test_the_invoice_template_is_clean(): void
{
    $pdf = Fagus::view('invoices.show', ['invoice' => Invoice::factory()->make()])
        ->strict()
        ->render();

    $this->assertSame(1, $pdf->pages());
}
```

To render with a binary of your own in tests, set `FAGUS_BINARY` in `phpunit.xml`:

```xml
<php>
    <env name="FAGUS_BINARY" value="/usr/local/bin/fagus"/>
</php>
```
