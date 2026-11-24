<?php

declare(strict_types=1);

namespace Fagus\Tests;

use Fagus\Installer;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;

/**
 * Runs `composer install` in a project that requires a copy of this package, with a directory
 * as the source of the binaries: empty, so that the plugin cannot install the binary, unless a
 * test puts one there.
 */
final class ComposerPluginTest extends TestCase
{
    private string $directory;

    private string $composer;

    protected function setUp(): void
    {
        $composer = self::composer();
        if ($composer === null) {
            $this->markTestSkipped('Composer is not on the PATH.');
        }
        $this->composer = $composer;
        $this->directory = sys_get_temp_dir().DIRECTORY_SEPARATOR.'fagus-plugin-'.bin2hex(random_bytes(6));
        $root = dirname(__DIR__, 3);
        // The package without a binary that the plugin would keep.
        foreach (['php/fagus/src', 'php/laravel/src'] as $directory) {
            self::copy($root.'/'.$directory, $this->directory.'/package/'.$directory);
        }
        self::copy($root.'/php/fagus/bin/fagus-install', $this->directory.'/package/php/fagus/bin/fagus-install');
        $package = json_decode((string) file_get_contents($root.'/composer.json'), true);
        $this->assertIsArray($package);
        unset($package['require-dev'], $package['autoload-dev']);
        $package['version'] = '1.0.0';
        $this->write('package/composer.json', $package);
        mkdir($this->directory.'/release');
    }

    protected function tearDown(): void
    {
        if (! isset($this->directory)) {
            return;
        }
        $remove = static function (string $path) use (&$remove): void {
            if (is_dir($path) && ! is_link($path)) {
                foreach (scandir($path) ?: [] as $entry) {
                    if ($entry !== '.' && $entry !== '..') {
                        $remove($path.'/'.$entry);
                    }
                }
                rmdir($path);
            } elseif (file_exists($path) || is_link($path)) {
                unlink($path);
            }
        };
        $remove($this->directory);
    }

    #[Test]
    public function it_warns_and_goes_on_by_default(): void
    {
        [$status, $output] = $this->install();

        $this->assertSame(0, $status, $output);
        $this->assertStringContainsString('fagus: ', $output);
        $this->assertStringNotContainsString('Composer fails because', $output);
    }

    #[Test]
    public function it_fails_with_fagus_install_strict(): void
    {
        [$status, $output] = $this->install(environment: ['FAGUS_INSTALL_STRICT' => '1']);

        $this->assertNotSame(0, $status, $output);
        $this->assertStringContainsString('Composer fails because FAGUS_INSTALL_STRICT is set.', $output);
        // With the reason the binary was not installed.
        $this->assertMatchesRegularExpression('/fagus: (The fagus binary could not be downloaded|There is no fagus binary for)/', $output);
    }

    #[Test]
    public function it_fails_with_the_setting_in_composer_json_unless_the_environment_turns_it_off(): void
    {
        [$status, $output] = $this->install(extra: ['install-strict' => true]);

        $this->assertNotSame(0, $status, $output);
        $this->assertStringContainsString('Composer fails because extra.fagus.install-strict is set', $output);

        [$status, $output] = $this->install(extra: ['install-strict' => true], environment: ['FAGUS_INSTALL_STRICT' => '0']);

        $this->assertSame(0, $status, $output);
        $this->assertStringNotContainsString('Composer fails because', $output);
    }

    #[Test]
    public function it_fails_for_a_version_without_binaries_only_if_strict_and_names_the_way_out(): void
    {
        $this->pin([]);

        [$status, $output] = $this->install(environment: ['FAGUS_INSTALL_STRICT' => '1']);

        $this->assertNotSame(0, $status, $output);
        $this->assertStringContainsString('is a development version without built binaries', $output);
        $this->assertStringContainsString('Composer fails because FAGUS_INSTALL_STRICT is set. Set FAGUS_INSTALL_STRICT=0 where FAGUS_BINARY names a binary of your own.', $output);
        $this->assertStringNotContainsString('fagus-install', $output);

        [$status, $output] = $this->install();

        $this->assertSame(0, $status, $output);
        $this->assertStringNotContainsString('development version', $output);
    }

    #[Test]
    public function it_installs_the_pinned_binary_of_a_release_without_a_warning(): void
    {
        $this->release();

        [$status, $output, $project] = $this->install(environment: ['FAGUS_INSTALL_STRICT' => '1']);

        $this->assertSame(0, $status, $output);
        $this->assertStringNotContainsString('fagus: ', $output);
        $this->assertFileExists($this->binary($project));
    }

    #[Test]
    public function it_warns_that_a_development_version_gets_the_binary_of_the_last_release(): void
    {
        $this->release();
        $warning = '/fagus: nforst\/fagus is installed as a development version \(dev-main\)\. The binary is the one of release \d+\.\d+\.\d+ and may not know what the branch added\./';

        // Also if strict: the binary is there and checked.
        foreach ([[], ['FAGUS_INSTALL_STRICT' => '1']] as $environment) {
            [$status, $output, $project] = $this->install(environment: $environment, version: 'dev-main');

            $this->assertSame(0, $status, $output);
            $this->assertMatchesRegularExpression($warning, $output);
            $this->assertStringNotContainsString('Composer fails because', $output);
            $this->assertFileExists($this->binary($project));
        }

        // An alias does not hide the branch.
        [$status, $output] = $this->install(version: 'dev-main', constraint: 'dev-main as 1.0.0');

        $this->assertSame(0, $status, $output);
        $this->assertMatchesRegularExpression($warning, $output);
    }

