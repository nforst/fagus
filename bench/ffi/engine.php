<?php

// php engine.php <worker|ffi> <warm|cold|memory> <document.html> <repeats>
//
// Renders a document from PHP through the PHP package and its worker, or through the C library
// (crates/fagus-ffi) with PHP's FFI and the request the package would send. run.py drives it.
//
// warm:   one engine, one render to warm it, then <repeats> renders, timed one by one.
// cold:   <repeats> times a new engine and its first render, timed together, as a PHP-FPM
//         request that renders pays it; the engine is closed after the timer stops.
// memory: one engine, 21 renders, then "@@done" and a wait for a line on standard input, so that
//         run.py can read the memory of the process tree.
//
// FAGUS and LIBRARY name the binary and the library (target/release by default).

declare(strict_types=1);

[, $path, $mode, $file, $repeats] = $argv;
$repeats = (int) $repeats;
$repo = dirname(__DIR__, 2);
$binary = getenv('FAGUS') ?: $repo.'/target/release/fagus';
$library = getenv('LIBRARY') ?: $repo.'/target/release/'.(PHP_OS_FAMILY === 'Darwin' ? 'libfagus_ffi.dylib' : 'libfagus_ffi.so');
$html = (string) file_get_contents($file);
$directory = dirname((string) realpath($file));

require $repo.'/vendor/autoload.php';

if ($path === 'worker') {
    $new = fn () => new Fagus\Engine($binary);
    $render = fn (Fagus\Engine $engine) => Fagus\Fagus::html($html)->baseDirectory($directory)->render($engine)->bytes();
    $close = function (Fagus\Engine $engine): void {
        $engine->close();
    };
} else {
    $header = (string) file_get_contents($repo.'/crates/fagus-ffi/include/fagus.h');
    $header = (string) preg_replace(['~/\*.*?\*/~s', '~^\s*#.*$~m', '~^extern "C" \{$|^\}$~m'], '', $header);
    $ffi = FFI::cdef($header, $library);
    $new = fn () => $ffi->fagus_engine_new(null, 0, null);
    // The request the PHP package sends for the same document (Document::request() and
    // Engine::limits()), encoded the same way.
    $render = function (FFI\CData $engine) use ($ffi, $html, $directory): string {
        $request = json_encode(['id' => 1, 'html' => 0, 'ask' => false, 'base' => $directory, 'timeLimit' => 54000.0, 'blobs' => [strlen($html)]], JSON_THROW_ON_ERROR);
        $result = $ffi->fagus_render($engine, $request, strlen($request), $html, strlen($html));
        try {
            $reply = json_decode($ffi->fagus_result_header($result, null), true, flags: JSON_THROW_ON_ERROR);
            if ($ffi->fagus_result_ok($result) !== 1) {
                throw new RuntimeException($reply['message']);
            }
            $length = $ffi->new('size_t');
            $pdf = $ffi->fagus_result_pdf($result, FFI::addr($length));

            return FFI::string($pdf, $length->cdata);
        } finally {
            $ffi->fagus_result_free($result);
        }
    };
    $close = function (FFI\CData $engine) use ($ffi): void {
        $ffi->fagus_engine_free($engine);
    };
}

$times = [];
$bytes = 0;
if ($mode === 'cold') {
    for ($i = 0; $i < $repeats; $i++) {
        $start = hrtime(true);
        $engine = $new();
        $bytes = strlen($render($engine));
        $times[] = (hrtime(true) - $start) / 1e6;
        $close($engine);
        unset($engine);
    }
} else {
    $engine = $new();
    $bytes = strlen($render($engine));
    $count = $mode === 'memory' ? 20 : $repeats;
    for ($i = 0; $i < $count; $i++) {
        $start = hrtime(true);
        $bytes = strlen($render($engine));
        $times[] = (hrtime(true) - $start) / 1e6;
    }
    if ($mode === 'memory') {
        echo "@@done\n";
        fflush(STDOUT);
        fgets(STDIN);
    }
}
sort($times);
$n = count($times);
echo json_encode([
    'path' => $path,
    'mode' => $mode,
    'median' => round($times[intdiv($n, 2)], 3),
    'n' => $n,
    'bytes' => $bytes,
]), "\n";
