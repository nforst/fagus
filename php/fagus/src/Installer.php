<?php

declare(strict_types=1);

namespace Fagus;

use Fagus\Exception\InstallException;
use Fagus\Internal\Release;

/**
 * Installs the fagus binary of this package's release for the platform PHP runs on.
 *
 * The binary comes from the GitHub release of the version, or from `source`: a directory or URL
 * with the files of the release, for servers without access to GitHub. A private repository needs
 * a token. Whatever its source, the binary is installed only if its SHA-256 is the one this
 * package lists for the release.
 */
final class Installer
{
    /**
     * @param  ?string  $token  a GitHub token for a private fork; FAGUS_GITHUB_TOKEN or GITHUB_TOKEN if null
     * @param  ?string  $source  a directory or URL with the files of the release; FAGUS_DOWNLOAD_URL if null
     * @param  ?string  $cacheDirectory  where downloaded binaries are kept for the next installation
     * @param  array<string, string>  $binaries  the SHA-256 of the binary for each target
     */
    public function __construct(
        private readonly ?string $token = null,
        private readonly ?string $source = null,
        private readonly ?string $cacheDirectory = null,
        private readonly string $version = Release::VERSION,
        private readonly array $binaries = Release::BINARIES,
        private readonly string $repository = Release::REPOSITORY,
    ) {}

    /**
     * Installs the binary into `$directory`, by default the `bin` directory of this package, where
     * the engine finds it, and returns its path. A binary that is already there is kept.
     */
    public function install(?string $directory = null): string
    {
        $target = self::target();
        $expected = $this->binaries[$target] ?? null;
        if ($expected === null) {
            throw $this->binaries === []
                ? InstallException::developmentVersion($this->version)
                : InstallException::unsupportedPlatform($target);
        }
        $directory = rtrim($directory ?? dirname(__DIR__).'/bin', '/\\');
        $path = $directory.DIRECTORY_SEPARATOR.(str_contains($target, 'windows') ? 'fagus.exe' : 'fagus');
        if (is_file($path) && hash_file('sha256', $path) === $expected) {
            return $path;
        }

        $asset = self::asset($target);
        $cached = $this->cacheDirectory === null
            ? null
            : rtrim($this->cacheDirectory, '/\\').DIRECTORY_SEPARATOR.$this->version.DIRECTORY_SEPARATOR.$asset;
        $bytes = $cached !== null && is_file($cached) && hash_file('sha256', $cached) === $expected
            ? (string) file_get_contents($cached)
            : $this->download($asset);
        $actual = hash('sha256', $bytes);
        if (! hash_equals($expected, $actual)) {
            throw InstallException::checksumMismatch($asset, $expected, $actual);
        }
        self::write($path, $bytes, 0755);
        if ($cached !== null && ! is_file($cached)) {
            try {
                self::write($cached, $bytes, 0644);
            } catch (InstallException) {
                // The cache only saves a download.
            }
        }

        return $path;
    }

    /**
     * The target of a platform, as Rust names it (`x86_64-unknown-linux-gnu`): by default the
     * platform PHP runs on. `$os` is a family of PHP_OS_FAMILY, or for the BSDs the name of the
     * system (`FreeBSD`). Linux with musl, as in Alpine, has binaries of its own.
     */
    public static function target(?string $os = null, ?string $machine = null, ?bool $musl = null): string
    {
        // PHP_OS_FAMILY is "BSD" for FreeBSD, OpenBSD, NetBSD and DragonFly alike.
        $os ??= PHP_OS_FAMILY === 'BSD' ? php_uname('s') : PHP_OS_FAMILY;
        $machine = strtolower($machine ?? php_uname('m'));
        $architecture = match ($machine) {
            'x86_64', 'amd64', 'x64' => 'x86_64',
            'aarch64', 'arm64', 'armv8', 'armv8l' => 'aarch64',
            default => throw InstallException::unsupportedPlatform("{$os} on {$machine}"),
        };

        return match ($os) {
            'Linux' => $architecture.'-unknown-linux-'.(($musl ?? self::usesMusl()) ? 'musl' : 'gnu'),
            'Darwin' => $architecture.'-apple-darwin',
            'Windows' => $architecture === 'x86_64'
                ? 'x86_64-pc-windows-msvc'
                : throw InstallException::unsupportedPlatform("Windows on {$machine}"),
            'FreeBSD' => $architecture === 'x86_64'
                ? 'x86_64-unknown-freebsd'
                : throw InstallException::unsupportedPlatform("FreeBSD on {$machine}"),
            default => throw InstallException::unsupportedPlatform("{$os} on {$machine}"),
        };
    }

    /** The file of a target's binary in a release. */
    public static function asset(string $target): string
    {
        return 'fagus-'.$target.(str_contains($target, 'windows') ? '.exe' : '');
    }

