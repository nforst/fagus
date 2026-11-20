<?php

declare(strict_types=1);

namespace Fagus\Tests;

use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;

/**
 * Runs `composer install` in a project that requires a copy of this package, with an empty
 * directory as the source of the binaries, so that the plugin cannot install the binary.
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

    /**
     * Runs `composer install` in a new project and returns its exit status and output, with
     * white space collapsed.
     *
     * @param  array<string, mixed>  $extra  `extra.fagus` of the project
     * @param  array<string, string>  $environment
     * @return array{int, string}
     */
    private function install(array $extra = [], array $environment = []): array
    {
        $project = 'project-'.bin2hex(random_bytes(4));
        $this->write($project.'/composer.json', [
            'name' => 'example/app',
            'require' => ['nforst/fagus' => '1.0.0'],
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

        return [proc_close($process), (string) preg_replace('/\s+/', ' ', (string) $output)];
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
