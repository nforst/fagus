<?php

declare(strict_types=1);

namespace Fagus\Tests;

use Fagus\Binary;
use Fagus\Exception\InstallException;
use Fagus\Installer;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;

/**
 * Installs fake binaries from a directory that stands in for a release.
 */
final class InstallerTest extends TestCase
{
    private string $directory;

    protected function setUp(): void
    {
        // With a space, as in C:\Users\Jane Doe or "Application Support".
        $this->directory = sys_get_temp_dir().DIRECTORY_SEPARATOR.'fagus installer '.bin2hex(random_bytes(6));
        mkdir($this->directory.'/release', 0755, true);
    }

    protected function tearDown(): void
    {
        $remove = static function (string $path) use (&$remove): void {
            if (is_dir($path) && ! is_link($path)) {
                foreach (scandir($path) ?: [] as $entry) {
                    if ($entry !== '.' && $entry !== '..') {
                        $remove($path.'/'.$entry);
                    }
                }
                rmdir($path);
            } elseif (file_exists($path)) {
                unlink($path);
            }
        };
        $remove($this->directory);
    }

    #[Test]
    public function it_names_the_targets_of_platforms(): void
    {
        $this->assertSame('x86_64-unknown-linux-gnu', Installer::target('Linux', 'x86_64', false));
        $this->assertSame('aarch64-unknown-linux-musl', Installer::target('Linux', 'aarch64', true));
        $this->assertSame('aarch64-apple-darwin', Installer::target('Darwin', 'arm64'));
        $this->assertSame('x86_64-apple-darwin', Installer::target('Darwin', 'x86_64'));
        $this->assertSame('x86_64-pc-windows-msvc', Installer::target('Windows', 'AMD64'));
        // php_uname('m') says amd64 on FreeBSD.
        $this->assertSame('x86_64-unknown-freebsd', Installer::target('FreeBSD', 'amd64'));
        $this->assertSame('fagus-x86_64-pc-windows-msvc.exe', Installer::asset('x86_64-pc-windows-msvc'));
        $this->assertSame('fagus-aarch64-apple-darwin', Installer::asset('aarch64-apple-darwin'));
        $this->assertSame('fagus-x86_64-unknown-freebsd', Installer::asset('x86_64-unknown-freebsd'));

        $this->expectException(InstallException::class);
        Installer::target('Linux', 'riscv64', false);
    }

    #[Test]
    public function it_names_the_platforms_without_binaries(): void
    {
        $platforms = [
            'FreeBSD on arm64' => ['FreeBSD', 'arm64'],
            'OpenBSD on amd64' => ['OpenBSD', 'amd64'],
            'NetBSD on amd64' => ['NetBSD', 'amd64'],
            'BSD on amd64' => ['BSD', 'amd64'],
            'Windows on arm64' => ['Windows', 'ARM64'],
        ];
        foreach ($platforms as $name => [$os, $machine]) {
            try {
                Installer::target($os, $machine);
                $this->fail("a target for {$name}");
            } catch (InstallException $exception) {
                $this->assertStringStartsWith("There is no fagus binary for {$name}. ", $exception->getMessage());
            }
        }
    }

    #[Test]
    public function it_names_the_files_the_release_publishes(): void
    {
        $workflow = __DIR__.'/../../../.github/workflows/release.yml';
        if (! is_file($workflow)) {
            $this->markTestSkipped('Not in the repository.');
        }
        // The targets of the job `build`, which makes the binaries; other jobs (the C library) have their own.
        preg_match('/^  build:\n(.*?)(?=^  [a-z][a-z-]*:$)/ms', (string) file_get_contents($workflow), $job);
        preg_match_all('/\{ target: ([a-z0-9_-]+),/', $job[1] ?? '', $matches);
        $published = $matches[1];
        sort($published);
        $platforms = [
            ['Linux', 'x86_64', false], ['Linux', 'aarch64', false], ['Linux', 'x86_64', true], ['Linux', 'aarch64', true],
            ['Darwin', 'x86_64', null], ['Darwin', 'arm64', null], ['Windows', 'AMD64', null],
            ['FreeBSD', 'amd64', null],
        ];
        $targets = array_map(static fn (array $platform): string => Installer::target(...$platform), $platforms);
        sort($targets);

        // Every platform has a binary in the release, and every binary has a platform.
        $this->assertSame($published, $targets);
        // The release collects `fagus-<target>`, with `.exe` on Windows.
        $this->assertStringContainsString('cp "target/${{ matrix.target }}/release/fagus.exe" "dist/fagus-${{ matrix.target }}.exe"', (string) file_get_contents($workflow));
        $this->assertStringContainsString('cp "target/${{ matrix.target }}/release/fagus" "dist/fagus-${{ matrix.target }}"', (string) file_get_contents($workflow));
        // tools/pin-binaries.php pins them all.
        $pin = (string) file_get_contents(__DIR__.'/../../../tools/pin-binaries.php');
        foreach ($published as $target) {
            $this->assertStringContainsString("'{$target}',", $pin);
        }
    }

    #[Test]
    public function it_installs_the_binary_with_the_listed_checksum(): void
    {
        $binary = "#!/bin/sh\necho fagus\n";
        $installer = $this->installer($binary, hash('sha256', $binary));

        $path = $installer->install($this->directory.'/bin');

        $this->assertSame($this->directory.'/bin'.DIRECTORY_SEPARATOR.Binary::name(), $path);
        $this->assertSame($binary, file_get_contents($path));
        $this->assertTrue(is_executable($path));
        $this->assertSame([Binary::name()], array_values(array_diff(scandir($this->directory.'/bin') ?: [], ['.', '..'])), 'no temporary files');
        // Downloaded once, kept in the cache.
        $this->assertFileExists($this->directory.'/cache/9.9.9/'.Installer::asset(Installer::target()));
        // An installed binary is kept, even if the release is gone.
        unlink($this->directory.'/release/'.Installer::asset(Installer::target()));
        $this->assertSame($path, $installer->install($this->directory.'/bin'));
        // Another directory takes it from the cache.
        $this->assertSame($binary, file_get_contents($installer->install($this->directory.'/other')));
    }

    #[Test]
    public function it_refuses_a_binary_with_another_checksum(): void
    {
        $installer = $this->installer('not the binary', hash('sha256', 'the binary'));

        try {
            $installer->install($this->directory.'/bin');
            $this->fail('installed');
        } catch (InstallException $exception) {
            $this->assertStringContainsString('is not the binary of this release', $exception->getMessage());
        }
        $this->assertDirectoryDoesNotExist($this->directory.'/bin');
    }

    #[Test]
    public function it_says_when_there_are_no_binaries(): void
    {
        $this->expectExceptionMessage('a development version without built binaries');

        (new Installer(source: $this->directory.'/release', version: '0.0.1', binaries: []))->install($this->directory.'/bin');
    }

    #[Test]
    public function it_says_when_a_platform_has_no_binary(): void
    {
        $this->expectExceptionMessage('There is no fagus binary for '.Installer::target());

        (new Installer(source: $this->directory.'/release', binaries: ['sparc-sun-solaris' => str_repeat('0', 64)]))
            ->install($this->directory.'/bin');
    }

    private function installer(string $binary, string $checksum): Installer
    {
        $target = Installer::target();
        file_put_contents($this->directory.'/release/'.Installer::asset($target), $binary);

        return new Installer(
            source: 'file://'.$this->directory.'/release',
            cacheDirectory: $this->directory.'/cache',
            version: '9.9.9',
            binaries: [$target => $checksum],
        );
    }
}