    private static function usesMusl(): bool
    {
        return (glob('/lib/ld-musl-*.so.1') ?: []) !== [] || is_file('/etc/alpine-release');
    }

    private function download(string $asset): string
    {
        $source = $this->source ?? self::environment('FAGUS_DOWNLOAD_URL');
        if ($source !== null) {
            $location = rtrim($source, '/\\').'/'.$asset;
            if (! str_contains($source, '://') || str_starts_with($source, 'file://')) {
                $bytes = @file_get_contents($location);

                return $bytes === false ? throw InstallException::downloadFailed($location, 'The file cannot be read.') : $bytes;
            }

            return $this->get($location);
        }
        $tag = 'v'.$this->version;
        $token = $this->token ?? self::environment('FAGUS_GITHUB_TOKEN') ?? self::environment('GITHUB_TOKEN');
        if ($token === null) {
            return $this->get("https://github.com/{$this->repository}/releases/download/{$tag}/{$asset}");
        }
        // A private repository's assets come through the API, which redirects to storage that
        // must not see the token.
        $authorization = ['Authorization: Bearer '.$token, 'X-GitHub-Api-Version: 2022-11-28'];
        $release = json_decode(
            $this->get("https://api.github.com/repos/{$this->repository}/releases/tags/{$tag}", [...$authorization, 'Accept: application/vnd.github+json']),
            true,
        );
        foreach (is_array($release) && is_array($release['assets'] ?? null) ? $release['assets'] : [] as $candidate) {
            if (is_array($candidate) && ($candidate['name'] ?? null) === $asset && is_string($candidate['url'] ?? null)) {
                return $this->get($candidate['url'], [...$authorization, 'Accept: application/octet-stream'], authorizeRedirect: false);
            }
        }

        throw InstallException::downloadFailed("the release {$tag} of {$this->repository}", "It has no file {$asset}.");
    }

    /**
     * The body of a GET request. `authorizeRedirect: false` follows a redirect without the
     * request's headers.
     *
     * @param  list<string>  $headers
     */
    private function get(string $url, array $headers = [], bool $authorizeRedirect = true): string
    {
        $context = stream_context_create(['http' => [
            'method' => 'GET',
            'header' => [...$headers, 'User-Agent: fagus-installer'],
            'follow_location' => $authorizeRedirect ? 1 : 0,
            'ignore_errors' => true,
            'timeout' => 120,
        ]]);
        $stream = @fopen($url, 'rb', false, $context);
        if ($stream === false) {
            throw InstallException::downloadFailed($url, (string) (error_get_last()['message'] ?? ''));
        }
        try {
            $response = stream_get_meta_data($stream)['wrapper_data'] ?? [];
            $body = (string) stream_get_contents($stream);
        } finally {
            fclose($stream);
        }
        $status = 0;
        $location = null;
        foreach (is_array($response) ? $response : [] as $line) {
            if (! is_string($line)) {
                continue;
            }
            if (preg_match('{^HTTP/\S+\s+(\d{3})}', $line, $match) === 1) {
                $status = (int) $match[1];
                $location = null;
            } elseif (stripos($line, 'Location:') === 0) {
                $location = trim(substr($line, 9));
            }
        }
        if (! $authorizeRedirect && $status >= 300 && $status < 400 && $location !== null) {
            if (str_starts_with($location, '/')) {
                $parts = parse_url($url);
                $port = isset($parts['port']) ? ':'.$parts['port'] : '';
                $location = ($parts['scheme'] ?? 'https').'://'.($parts['host'] ?? '').$port.$location;
            }
            if (preg_match('{^https?://}i', $location) !== 1) {
                throw InstallException::downloadFailed($url, "It redirects to {$location}.");
            }

            return $this->get($location);
        }
        if ($status !== 200) {
            throw InstallException::downloadFailed($url, $status === 0 ? '' : "The server answered {$status}.");
        }

        return $body;
    }

    /** Writes a file through a temporary one, so that it is never seen half written. */
    private static function write(string $path, string $bytes, int $mode): void
    {
        $directory = dirname($path);
        if (! is_dir($directory) && ! @mkdir($directory, 0755, true) && ! is_dir($directory)) {
            throw InstallException::notWritten($path);
        }
        $temporary = $path.'.'.bin2hex(random_bytes(4)).'.tmp';
        if (@file_put_contents($temporary, $bytes) !== strlen($bytes) || ! @chmod($temporary, $mode) || ! @rename($temporary, $path)) {
            @unlink($temporary);

            throw InstallException::notWritten($path);
        }
    }

    private static function environment(string $name): ?string
    {
        $value = getenv($name);

        return $value === false || $value === '' ? null : $value;
    }
}
