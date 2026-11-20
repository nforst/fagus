<?php

declare(strict_types=1);

/*
 * Pins the binaries of a release in the PHP package, so that its installer accepts only them:
 *
 *     php tools/pin-binaries.php 0.1.0 SHA256SUMS
 *
 * SHA256SUMS is the file of the release (`gh release download v0.1.0 -p SHA256SUMS` fetches it
 * from a draft): one line "<sha256>  fagus-<target>" per binary. The script rewrites
 * php/fagus/src/Internal/Release.php with the version and the sums.
 *
 * Where the binaries lie next to SHA256SUMS (`gh release download v0.1.0` fetches them all), each
 * must have its sum, and `gh attestation verify` must find that the release workflow of this
 * repository built it; otherwise nothing is pinned. Without the binaries, only the sums are read.
 */

const REPOSITORY = 'nforst/fagus';

const TARGETS = [
    'aarch64-apple-darwin',
    'aarch64-unknown-linux-gnu',
    'aarch64-unknown-linux-musl',
    'x86_64-apple-darwin',
    'x86_64-pc-windows-msvc',
    'x86_64-unknown-freebsd',
    'x86_64-unknown-linux-gnu',
    'x86_64-unknown-linux-musl',
];

[, $version, $sums] = $argv + [null, null, null];
if (! is_string($version) || preg_match('{^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$}', $version) !== 1 || ! is_string($sums)) {
    fwrite(STDERR, "Usage: php tools/pin-binaries.php VERSION SHA256SUMS\n");
    exit(2);
}
$lines = @file($sums, FILE_IGNORE_NEW_LINES | FILE_SKIP_EMPTY_LINES);
if ($lines === false) {
    fwrite(STDERR, "{$sums} cannot be read.\n");
    exit(1);
}
$binaries = [];
foreach ($lines as $line) {
    if (preg_match('{^([0-9a-f]{64})\s+\*?fagus-([a-z0-9_]+(?:-[a-z0-9_]+)+?)(?:\.exe)?$}', trim($line), $match) === 1) {
        $binaries[$match[2]] = $match[1];
    }
}
ksort($binaries);
$missing = array_diff(TARGETS, array_keys($binaries));
if ($missing !== []) {
    fwrite(STDERR, 'SHA256SUMS lacks the binaries for '.implode(', ', $missing).".\n");
    exit(1);
}

$directory = dirname($sums);
$verified = 0;
foreach ($binaries as $target => $sum) {
    $file = $directory.'/fagus-'.$target.(str_contains($target, 'windows') ? '.exe' : '');
    if (! is_file($file)) {
        continue;
    }
    if (hash_file('sha256', $file) !== $sum) {
        fwrite(STDERR, "{$file} does not have the sum that SHA256SUMS lists.\n");
        exit(1);
    }
    // Built by the release workflow of the repository from one of its commits, as GitHub attests.
    $command = ['gh', 'attestation', 'verify', $file, '--repo', REPOSITORY, '--signer-workflow', REPOSITORY.'/.github/workflows/release.yml'];
    $process = proc_open($command, [1 => ['file', PHP_OS_FAMILY === 'Windows' ? 'NUL' : '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
    $errors = is_resource($process) ? (string) stream_get_contents($pipes[2]) : 'gh could not be started';
    if (! is_resource($process) || proc_close($process) !== 0) {
        fwrite(STDERR, "The attestation of {$file} does not hold: ".trim($errors)."\n");
        exit(1);
    }
    $verified++;
}
if ($verified > 0 && $verified < count($binaries)) {
    fwrite(STDERR, 'Only '.$verified.' of '.count($binaries)." binaries lie next to SHA256SUMS; fetch all or none.\n");
    exit(1);
}

$entries = '';
foreach ($binaries as $target => $sum) {
    $entries .= "        '{$target}' => '{$sum}',\n";
}
$path = dirname(__DIR__).'/php/fagus/src/Internal/Release.php';
$source = (string) file_get_contents($path);
$source = preg_replace("{public const string VERSION = '[^']*';}", "public const string VERSION = '{$version}';", $source, 1, $versions);
$source = preg_replace('{public const array BINARIES = \[.*?\];}s', "public const array BINARIES = [\n{$entries}    ];", (string) $source, 1, $lists);
if ($versions !== 1 || $lists !== 1) {
    fwrite(STDERR, "{$path} does not have the expected form.\n");
    exit(1);
}
file_put_contents($path, $source);
fwrite(STDOUT, 'Pinned '.count($binaries)." binaries of {$version}".($verified > 0 ? ', each checked against its sum and attestation' : ', without checking the binaries themselves').".\n");
