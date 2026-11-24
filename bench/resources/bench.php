<?php

declare(strict_types=1);

/*
 * A document with remote images, answered by a resolver that is as slow as a server far away.
 *
 *     php bench/resources/bench.php [images] [milliseconds] [runs]
 *
 * Fifty images and 20 ms each by default. `resolver()` is asked for one URL after the other and
 * waits for each; `resolveMany()` gets the URLs together and waits once for all of them, as a
 * callback does that fetches them concurrently (`Http::pool()`, `curl_multi`). The binary is
 * FAGUS_BINARY, or the release build of this repository; one older than 1.2.0 asks `resolveMany()`
 * for one URL after the other too, which shows the time before.
 */

require __DIR__.'/../../vendor/autoload.php';

use Fagus\Binary;
use Fagus\Document;
use Fagus\Engine;

$images = (int) ($argv[1] ?? 50);
$delay = (int) ($argv[2] ?? 20);
$runs = (int) ($argv[3] ?? 5);

$binary = getenv('FAGUS_BINARY') ?: __DIR__.'/../../target/release/'.Binary::name();
$engine = new Engine($binary, onWarning: static function (string $warning): void {});

$html = '<style>img { width: 20mm; height: 20mm }</style>';
for ($index = 0; $index < $images; $index++) {
    $html .= "<img src='https://cdn.example.com/products/{$index}.svg' alt=''>";
}
// A different image for each URL, so that none shares its data with another.
$image = static fn (string $url): string => '<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">'
    .'<rect width="20" height="20" fill="#'.substr(hash('sha256', $url), 0, 6).'"/></svg>';
// Waits by the clock: on a busy machine `usleep()` of 20 ms takes several times as long.
$wait = static function () use ($delay): void {
    $end = hrtime(true) + $delay * 1_000_000;
    while (hrtime(true) < $end) {
    }
};

$median = static function (callable $render) use ($runs): array {
    $render();   // the worker starts
    $times = [];
    for ($run = 0; $run < $runs; $run++) {
        $start = hrtime(true);
        $pdf = $render();
        $times[] = (hrtime(true) - $start) / 1e6;
    }
    sort($times);

    return [$times[intdiv(count($times), 2)], hash('sha256', $pdf->bytes())];
};

$calls = new stdClass;
$calls->count = 0;
[$handed, $expected] = $median(function () use ($engine, $html, $image, $images) {
    $document = Document::html($html)->created(new DateTimeImmutable('2026-11-02T12:00:00Z'));
    for ($index = 0; $index < $images; $index++) {
        $url = "https://cdn.example.com/products/{$index}.svg";
        $document = $document->resource($url, $image($url));
    }

    return $document->render($engine);
});
[$one, $single] = $median(fn () => Document::html($html)
    ->created(new DateTimeImmutable('2026-11-02T12:00:00Z'))
    ->resolver(function (string $url) use ($image, $wait): string {
        $wait();

        return $image($url);
    })
    ->render($engine));
[$many, $batched] = $median(fn () => Document::html($html)
    ->created(new DateTimeImmutable('2026-11-02T12:00:00Z'))
    ->resolveMany(function (array $urls) use ($image, $wait, $calls): array {
        $calls->count++;
        $wait();

        return array_combine($urls, array_map($image, $urls));
    })
    ->render($engine));

if ($single !== $expected || $batched !== $expected) {
    fwrite(STDERR, "The PDFs differ.\n");
    exit(1);
}
printf("%d images, %d ms each, median of %d renders\n", $images, $delay, $runs);
printf("resource() (nothing to fetch) : %8.1f ms\n", $handed);
printf("resolver()                    : %8.1f ms\n", $one);
printf("resolveMany()                 : %8.1f ms  (%d calls for each PDF)\n", $many, intdiv($calls->count, $runs + 1));
