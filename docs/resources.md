# Resources

A document refers to other files: images, stylesheets, fonts, SVG sprites, the documents of
frames. Fagus loads them only from places you allow. It never goes to the network, never reads a
file outside the directory you name, and runs no scripts. This page describes where resources
come from, and so the security model.

## Where a resource comes from

For every URL in a document, Fagus tries these sources in this order and takes the first that has
the resource:

1. **A `data:` URL**: the data in the URL itself, base64 or percent-encoded.
2. **The resources handed over** with `resource($url, $data)`.
3. **The resolver** set with `resolver()` or `resolveMany()`, a PHP callback.
4. **A file inside the root directory**, if a base directory is set.

If none has it, the resource is missing: the document renders without it and the PDF's
diagnostics say so (`resource.not-loaded`).

```php
$pdf = Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf')
    ->resource('logo.png', $tenant->logo)
    ->resolver(fn (string $url): ?string => $media->find($url))
    ->render();
```

## Read files from a base directory

Without a base directory, Fagus reads no files at all. With one, relative URLs resolve against
it, and only files inside it are read:

```php
Fagus::html('<link rel="stylesheet" href="css/invoice.css"><img src="images/logo.png" alt="ACME">')
    ->baseDirectory(__DIR__.'/resources/pdf');
// reads resources/pdf/css/invoice.css and resources/pdf/images/logo.png
```

A query or fragment (`logo.png?v=2`, `icons.svg#check`) is not part of the file name. Percent
escapes are decoded (`my%20logo.png` is `my logo.png`). Data handed over with `resource()` and the
resolver are looked up by the URL as written first and, for a URL with a fragment
(`icons.svg#check`), by the URL without it next; the file is loaded once for all its fragments.

In Laravel the base directory is `public/` unless `base_directory` in `config/fagus.php` says
otherwise.

## Allow a larger directory with `root()`

The root is the directory that files may come from. It is the base directory unless you set it
with `root()`, and it must contain the base directory. Use it when the document's URLs lead out of
its own directory:

```php
Fagus::html($html)
    ->baseDirectory(__DIR__.'/resources/pdf/invoices')
    ->root(__DIR__.'/resources/pdf');
```

```html
<!-- resources/pdf/invoices/invoice.html -->
<link rel="stylesheet" href="../shared/base.css">  <!-- resources/pdf/shared/base.css -->
```

Anything that leads outside the root is refused: `..` past the root, absolute file paths
(`/etc/passwd` is looked up as `<root>/etc/passwd`), and symbolic links that point outside it.

## URLs that start with `/`

A URL that starts with `/` resolves against the root, as on a web server:

```php
Fagus::html('<img src="/images/logo.png" alt="ACME">')
    ->baseDirectory(__DIR__.'/public/invoices')
    ->root(__DIR__.'/public');   // reads public/images/logo.png
```

Without `root()`, it resolves against the base directory. In Laravel, with the base directory at
`public/`, the same markup works in a page of the site and in the PDF.

## Hand over data with `resource()`

`resource($url, $data)` gives Fagus the data for a URL, so it does not have to be a file: a logo
from the database, a chart drawn in PHP, a stylesheet built at runtime.

```php
Fagus::html('<img src="logo.png" alt="ACME"><img src="chart.svg" alt="Revenue by month">')
    ->resource('logo.png', $company->logo_png)
    ->resource('chart.svg', $chart->toSvg());
```

The URL must match the URL that Fagus asks for:

- In the document, that is the URL as written in the attribute: `<img src="logo.png">` asks for
  `logo.png`, `<img src="./logo.png">` for `./logo.png`.
- In a stylesheet that was loaded from a URL (`<link href="css/main.css">`, `@import`), relative
  URLs are resolved against the stylesheet first, as in browsers: `url(img/bg.png)` in
  `css/main.css` asks for `css/img/bg.png`, and `@import "more.css"` there for `css/more.css`.
- In a frame's document (`<iframe src="frames/page.html">`), relative URLs are resolved against
  the frame's URL: `style.css` in `frames/page.html` asks for `frames/style.css`.

Resources handed over work without a base directory. They are also the only way, besides
`data:` URLs and the resolver, to provide resources for a document that has no files.

## Answer requests with `resolver()`

A resolver is a callback that gets each URL that was not handed over, as described above, and
returns its data or `null`:

