<?php

declare(strict_types=1);

namespace Fagus\Tests;

use Fagus\Document;
use Fagus\Engine;
use Fagus\Exception\EngineException;
use Fagus\Exception\InvalidArgumentException;
use Fagus\Internal\Release;
use Fagus\Signature;
use Fagus\Tests\Fixtures\FakeWorker;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;

/**
 * The engine against a stand-in for the worker ({@see FakeWorker}), for what the real binary
 * cannot be made to do: speak another protocol, be of another release, report its memory.
 */
final class EngineTest extends TestCase
{
    /** @var list<string> */
    private array $binaries = [];

    protected function setUp(): void
    {
        if (PHP_OS_FAMILY === 'Windows') {
            $this->markTestSkipped('The stand-in binary is a shell script.');
        }
    }

    protected function tearDown(): void
    {
        array_map(unlink(...), $this->binaries);
    }

    #[Test]
    public function it_refuses_a_binary_of_another_protocol(): void
    {
        $binary = $this->fake(protocol: 2, version: '9.0.0');

        try {
            (new Engine($binary))->render(Document::html('<p>x</p>'));
            $this->fail('A binary of protocol 2 was accepted.');
        } catch (EngineException $exception) {
            $this->assertSame(
                "The fagus binary `{$binary}` (version 9.0.0) speaks protocol 2; this package, nforst/fagus ".Release::VERSION.', needs protocol 1. '
                ."Install the binary of the package's release with `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`), "
                .'or point FAGUS_BINARY at the binary of version '.Release::VERSION.'.',
                $exception->getMessage(),
            );
        }
    }

    #[Test]
    public function it_warns_once_about_a_binary_of_another_release(): void
    {
        $binary = $this->fake(version: '0.0.1');
        $warnings = [];
        $warn = static function (string $message) use (&$warnings): void {
            $warnings[] = $message;
        };
        $engine = new Engine($binary, onWarning: $warn);

        $this->assertSame('%PDF-fake', substr(Document::html('<p>x</p>')->render($engine)->bytes(), 0, 9), 'it renders');
        $engine->close();
        Document::html('<p>x</p>')->render($engine);
        (new Engine($binary, onWarning: $warn))->version();

        $this->assertCount(1, $warnings, 'once per binary and version in a process');
        $this->assertStringStartsWith("The fagus binary `{$binary}` is version 0.0.1, this package nforst/fagus ".Release::VERSION.'. Both speak protocol 1', $warnings[0]);
        $this->assertStringContainsString('`php artisan fagus:install`', $warnings[0]);
    }

    #[Test]
    public function it_does_not_warn_about_the_binary_of_its_release(): void
    {
        $warnings = [];
        $engine = new Engine($this->fake(version: Release::VERSION), onWarning: static function (string $message) use (&$warnings): void {
            $warnings[] = $message;
        });

        $this->assertSame(Release::VERSION, $engine->version());
        $this->assertSame([], $warnings);
    }

    #[Test]
    public function it_writes_warnings_to_the_error_log_by_default(): void
    {
        $log = tempnam(sys_get_temp_dir(), 'fagus-log-');
        $previous = ini_set('error_log', (string) $log);
        try {
            (new Engine($this->fake(version: '0.0.2')))->version();
            $this->assertStringContainsString('is version 0.0.2, this package nforst/fagus', (string) file_get_contents((string) $log));
        } finally {
            ini_set('error_log', $previous === false ? '' : $previous);
            unlink((string) $log);
        }
    }

    #[Test]
    public function it_replaces_a_worker_after_as_many_documents_as_it_may_render(): void
    {
        $engine = new Engine($this->fake(), maxDocuments: 2);
        $engine->addFont('font');

        $documents = $this->render($engine, 5);

        $this->assertSame([1, 2, 1, 2, 1], array_column($documents, 1));
        $processes = array_column($documents, 0);
        $this->assertSame($processes[0], $processes[1]);
        $this->assertNotSame($processes[1], $processes[2]);
        $this->assertSame($processes[2], $processes[3]);
        $this->assertNotSame($processes[3], $processes[4]);
    }

    #[Test]
    public function it_replaces_a_worker_that_has_grown_beyond_its_memory(): void
    {
        $mb = 1024 * 1024;
        // Linux tells the size now: it grows with the second document and the third.
        $engine = new Engine($this->fake(memory: ['rss' => [10 * $mb, 20 * $mb, 21 * $mb], 'maxRss' => [50 * $mb]]), maxMemory: 20);
        $this->assertSame([1, 2, 3, 1, 2, 3], array_column($this->render($engine, 6), 1), 'replaced after the third, above 20 MB now');

        // macOS tells the peak only.
        $engine = new Engine($this->fake(memory: ['maxRss' => [10 * $mb, 30 * $mb]]), maxMemory: 20);
        $this->assertSame([1, 2, 1, 2], array_column($this->render($engine, 4), 1), 'replaced after the second, above 20 MB at its peak');

        // A worker that does not tell (Windows) keeps running.
        $engine = new Engine($this->fake(), maxMemory: 20);
        $this->assertSame([1, 2, 3], array_column($this->render($engine, 3), 1));
    }