    /**
     * Runs `composer install` in a new project and returns its exit status, its output with
     * white space collapsed, and the project's directory.
     *
     * @param  array<string, mixed>  $extra  `extra.fagus` of the project
     * @param  array<string, string>  $environment
     * @param  string  $version  the version of the package, which a path repository takes from its composer.json
     * @param  string|null  $constraint  what the project requires, by default that version
     * @return array{int, string, string}
     */
    private function install(array $extra = [], array $environment = [], string $version = '1.0.0', ?string $constraint = null): array
    {
        $package = json_decode((string) file_get_contents($this->directory.'/package/composer.json'), true);
        $this->assertIsArray($package);
        $this->write('package/composer.json', [...$package, 'version' => $version]);
        $project = 'project-'.bin2hex(random_bytes(4));
        $this->write($project.'/composer.json', [
            'name' => 'example/app',
            'require' => ['nforst/fagus' => $constraint ?? $version],
            'repositories' => [
                ['type' => 'path', 'url' => $this->directory.'/package', 'options' => ['symlink' => false]],
                ['packagist.org' => false],
            ],
            'config' => ['allow-plugins' => ['nforst/fagus' => true]],
            ...($extra === [] ? [] : ['extra' => ['fagus' => $extra]]),
        ]);
        $variables = getenv();
        unset($variables['FAGUS_INSTALL_STRICT'], $variables['FAGUS_GITHUB_TOKEN'], $variables['GITHUB_TOKEN'], $variables['COMPOSER_AUTH']);
        $variables = [
            ...$variables,
            'COMPOSER_HOME' => $this->directory.'/composer-home',
            'COMPOSER_CACHE_DIR' => $this->directory.'/composer-cache',
            'COMPOSER_NO_INTERACTION' => '1',
            'FAGUS_DOWNLOAD_URL' => $this->directory.'/release',
            // Long lines, so that Composer does not wrap its messages.
            'COLUMNS' => '1000',
            ...$environment,
        ];
        $process = proc_open(
            [PHP_BINARY, $this->composer, 'install', '--no-ansi', '--no-progress'],
            [0 => ['pipe', 'r'], 1 => ['pipe', 'w'], 2 => ['pipe', 'w']],
            $pipes,
            $this->directory.'/'.$project,
            $variables,
        );
        $this->assertIsResource($process);
        fclose($pipes[0]);
        // Composer writes little; one pipe cannot fill while the other is read.
        $output = stream_get_contents($pipes[1]).stream_get_contents($pipes[2]);
        fclose($pipes[1]);
        fclose($pipes[2]);

        return [proc_close($process), (string) preg_replace('/\s+/', ' ', (string) $output), $this->directory.'/'.$project];
    }

    /**
     * Writes the checksums of the binaries into the copy of the package, as
     * `tools/pin-binaries.php` does for a release.
     *
     * @param  array<string, string>  $binaries
     */
    private function pin(array $binaries): void
    {
        $path = $this->directory.'/package/php/fagus/src/Internal/Release.php';
        $source = preg_replace(
            '/(public const array BINARIES = )\[.*?\];/s',
            '$1'.var_export($binaries, true).';',
            (string) file_get_contents($path),
            count: $count,
        );
        $this->assertSame(1, $count);
        file_put_contents($path, $source);
    }

    /** Puts a binary for this platform into the release and pins it. */
    private function release(): void
    {
        $target = Installer::target();
        $bytes = "#!/bin/sh\necho fagus\n";
        file_put_contents($this->directory.'/release/'.Installer::asset($target), $bytes);
        $this->pin([$target => hash('sha256', $bytes)]);
    }

    /** Where the plugin installs the binary in a project. */
    private function binary(string $project): string
    {
        return $project.'/vendor/nforst/fagus/php/fagus/bin/'.(PHP_OS_FAMILY === 'Windows' ? 'fagus.exe' : 'fagus');
    }

    /** @param  array<string, mixed>  $json */
    private function write(string $path, array $json): void
    {
        $path = $this->directory.'/'.$path;
        if (! is_dir(dirname($path))) {
            mkdir(dirname($path), 0755, true);
        }
        file_put_contents($path, json_encode($json, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES));
    }

    private static function copy(string $from, string $to): void
    {
        if (is_dir($from)) {
            foreach (scandir($from) ?: [] as $entry) {
                if ($entry !== '.' && $entry !== '..') {
                    self::copy($from.'/'.$entry, $to.'/'.$entry);
                }
            }

            return;
        }
        if (! is_dir(dirname($to))) {
            mkdir(dirname($to), 0755, true);
        }
        copy($from, $to);
    }

    /** The Composer that runs the tests, else the first on the PATH (as a PHP script). */
    private static function composer(): ?string
    {
        $binary = getenv('COMPOSER_BINARY');
        if (is_string($binary) && is_file($binary)) {
            return $binary;
        }
        foreach (explode(PATH_SEPARATOR, (string) getenv('PATH')) as $directory) {
            foreach (['composer.phar', 'composer'] as $name) {
                $path = $directory.DIRECTORY_SEPARATOR.$name;
                if (is_file($path) && str_contains((string) file_get_contents($path, length: 200), '<?php')) {
                    return $path;
                }
            }
        }

        return null;
    }
}
