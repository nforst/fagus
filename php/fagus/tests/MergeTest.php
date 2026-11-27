<?php

declare(strict_types=1);

namespace Fagus\Tests;

use DateTimeImmutable;
use Fagus\Binary;
use Fagus\Conformance;
use Fagus\Engine;
use Fagus\Exception\FagusException;
use Fagus\Exception\InvalidArgumentException;
use Fagus\Exception\RenderException;
use Fagus\Fagus;
use Fagus\Merge;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;

/**
 * Merging PDFs, against the fagus binary in FAGUS_BINARY, or the release build of this
 * repository.
 */
final class MergeTest extends TestCase
{
    private const string FONTS = __DIR__.'/../../../tests/fonts';

    private const string MEDIA = __DIR__.'/../../../tests/corpus/images/media';

    private static Engine $engine;

    public static function setUpBeforeClass(): void
    {
        $binary = getenv('FAGUS_BINARY') ?: __DIR__.'/../../../target/release/'.Binary::name();
        if (! is_executable($binary)) {
            self::markTestSkipped("No fagus binary at {$binary}; build it with `cargo build --release`.");
        }
        self::$engine = new Engine($binary, [self::FONTS]);
        Fagus::useEngine(self::$engine);
    }

    public static function tearDownAfterClass(): void
    {
        Fagus::useEngine(null);
    }

    #[Test]
    public function it_merges_pdfs_and_rendered_pdfs(): void
    {
        $invoice = Fagus::html('<p style="break-after: page">Rechnung</p><p>Seite 2</p>')->render();
        $terms = Fagus::html('<p>AGB</p>')->pdfA(Conformance::PdfA3b)->render()->bytes();

        $pdf = Fagus::merge([$invoice, $terms])->render();
        $this->assertStringStartsWith('%PDF-1.7', $pdf->bytes());
        $this->assertSame(3, $pdf->pages());
        $this->assertSame([], $pdf->diagnostics());
        $this->assertArrayHasKey('pdf', $pdf->timings());

        // Some pages, a file, and the same PDF more than once.
        $merged = Merge::of()
            ->add($invoice, pages: '2')
            ->add($terms)
            ->addFile(self::MEDIA.'/pdf/shapes.pdf', pages: 2)
            ->add($invoice, pages: '1-')
            ->render();
        $this->assertSame(5, $merged->pages());
    }

    #[Test]
    public function it_writes_the_same_bytes_without_a_date(): void
    {
        $pdfs = [Fagus::html('<p>Eins</p>')->render(), Fagus::html('<p>Zwei</p>')->render()];
        $merge = Fagus::merge($pdfs)->title('Sammeldruck')->author('Buchhaltung', 'Fagus')->language('de-DE');

        $first = $merge->render()->bytes();
        $this->assertSame($first, $merge->render()->bytes());
        $dated = $merge->created(new DateTimeImmutable('2026-11-27T12:00:00Z'))->render()->bytes();
        $this->assertNotSame($first, $dated);
        $this->assertSame($dated, $merge->created(new DateTimeImmutable('2026-11-27T13:00:00+01:00'))->render()->bytes());

        $path = sys_get_temp_dir().'/fagus-merge-'.bin2hex(random_bytes(6)).'.pdf';
        try {
            $this->assertSame(2, $merge->save($path)->pages());
            $this->assertSame($first, file_get_contents($path));
        } finally {
            @unlink($path);
        }
    }

    #[Test]
    public function it_says_which_pdf_it_cannot_merge_and_why(): void
    {
        $good = Fagus::html('<p>Eins</p>')->render();
        foreach ([
            [Fagus::merge([$good, 'kein PDF']), 'the PDF at index 1 cannot be read: it is not a PDF file (it has no `%PDF-` header)'],
            [Fagus::merge([$good])->addFile(self::MEDIA.'/pdf/restricted.pdf'), 'the PDF at index 1 is encrypted; only PDFs without encryption can be merged'],
            [Fagus::merge()->addFile(self::MEDIA.'/pdf/locked.pdf'), 'the PDF at index 0 is encrypted; only PDFs without encryption can be merged'],
            [Fagus::merge()->add($good, '2'), 'the PDF at index 0 has 1 page, not page 2'],
        ] as [$merge, $message]) {
            try {
                $merge->render();
                $this->fail("Merged: {$message}");
            } catch (RenderException $exception) {
                $this->assertSame($message, $exception->getMessage());
                $this->assertSame([], $exception->diagnostics());
            }
        }

        // The engine goes on.
        $this->assertSame(2, Fagus::merge([$good, $good])->render()->pages());
    }

    #[Test]
    public function it_refuses_what_it_cannot_send(): void
    {
        foreach ([
            ['0', 'Invalid pages `0`: pages count from 1.'],
            [0, 'Invalid pages `0`: pages count from 1.'],
            ['3-1', 'Invalid pages `3-1`: a range ends before it begins.'],
            ['1,,2', 'Invalid pages `1,,2`: expected page numbers and ranges like `1-3,5,8-`.'],
            ['-2', 'Invalid pages `-2`: expected page numbers and ranges like `1-3,5,8-`.'],
            ['letzte', 'Invalid pages `letzte`: expected page numbers and ranges like `1-3,5,8-`.'],
        ] as [$pages, $message]) {
            try {
                Fagus::merge()->add('%PDF-', $pages);
                $this->fail("Accepted pages {$pages}.");
            } catch (InvalidArgumentException $exception) {
                $this->assertSame($message, $exception->getMessage());
            }
        }
        $this->assertSame(1, Fagus::merge()->add('%PDF-', ' 1 - 3 , 5, 8- ')->count());

        $this->expectException(InvalidArgumentException::class);
        $this->expectExceptionMessage('There are no PDFs to merge; add them with add() or addFile().');
        Fagus::merge()->render();
    }

    #[Test]
    public function it_reads_files_only_through_add_file(): void
    {
        // A path given to add() is data, not a file: it is no PDF.
        try {
            Fagus::merge([self::MEDIA.'/pdf/shapes.pdf'])->render();
            $this->fail('A path was read as a file.');
        } catch (RenderException $exception) {
            $this->assertStringContainsString('it is not a PDF file', $exception->getMessage());
        }

        $this->expectException(FagusException::class);
        $this->expectExceptionMessage('The PDF file `'.self::MEDIA.'/fehlt.pdf` could not be read.');
        Fagus::merge()->addFile(self::MEDIA.'/fehlt.pdf');
    }

    #[Test]
    public function it_sends_each_pdf_once(): void
    {
        $merge = Merge::of(['A', 'B', 'A'])->add('B', '2-')->title('T')->created(new DateTimeImmutable('2026-11-27T13:00:00+01:00'));
        [$header, $blobs] = $merge->request(7);

        $this->assertSame(['A', 'B'], $blobs);
        $this->assertSame([
            'id' => 7,
            'inputs' => [['blob' => 0], ['blob' => 1], ['blob' => 0], ['blob' => 1, 'pages' => '2-']],
            'title' => 'T',
            'created' => '2026-11-27T12:00:00Z',
        ], $header);
    }
}