    #[Test]
    public function it_refuses_the_pdf_of_a_binary_that_ignored_newer_options(): void
    {
        // The stand-in, like a binary older than these options, answers without `signed`, `encrypted` and `forms`.
        $engine = new Engine($this->fake());
        $signature = Signature::fromPkcs12(__DIR__.'/../../../tests/signing/rsa.p12', 'fagus-test-only');
        foreach ([
            [Document::html('<p>x</p>')->sign($signature), 'The fagus binary cannot sign PDFs and wrote the PDF without a signature. Install the binary of the same release.'],
            [Document::html('<p>x</p>')->encrypt('user'), 'The fagus binary cannot encrypt PDFs and wrote the PDF without a password. Install the binary of the same release.'],
            [Document::html('<input name=x>')->forms(), 'The fagus binary cannot make form fields and wrote the controls as they look. Install the binary of the same release.'],
        ] as [$document, $message]) {
            try {
                $document->render($engine);
                $this->fail('The PDF was handed out.');
            } catch (EngineException $exception) {
                $this->assertStringStartsWith($message, $exception->getMessage());
            }
        }
    }

    #[Test]
    public function it_refuses_limits_below_one(): void
    {
        $this->expectException(InvalidArgumentException::class);
        $this->expectExceptionMessage("The engine's maxDocuments must be at least 1 or null, not 0.");
        new Engine(maxDocuments: 0);
    }

    #[Test]
    public function it_refuses_negative_budgets(): void
    {
        $this->expectException(InvalidArgumentException::class);
        $this->expectExceptionMessage("The engine's timeLimit must be at least 0 or null, not -1.");
        new Engine(timeLimit: -1.0);
    }

    #[Test]
    public function it_passes_its_limits_with_each_document(): void
    {
        $limits = static fn (Engine $engine): array => FakeWorker::limits(Document::html('<p>x</p>')->render($engine)->bytes());

        // A time limit a tenth less than the timeout, the worker's memory budget.
        $this->assertEquals(['timeLimit' => 54000], $limits(new Engine($this->fake())));
        $this->assertEquals(['timeLimit' => 9000], $limits(new Engine($this->fake(), timeout: 10.0)));
        $this->assertEquals(
            ['timeLimit' => 2500, 'memoryBudget' => 512 * 1024 * 1024],
            $limits(new Engine($this->fake(), memoryBudget: 512, timeLimit: 2.5)),
        );
        $this->assertEquals(['timeLimit' => 0, 'memoryBudget' => 0], $limits(new Engine($this->fake(), memoryBudget: 0, timeLimit: 0.0)));
    }

    #[Test]
    public function it_answers_a_worker_that_asks_for_one_resource_after_the_other(): void
    {
        // A binary older than `askMany` ignores it and asks with `?`: a resolver for many gets
        // each URL on its own.
        $engine = new Engine($this->binaries[] = FakeWorker::create(version: Release::VERSION, ask: ['a.png', 'b.png', 'c.png']));
        $asked = [];
        $pdf = Document::html('<p>x</p>')->resolveMany(function (array $urls) use (&$asked): array {
            $asked[] = $urls;

            return array_intersect_key(['a.png' => 'A', 'c.png' => 'C'], array_flip($urls));
        })->render($engine);

        $this->assertSame([['a.png'], ['b.png'], ['c.png']], $asked);
        $this->assertSame(['a.png' => 'A', 'b.png' => null, 'c.png' => 'C'], FakeWorker::answers($pdf->bytes()));

        // Without a resolver the document does not allow questions.
        $this->assertSame([], FakeWorker::answers(Document::html('<p>x</p>')->render($engine)->bytes()));
    }

    /**
     * Renders `$count` documents and returns the process and the number of each in its process.
     *
     * @return list<array{0: int, 1: int}>
     */
    private function render(Engine $engine, int $count): array
    {
        $documents = [];
        for ($i = 0; $i < $count; $i++) {
            $documents[] = FakeWorker::parse(Document::html('<p>x</p>')->render($engine)->bytes());
        }

        return $documents;
    }

    /**
     * @param  array<string, list<int>>  $memory
     */
    private function fake(int $protocol = 1, string $version = Release::VERSION, array $memory = []): string
    {
        return $this->binaries[] = FakeWorker::create($protocol, $version, $memory);
    }
}
