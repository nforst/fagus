<?php

// Tests the experimental C library (crates/fagus-ffi, docs/ffi.md) from PHP through FFI:
//
//   cargo build -p fagus-ffi
//   php tools/ffi.php [path of the library]
//
// The library is target/debug/libfagus_ffi.so (.dylib on macOS, fagus_ffi.dll on Windows) unless
// a path is given. Where PHP has no FFI, or ffi.enable forbids it, the script says so and ends
// with 0, as the PHP package itself does not use the library; with FAGUS_REQUIRE_FFI=1 (CI), it
// fails instead.

declare(strict_types=1);

function skip(string $reason): never
{
    echo "skipped: $reason\n";
    exit(getenv('FAGUS_REQUIRE_FFI') === '1' ? 1 : 0);
}

if (! extension_loaded('ffi')) {
    skip('PHP has no FFI extension');
}
$enabled = strtolower((string) ini_get('ffi.enable'));
if (! in_array($enabled, ['1', 'on', 'true', 'yes'], true) && ! ($enabled === 'preload' && PHP_SAPI === 'cli')) {
    skip("ffi.enable is '$enabled'");
}

$repository = dirname(__DIR__);
$library = $argv[1] ?? $repository.'/target/debug/'.match (PHP_OS_FAMILY) {
    'Darwin' => 'libfagus_ffi.dylib',
    'Windows' => 'fagus_ffi.dll',
    default => 'libfagus_ffi.so',
};

// The declarations of the header, without comments and what PHP's C parser does not read
// (the preprocessor and C++'s extern "C").
$header = (string) file_get_contents($repository.'/crates/fagus-ffi/include/fagus.h');
$header = (string) preg_replace(['~/\*.*?\*/~s', '~^\s*#.*$~m', '~^extern "C" \{$|^\}$~m'], '', $header);
$ffi = FFI::cdef($header, $library);

$failures = 0;
function check(bool $passed, string $what): void
{
    global $failures;
    echo ($passed ? 'ok' : 'FAILED').": $what\n";
    $failures += $passed ? 0 : 1;
}

/**
 * Renders a request of the worker protocol: [whether it is a PDF, its header, the PDF].
 *
 * @param  array<string, mixed>  $header
 * @param  list<string>  $blobs
 * @return array{bool, array<string, mixed>, string}
 */
function render(FFI $ffi, FFI\CData $engine, array $header, array $blobs): array
{
    $header['blobs'] = array_map(strlen(...), $blobs);
    $request = json_encode($header, JSON_THROW_ON_ERROR);
    $data = implode('', $blobs);
    $result = $ffi->fagus_render($engine, $request, strlen($request), $data, strlen($data));
    try {
        // A const char * comes back as a PHP string (up to its NUL); the PDF, which has NULs,
        // as a pointer to copy with its length.
        $answer = json_decode($ffi->fagus_result_header($result, null), true, flags: JSON_THROW_ON_ERROR);
        $length = $ffi->new('size_t');
        $pdf = $ffi->fagus_result_pdf($result, FFI::addr($length));
        $bytes = $pdf === null ? '' : FFI::string($pdf, $length->cdata);

        return [$ffi->fagus_result_ok($result) === 1, $answer, $bytes];
    } finally {
        $ffi->fagus_result_free($result);
    }
}

check($ffi->fagus_abi() === 0, 'the interface is experimental (fagus_abi() is 0)');
check($ffi->fagus_protocol() === 1, 'it takes requests of protocol 1');
check(preg_match('/^\d+\.\d+\.\d+/', $ffi->fagus_version()) === 1, 'it has a version ('.$ffi->fagus_version().')');

$error = $ffi->new('char *');
$options = json_encode(['fontDirs' => [$repository.'/tests/fonts'], 'locale' => 'de-DE'], JSON_THROW_ON_ERROR);
$engine = $ffi->fagus_engine_new($options, strlen($options), FFI::addr($error));
check($engine !== null, 'an engine with options');

[$ok, $header, $pdf] = render($ffi, $engine, [
    'id' => 3,
    'html' => 0,
    'base' => $repository.'/tests/fonts',
    'stylesheets' => [['name' => 'app.css', 'blob' => 1]],
    'created' => '2026-11-02T12:00:00Z',
], [
    '<style>@font-face { font-family: Doc; src: url(Inter-Bold.ttf) }</style><p style="font-family: Doc">Rechnung</p>',
    'p { colr: red }',
]);
check($ok && str_starts_with($pdf, '%PDF-') && str_contains($pdf, '%%EOF'), 'a PDF, with its NUL bytes ('.strlen($pdf).' bytes)');
check($header['id'] === 3 && $header['pages'] === 1, 'the header of P');
check(($header['diagnostics'][0]['code'] ?? null) === 'css.unknown-property', 'the diagnostics');
[, , $again] = render($ffi, $engine, [
    'id' => 4,
    'html' => 0,
    'base' => $repository.'/tests/fonts',
    'stylesheets' => [['name' => 'app.css', 'blob' => 1]],
    'created' => '2026-11-02T12:00:00Z',
], [
    '<style>@font-face { font-family: Doc; src: url(Inter-Bold.ttf) }</style><p style="font-family: Doc">Rechnung</p>',
    'p { colr: red }',
]);
check($again === $pdf, 'the same bytes again');

[$ok, $header, $pdf] = render($ffi, $engine, ['id' => 5, 'html' => 0, 'ask' => true], ['<p>x</p>']);
check(! $ok && $pdf === '' && $header['message'] === '`ask` needs a caller that answers questions, and there is none', 'no questions to the caller');

$ffi->fagus_engine_free($engine);

$options = '{"fontDir": "fonts"}';
$engine = $ffi->fagus_engine_new($options, strlen($options), FFI::addr($error));
$message = $engine === null && ! FFI::isNull($error) ? FFI::string($error) : '';
if ($message !== '') {
    $ffi->fagus_string_free($error);
}
check(str_contains($message, 'unknown field `fontDir`'), 'options it does not know are refused');

exit($failures === 0 ? 0 : 1);