```php
Fagus::html($html)
    ->baseDirectory(resource_path('pdf'))
    ->resolver(function (string $url): ?string {
        if (! str_starts_with($url, 'media://')) {
            return null;   // let Fagus look for a file
        }

        return Storage::disk('media')->get(substr($url, strlen('media://')));
    });
```

Fagus asks the resolver before it looks for a file; after `null`, it tries the file in the root.
The resolver sees every URL the document refers to that is not a `data:` URL or handed over,
including `https://…` URLs. It decides what to hand out. Fagus does not go to the network, but
your resolver may, under your own rules: check the host against a list, set a timeout, and keep
in mind that a document from user input can name any URL. The
[recipe below](#remote-images-through-the-resolver-with-a-host-allowlist) shows how.

The resolver is asked once for each URL in a render, whatever it answers, however often the
document names the URL.

The engine waits while the callback runs; the [timeout](deployment.md#timeouts) does not count
that time, as it limits the wait for the worker's answers.

## Remote images through the resolver, with a host allowlist

Fagus fetches nothing, so `<img src="https://cdn.example.com/products/4711.jpg">` is missing from
the PDF until a resolver answers it. A resolver that fetches whatever URL it gets is a hole in
your server (server-side request forgery): a document built from user input can name
`http://169.254.169.254/…`, `http://localhost:9200/…` or a host of your internal network, and
the answer ends up in a PDF the user reads. Fetch only what passes all of these:

- **`https` only.** No `http`, `file`, `ftp`, `gopher` or anything else.
- **A host from your list, compared exactly.** Parse the URL with `parse_url()`, lower the host,
  and look it up with `in_array(…, true)`. No `str_contains()`, no `str_ends_with()`, no regular
  expression: `cdn.example.com.evil.test` and `evil.test/cdn.example.com` pass those. Refuse URLs
  with a user name or a port.
- **No redirects.** An allowed host that redirects to `http://10.0.0.5/` leads the request where
  the list does not. Turn redirects off and take only a `200`.
- **A timeout.** The render waits for the resolver.
- **A size limit**, enforced while the data arrives.

The list holds the names of hosts you trust to serve images: your CDN, your storage bucket. An IP
address, `localhost` or an internal host never belongs on it, and neither does a host where users
upload what they like under any path, unless you check the path too.

In plain PHP, with the cURL extension:

```php
use Fagus\Fagus;

/**
 * The data at an https URL of one of the hosts, or null.
 *
 * @param  list<string>  $hosts
 */
function fetch_allowed(string $url, array $hosts, int $limit = 5 * 1024 * 1024): ?string
{
    $parts = parse_url($url);
    if (! is_array($parts)
        || strtolower($parts['scheme'] ?? '') !== 'https'
        || ! in_array(strtolower($parts['host'] ?? ''), $hosts, true)
        || isset($parts['user']) || isset($parts['pass']) || isset($parts['port'])) {
        return null;
    }
    $data = '';
    $curl = curl_init($url);
    curl_setopt_array($curl, [
        CURLOPT_PROTOCOLS => CURLPROTO_HTTPS,
        CURLOPT_FOLLOWLOCATION => false,
        CURLOPT_CONNECTTIMEOUT => 3,
        CURLOPT_TIMEOUT => 5,
        // Returning another number than the length of the chunk stops the transfer.
        CURLOPT_WRITEFUNCTION => function ($curl, string $chunk) use (&$data, $limit): int {
            $data .= $chunk;

            return strlen($data) > $limit ? 0 : strlen($chunk);
        },
    ]);
    $found = curl_exec($curl) === true && curl_getinfo($curl, CURLINFO_RESPONSE_CODE) === 200;

    return $found ? $data : null;
}

$hosts = ['cdn.example.com', 'media.example.com'];

$pdf = Fagus::html($html)
    ->resolver(fn (string $url): ?string => fetch_allowed($url, $hosts))
    ->render();
```

In Laravel, with the HTTP client:

```php
use Fagus\Laravel\Facades\Fagus;
use Illuminate\Support\Facades\Http;

Fagus::view('catalog.show', $data)->resolver(function (string $url): ?string {
    $hosts = ['cdn.example.com', 'media.example.com'];
    $limit = 5 * 1024 * 1024;
    $parts = parse_url($url);
    if (! is_array($parts)
        || strtolower($parts['scheme'] ?? '') !== 'https'
        || ! in_array(strtolower($parts['host'] ?? ''), $hosts, true)
        || isset($parts['user']) || isset($parts['pass']) || isset($parts['port'])) {
        return null;
    }

    return rescue(function () use ($url, $limit): ?string {
        $response = Http::withoutRedirecting()
            ->connectTimeout(3)
            ->timeout(5)
            ->withOptions(['progress' => function (int $total, int $loaded) use ($limit): void {
                if ($loaded > $limit) {
                    throw new RuntimeException('The image is too large.');
                }
            }])
            ->get($url);

        return $response->status() === 200 ? $response->body() : null;
    }, report: false);
});
```

Everything the resolver refuses or fails to fetch is `null`: the image is left out, and the
diagnostics name it as `resource.not-loaded`. An image that must not be missing is better caught
with `strict()` in a test than by a resolver that throws.

A resolver fetches one URL after the other, and the render waits for each. For a document with
many remote images, fetch them before the render, as the next section shows, or answer many
URLs at once with [`resolveMany()`](#answer-many-requests-at-once-with-resolvemany).

## Fetch remote images before the render

Where the application knows the URLs before it renders, such as the product images of a catalog,
it can fetch them all at once and hand them over with `resource()`. Fagus then asks for nothing,
and fifty images take as long as the slowest one, not as long as all of them together.

In Laravel, `Http::pool()` sends the requests concurrently:

```php
use Fagus\Laravel\Facades\Fagus;
use Illuminate\Http\Client\Pool;
use Illuminate\Http\Client\Response;
use Illuminate\Support\Facades\Http;

$hosts = ['cdn.example.com', 'media.example.com'];

// The URLs as the view writes them, each once, and only those of the allowed hosts.
$urls = $products->pluck('image_url')->unique()->filter(function (string $url) use ($hosts): bool {
    $parts = parse_url($url);

    return is_array($parts)
        && strtolower($parts['scheme'] ?? '') === 'https'
        && in_array(strtolower($parts['host'] ?? ''), $hosts, true)
        && ! isset($parts['user']) && ! isset($parts['pass']) && ! isset($parts['port']);
})->values();

$responses = Http::pool(fn (Pool $pool) => $urls->map(
    fn (string $url) => $pool->as($url)->withoutRedirecting()->connectTimeout(3)->timeout(5)->get($url),
)->all());

$pdf = Fagus::view('catalog.show', ['products' => $products]);
foreach ($responses as $url => $response) {
    // A request that failed is an exception in the list, not a response.
    if ($response instanceof Response && $response->status() === 200) {
        $pdf = $pdf->resource($url, $response->body());
    }
}

return $pdf->download('catalog.pdf');
```

The same rules apply as for a resolver: the list of hosts, no redirects, a timeout. The key of
`resource()` must be the URL exactly as the document writes it (see
[above](#hand-over-data-with-resource)). An image that was not fetched is missing from the PDF
with a `resource.not-loaded` diagnostic, unless a resolver or a file answers it.

## Answer many requests at once with `resolveMany()`

Where the URLs are not known before the render, as with a stylesheet that names its images or a
document someone else wrote, `resolveMany()` does the same from inside it. Fagus knows most of a
document's resources before it uses them and asks for them together: the linked stylesheets,
then the first font file of each `@font-face` rule, then the images. The callback gets the URLs,
at most 64 in one call, fetches them concurrently and returns the data by URL:

```php
use Fagus\Laravel\Facades\Fagus;
use Illuminate\Http\Client\Pool;
use Illuminate\Http\Client\Response;
use Illuminate\Support\Facades\Http;

$hosts = ['cdn.example.com', 'media.example.com'];

$allowed = function (string $url) use ($hosts): bool {
    $parts = parse_url($url);

    return is_array($parts)
        && strtolower($parts['scheme'] ?? '') === 'https'
        && in_array(strtolower($parts['host'] ?? ''), $hosts, true)
        && ! isset($parts['user']) && ! isset($parts['pass']) && ! isset($parts['port']);
};

return Fagus::view('catalog.show', $data)
    ->resolveMany(function (array $urls) use ($allowed): array {
        $urls = array_values(array_filter($urls, $allowed));
        $responses = Http::pool(fn (Pool $pool) => array_map(
            fn (string $url) => $pool->as($url)->withoutRedirecting()->connectTimeout(3)->timeout(5)->get($url),
            $urls,
        ));
        $found = [];
        foreach ($responses as $url => $response) {
            // A request that failed is an exception in the list, not a response.
            if ($response instanceof Response && $response->status() === 200) {
                $found[$url] = $response->body();
            }
        }

        return $found;
    })
    ->download('catalog.pdf');
```

A URL the callback leaves out is not found: Fagus looks for a file inside the root next, so
relative URLs and files keep working beside the remote images. The rules of the
[allowlist](#remote-images-through-the-resolver-with-a-host-allowlist) apply here as there.

In plain PHP, `curl_multi` runs the transfers side by side:

```php
use Fagus\Fagus;

/**
 * The data at the https URLs of the hosts, by URL, fetched concurrently.
 *
 * @param  list<string>  $urls
 * @param  list<string>  $hosts
 * @return array<string, string>
 */
function fetch_all_allowed(array $urls, array $hosts, int $limit = 5 * 1024 * 1024): array
{
    $multi = curl_multi_init();
    $handles = [];
    $data = [];
    foreach ($urls as $url) {
        $parts = parse_url($url);
        if (! is_array($parts)
            || strtolower($parts['scheme'] ?? '') !== 'https'
            || ! in_array(strtolower($parts['host'] ?? ''), $hosts, true)
            || isset($parts['user']) || isset($parts['pass']) || isset($parts['port'])) {
            continue;
        }
        $data[$url] = '';
        $handle = curl_init($url);
        curl_setopt_array($handle, [
            CURLOPT_PROTOCOLS => CURLPROTO_HTTPS,
            CURLOPT_FOLLOWLOCATION => false,
            CURLOPT_CONNECTTIMEOUT => 3,
            CURLOPT_TIMEOUT => 5,
            // Returning another number than the length of the chunk stops the transfer.
            CURLOPT_WRITEFUNCTION => function ($handle, string $chunk) use (&$data, $url, $limit): int {
                $data[$url] .= $chunk;

                return strlen($data[$url]) > $limit ? 0 : strlen($chunk);
            },
        ]);
        curl_multi_add_handle($multi, $handle);
        $handles[$url] = $handle;
    }
    do {
        $status = curl_multi_exec($multi, $running);
        if ($running > 0) {
            curl_multi_select($multi, 1.0);
        }
    } while ($running > 0 && $status === CURLM_OK);
    // The transfers that ended with an error: a timeout, one stopped for its size.
    $failed = [];
    while (($info = curl_multi_info_read($multi)) !== false) {
        if ($info['result'] !== CURLE_OK) {
            $failed[spl_object_id($info['handle'])] = true;
        }
    }
    $found = [];
    foreach ($handles as $url => $handle) {
        if (! isset($failed[spl_object_id($handle)]) && curl_getinfo($handle, CURLINFO_RESPONSE_CODE) === 200) {
            $found[$url] = $data[$url];
        }
        curl_multi_remove_handle($multi, $handle);
    }

    return $found;
}

$hosts = ['cdn.example.com', 'media.example.com'];

$pdf = Fagus::html($html)
    ->resolveMany(fn (array $urls): array => fetch_all_allowed($urls, $hosts))
    ->render();
```

What to know about the calls:

- Each URL comes once in a render, in the order a `resolver()` would get them. The PDF does not
  depend on which of the two callbacks answers.
- Resources that only show while others are read come one at a time, as a list of one URL: the
  stylesheets of `@import` rules, a later source of a font whose first could not be loaded, the
  sprites and images of inline SVG, and the documents of `<iframe>`s.
- The answers of one call are in PHP's memory together and travel to the engine in one message
  of at most 1 GiB. Sixty-four product photos are no concern; scans of 20 MB each are better
  handed over as files in the base directory.
- `resolver()` and `resolveMany()` replace each other; the one called last is asked.
- Fagus and the package still fetch nothing themselves: no HTTP client comes with them, and the
  rules stay yours ([ADR 9](adr/0009-resources-in-batches.md)).

## Keep what a resolver fetched with `cache()`

`cache()` puts a [PSR-16](https://www.php-fig.org/psr/psr-16/) cache in front of the resolver:
what `resolver()` or `resolveMany()` returned for a URL is kept, and the next document that names
the URL gets it from the cache, without the callback. Laravel's cache stores are PSR-16 caches:

```php
use Illuminate\Support\Facades\Cache;

Fagus::view('catalog.show', $data)
    ->resolveMany($fetch)
    ->cache(Cache::store('file'), ttl: 3600);
```

In plain PHP, any PSR-16 implementation works, such as `symfony/cache` with its `Psr16Cache`:

```php
use Symfony\Component\Cache\Adapter\FilesystemAdapter;
use Symfony\Component\Cache\Psr16Cache;

$cache = new Psr16Cache(new FilesystemAdapter('fagus', 0, __DIR__.'/storage/cache'));

Fagus::html($html)->resolver($fetch)->cache($cache, ttl: 86400);
```

- `$ttl` is how long an entry lives: seconds, a `DateInterval`, or `null` for the default of the
  cache.
- The keys are `$prefix` and the SHA-256 of the URL, `fagus.resource.` and 64 hexadecimal digits
  by default, since a PSR-16 key may not hold characters such as `:` and `/`. Give documents of
  different tenants different prefixes if the same URL means different data for them.
- Only what was found is kept. A URL the resolver did not have is asked for again with the next
  document, so that an image that was missing for a moment does not stay missing.
- With `resolveMany()`, the cache is read once for a batch (`getMultiple()`), the callback gets
  the URLs the cache does not have, and what it returns is written once (`setMultiple()`).
- Data handed over with `resource()` and files are not cached; they never reach the resolver.
- The package needs `psr/simple-cache` for it (`composer require psr/simple-cache`), which
  Laravel and Symfony's cache bring along.

A cache that holds images makes a PDF depend on what was fetched earlier. For PDFs that must be
the same every time, such as invoices, keep the files in the base directory instead.

## `data:` URLs

`data:` URLs work everywhere a URL does, base64 or percent-encoded:

```html
<img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA…" alt="">
<link rel="stylesheet" href="data:text/css,p%20%7B%20color%3A%20navy%20%7D">
```

In PHP:

```php
$src = 'data:image/png;base64,'.base64_encode($png);
```

A document of a frame that comes from a `data:` URL loads nothing else.

## Images

`<img>`, `<picture>` and `srcset`, CSS backgrounds, `mask`, `list-style-image`, `border-image`,
`content: url()`, and SVG clip paths, masks, filters and `<use href="sprite.svg#id">` in other
files load as resources.

| Format | Notes |
|---|---|
| PNG | Also with `gAMA`, converted to sRGB as Chrome converts them. |
| JPEG | CMYK JPEGs without a profile are converted to sRGB as in Chrome. |
| GIF | |
| WebP | |
| AVIF | 8, 10 and 12 bits, 4:2:0, 4:2:2, 4:4:4 and monochrome, with alpha, grids and the first frame of an animation, as in Chrome. |
| BMP | 1 to 32 bits per pixel, RLE, bottom-up and top-down, with alpha where a mask says so, as in Chrome. |
| TIFF | Not shown by Chrome, but by Safari: scans and receipts, uncompressed, LZW, Deflate, PackBits, CCITT Group 4 (fax) and JPEG; gray, RGB and CMYK, 1 to 16 bits. The first image of a file with several pages. |
| PDF | Not shown by Chrome, but by Safari and Prince: a page of the file (`#page=2` names one, the first without), drawn as vector graphics with its text and fonts, as large as its crop box. See [page templates](paged-media.md#use-a-pdf-as-page-template). |
| SVG | Drawn as vector graphics. An SVG image does not see the document's `@font-face` fonts; its own `@font-face` rules load fonts only from `data:` URLs, as in Chrome. |

JPEG, PNG, WebP and TIFF images stand as their EXIF or TIFF orientation says, AVIF images as their
`irot` and `imir` say. `<picture>` sources and `srcset` are chosen as Chrome chooses them; JPEG XL
sources are skipped, as Fagus cannot decode them, and TIFF sources are taken, as in Safari.

An image that cannot be used is left out, and a diagnostic says why:

| Code | Message |
|---|---|
| `resource.not-loaded` | image `…` could not be loaded |
| `image.unsupported` | image `…` is not a PNG, JPEG, GIF, WebP, BMP, TIFF, AVIF, SVG or PDF image |
| `image.unsupported` | image `…` is not a valid SVG image |
| `image.too-large` | image `…` has more than 200 megapixels |
| `image.broken` | image `…` could not be decoded and is left out |
| `image.broken` | image `…` is a PDF file that could not be read |
| `image.broken` | image `…` names page 3 of a PDF file of 2 pages |
| `image.unsupported` | image `…` is a PDF file that needs a password, which Fagus does not open |
| `image.unsupported` | image `…` is a PDF 2.0 file, newer than the PDF 1.7 Fagus writes |
| `image.too-large` | image `…` is a PDF file larger than 128 MiB, or its page holds more than 128 MiB of content |

## Stylesheets

Stylesheets come from `<style>` elements, `<link rel="stylesheet" href="…">`, `@import` and
`stylesheet()`:

```html
<link rel="stylesheet" href="css/invoice.css">
<style>
  @import url("css/print.css") print;
  @import "css/theme.css" layer(theme);
</style>
```

- A linked stylesheet loads as a resource. One that cannot be loaded is reported:
  `html:4: warning[resource.not-loaded]: stylesheet `css/missing.css` could not be loaded`.
- `@import` loads its stylesheet as a resource, with the media queries, `supports()` condition
  and `layer()` of its rule, up to eight levels deep. One that cannot be loaded is reported as
  "stylesheet `…` could not be imported".
- Relative URLs in a linked or imported stylesheet resolve against the stylesheet's URL.
- `stylesheet($css)` adds CSS from PHP after the document's stylesheets. Its relative URLs
  resolve against the base directory.

## Fonts

`@font-face` rules load their fonts as resources: TTF, OTF, WOFF and WOFF2, with variable fonts
and `unicode-range`.

```css
@font-face {
  font-family: "Corporate Sans";
  src: url("fonts/CorporateSans.woff2") format("woff2");
  font-weight: 100 900;
}
```

Fagus loads the first source that Chrome would load and passes over formats and technologies
Chrome does not load. A font that cannot be loaded is reported ("font `…` could not be loaded"),
one that is not a font Fagus can read too ("`…` is not a font Fagus can read",
`font.unsupported`). Fonts from directories, generic families and system fonts are described in
[fonts.md](fonts.md).

## Load web fonts through the resolver

A stylesheet of Google Fonts, Bunny Fonts or a similar service is a resource like any other:

```html
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Roboto:wght@400;700&display=swap">
<style>
  body { font-family: Roboto, sans-serif; }
</style>
```

Fagus does not fetch it, so without a resolver the text falls back to the next family and the
diagnostics report the stylesheet as `resource.not-loaded`. A resolver can fetch it:

1. Fagus asks the resolver for the stylesheet's URL, as written in the `<link>` or in an
   `@import url(…)`.
2. The stylesheet's `@font-face` rules name the font files, on Google Fonts with URLs of
   `https://fonts.gstatic.com/…`. Fagus asks the resolver for the file of each rule, whether the
   text uses it or not. Relative URLs in the stylesheet resolve against the stylesheet's URL
   first, as in browsers.

The resolver answers both kinds of URL, for the hosts you allow, with a timeout, and keeps what it
fetched: Fagus asks again for every document. [`cache()`](#keep-what-a-resolver-fetched-with-cache)
keeps it for you in a PSR-16 cache; the callbacks below keep it themselves.

```php
use Fagus\Fagus;

$hosts = ['fonts.googleapis.com', 'fonts.gstatic.com'];
$cache = __DIR__.'/storage/web-fonts';

$pdf = Fagus::html($html)
    ->resolver(function (string $url) use ($hosts, $cache): ?string {
        $host = strtolower((string) parse_url($url, PHP_URL_HOST));
        if (! str_starts_with($url, 'https://') || ! in_array($host, $hosts, true)) {
            return null;
        }
        $file = $cache.'/'.hash('sha256', $url);
        if (is_file($file)) {
            return file_get_contents($file);
        }
        $context = stream_context_create(['http' => ['timeout' => 5]]);
        $data = @file_get_contents($url, false, $context);
        if ($data === false) {
            return null;
        }
        if (! is_dir($cache)) {
            mkdir($cache, 0775, true);
        }
        file_put_contents($file, $data);

        return $data;
    })
    ->render();
```

In Laravel, with the HTTP client and the cache:

```php
use Fagus\Laravel\Facades\Fagus;
use Illuminate\Support\Facades\Cache;
use Illuminate\Support\Facades\Http;

Fagus::view('invoices.show', $data)->resolver(function (string $url): ?string {
    $hosts = ['fonts.bunny.net', 'fonts.googleapis.com', 'fonts.gstatic.com'];
    $host = strtolower((string) parse_url($url, PHP_URL_HOST));
    if (! str_starts_with($url, 'https://') || ! in_array($host, $hosts, true)) {
        return null;
    }

    return Cache::remember('web-font:'.hash('sha256', $url), now()->addWeek(), fn (): ?string => rescue(
        fn () => Http::timeout(5)->get($url)->throw()->body(),
        report: false,
    ));
});
```

Google Fonts chooses the format of the files by the `User-Agent` of the request: WOFF2 in pieces by
`unicode-range` for a browser, other formats such as TrueType for a client it does not know, such
as PHP without a `User-Agent`. Fagus reads either.

Fonts from a service can change from one request to the next, and every render that misses the
cache waits for the network. For PDFs that must look the same every time, such as invoices,
download the font files once and load them with `@font-face` from the base directory, or put them
into a font directory ([fonts.md](fonts.md#load-fonts-with-font-face)). Then no network is needed
when the PDF is rendered.

## Frames: `<iframe>`, `<object>` and `<embed>`

An `<iframe>` (its `srcdoc`, else its `src`), and an `<object>` or `<embed>` whose resource is an
HTML document, shows that document as Chrome prints it: laid out at the frame's size, not
paginated, cut off at the frame's edges.

```html
<iframe src="frames/terms.html" style="width: 100%; height: 120mm; border: 0"></iframe>
<iframe srcdoc="<p>Inline document</p>"></iframe>
```

A frame's document loads like any other resource: a `data:` URL, the resources handed over, the
resolver, or a file inside the root. Its own relative URLs resolve against its URL. Scripts in it
do not run. Frames nest at most 8 deep, and one render lays out at most 256 documents in frames
with 8 MiB of markup together; past that limit, frames show nothing and a `frame.limit` warning
says so. The diagnostics of documents in frames are not reported.

Text files and images in frames show as Chrome shows them.

## What is never loaded

- **The network.** URLs with `http:`, `https:`, `ftp:` or a host (`//cdn.example.com/…`) are
  never fetched by Fagus. Only a resolver of yours can answer them.
- **Files outside the root**, through `..`, absolute paths or symbolic links. A URL with a scheme
  (`file:///etc/passwd`) is never read as a file.
- **Files at all**, when no base directory is set.
- **Scripts.** `<script>` does not run, and neither do event handlers or `javascript:` URLs.
  Content that a script would create in a browser, such as a chart drawn by JavaScript, is
  missing. Render it on the server as SVG or an image and hand it over.

Links (`<a href="https://…">`) are different: they become links in the PDF and load nothing.

## Diagnostics for missing resources

Every resource that is missing shows in `$pdf->diagnostics()`:

```php
$pdf = Fagus::html($html)->baseDirectory(__DIR__.'/assets')->render();

foreach ($pdf->diagnostics() as $diagnostic) {
    echo $diagnostic, "\n";
}
```

```text
html:4: warning[resource.not-loaded]: stylesheet `css/missing.css` could not be loaded
resources: warning[resource.not-loaded]: image `https://example.com/a.png` could not be loaded
resources: warning[resource.not-loaded]: image `../outside/x.png` could not be loaded
```

With `strict()`, any diagnostic makes `render()` throw a `RenderException` instead, which helps
to catch missing files in tests. Long `data:` URLs are cut short in the messages.

## Laravel: assets and Vite

`asset()`, `url()`, `Vite::asset()` and `@vite` write absolute URLs with the host of the
application (`https://example.com/build/assets/app-4ed993c7.css`), which Fagus does not fetch.
Use one of these instead:

- Root-relative URLs in the view, which resolve against `public/`:

  ```blade
  <img src="/images/logo.png" alt="ACME">
  ```

- The built CSS, handed over as a stylesheet:

  ```php
  use Illuminate\Support\Facades\Vite;

  Fagus::view('invoices.show', $data)->stylesheet(Vite::content('resources/css/pdf.css'));
  ```

- A resolver that maps the application's URLs to files in `public/`, and only to those:

  ```php
  Fagus::view('invoices.show', $data)->resolver(function (string $url): ?string {
      $prefix = rtrim(config('app.url'), '/').'/';
      if (! str_starts_with($url, $prefix)) {
          return null;
      }
      $public = realpath(public_path());
      $file = realpath(public_path(parse_url($url, PHP_URL_PATH) ?? ''));

      return $file !== false && str_starts_with($file, $public.DIRECTORY_SEPARATOR) && is_file($file)
          ? file_get_contents($file)
          : null;
  });
  ```
