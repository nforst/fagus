# Getting started

Fagus turns HTML and CSS into PDF the way Chrome prints them. It is one Composer package,
`nforst/fagus`, with a native engine that Composer downloads for your platform. There is no
browser, Node or Docker image to install.

## Requirements

- PHP 8.3 or newer, with `proc_open()` allowed (it is unless `disable_functions` lists it).
- One of these platforms:

  | System | Architectures |
  |---|---|
  | Linux with glibc (2.17 or newer) | x64, arm64 |
  | Linux with musl (Alpine) | x64, arm64 |
  | macOS | x64 (Intel), arm64 (Apple silicon) |
  | Windows | x64 |
  | FreeBSD (14 or newer) | x64 |

- For the Laravel integration: Laravel 13. Older versions of Laravel conflict with the package.

Every platform prints the same PDF from the same input.

## Install the package

```sh
composer require nforst/fagus
```

The package contains a Composer plugin. Composer asks once whether you trust it:

```text
Do you trust "nforst/fagus" to execute code and wish to enable it now? (writes "allow-plugins" to composer.json) [y,n,d,?]
```

Answer `y`. Composer then writes this to your `composer.json`, which you commit:

```json
"config": {
    "allow-plugins": {
        "nforst/fagus": true
    }
}
```

After every `composer install` and `composer update`, the plugin installs the `fagus` binary of
the package's release for the platform Composer runs on:

1. It works out the target, such as `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-musl` or
   `aarch64-apple-darwin`.
2. It downloads `fagus-<target>` from the GitHub release of the package's version, or takes it
   from Composer's cache if it was downloaded before.
3. It checks the file's SHA-256 against the sum the package lists for that release
   (`php/fagus/src/Internal/Release.php`). A file with another sum is not installed.
4. It writes the binary into the package (`vendor/nforst/fagus/php/fagus/bin/fagus`), where the
   engine looks for it.

If the binary cannot be installed, Composer does not fail: it prints a warning that starts with
`fagus:` and goes on. Run `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`)
to try again; [troubleshooting.md](troubleshooting.md) lists the messages. With
`FAGUS_INSTALL_STRICT=1`, Composer fails instead
([deployment.md](deployment.md#fail-the-build-without-the-binary)). Servers without access
to GitHub, Docker builds and binaries of your own are covered in [deployment.md](deployment.md).

## Your first PDF in PHP

```php
<?php

require 'vendor/autoload.php';

use Fagus\Fagus;

$html = <<<'HTML'
<!doctype html>
<html lang="en">
<head>
  <title>Invoice 2026-0042</title>
  <style>
    @page {
      size: A4;
      margin: 20mm 18mm;
      @bottom-right { content: "Page " counter(page) " of " counter(pages); font-size: 9pt; }
    }
    body { font-family: sans-serif; }
    table { width: 100%; border-collapse: collapse; }
    th, td { border-bottom: 1px solid #ddd; padding: 4px; text-align: left; }
  </style>
</head>
<body>
  <h1>Invoice 2026-0042</h1>
  <table>
    <thead><tr><th>Item</th><th>Amount</th></tr></thead>
    <tbody><tr><td>Consulting</td><td>1,200.00 EUR</td></tr></tbody>
  </table>
</body>
</html>
HTML;

$pdf = Fagus::html($html)->save('invoice.pdf');

echo $pdf->pages();          // 1
print_r($pdf->diagnostics()); // CSS that was ignored, resources that are missing
```

The document describes itself, as in a browser: the page size and margins come from `@page`, the
PDF's title from `<title>` and its language from `<html lang>`. Without an `@page` rule, pages are
A4 (Chrome uses the paper size of its printer settings, often Letter).

Text is set in Inter, the font built into Fagus, until you add fonts of your own. Fagus has no
serif font: text without a `font-family`, which a browser sets in Times, comes out in Inter too,
and in the first font of your font directories once there are any. Put font files into a font
directory and name the family in your CSS, or say which families `serif`, `sans-serif` and
`monospace` stand for ([fonts.md](fonts.md#set-the-generic-families)).

The first document starts the engine's worker process, which takes a few milliseconds; the
following documents of the same PHP process render on the running worker.

### Images, stylesheets and fonts from files

Fagus reads no files unless you name a directory. Relative URLs resolve against the base
directory, and only files inside it are read:

```php
$pdf = Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf')   // <img src="logo.png">, <link href="invoice.css">
    ->render();
```

URLs with `http:` or `https:` are never fetched. [resources.md](resources.md) describes where
resources come from and how to hand them over.

## Your first PDF in Laravel

Laravel 13 discovers the package on its own: the `Fagus` facade, Blade views, responses and
filesystem disks work after `composer require`. Write the document as a Blade view, for example
`resources/views/invoices/show.blade.php`:

```blade
<!doctype html>
<html lang="en">
<head>
  <title>Invoice {{ $invoice->number }}</title>
  <style>
    @page { size: A4; margin: 20mm 18mm; }
    body { font-family: sans-serif; }
  </style>
</head>
<body>
  <img src="/images/logo.png" alt="ACME">
  <h1>Invoice {{ $invoice->number }}</h1>
</body>
</html>
```

and return it from a controller:

```php
use App\Models\Invoice;
use Fagus\Laravel\Facades\Fagus;

class InvoicePdfController
{
    public function __invoke(Invoice $invoice)
    {
        return Fagus::view('invoices.show', ['invoice' => $invoice])
            ->name("invoice-{$invoice->number}.pdf");   // shown in the browser
    }
}
```

The browser shows the PDF. `->download()` sends it as a download, and `->store()` writes it to a
filesystem disk:

```php
Fagus::view('invoices.show', ['invoice' => $invoice])->download('invoice.pdf');
Fagus::view('invoices.show', ['invoice' => $invoice])->store('invoices/2026-0042.pdf', 's3');
```

In Laravel, relative URLs resolve against `public/`, so `/images/logo.png` is
`public/images/logo.png`. Publish the configuration to change this and the fonts:

```sh
php artisan vendor:publish --tag=fagus-config
```

Use the facade `Fagus\Laravel\Facades\Fagus` in Laravel, not the class `Fagus\Fagus`: the facade
renders with the engine and the settings of `config/fagus.php`.

## Where to go next

- [php.md](php.md): every method of the PHP API.
- [laravel.md](laravel.md): views, responses, disks, queues, Octane and the configuration.
- [resources.md](resources.md): images, stylesheets, fonts and frames, and what Fagus never reads.
- [paged-media.md](paged-media.md): page sizes, margins, headers, footers and page numbers.
- [css.md](css.md): the CSS that Fagus renders.
- [fonts.md](fonts.md): web fonts, font directories and system fonts.
- [pdf.md](pdf.md): PDF/A, PDF/UA, attachments and ZUGFeRD/Factur-X invoices.
- [deployment.md](deployment.md): Docker, Alpine, servers without GitHub access and worker processes.
- [troubleshooting.md](troubleshooting.md): error messages, diagnostics and differences from Chrome.
