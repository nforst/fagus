<?php

declare(strict_types=1);

namespace Fagus\Tests;

use DateTimeImmutable;
use DateTimeZone;
use Fagus\Binary;
use Fagus\Conformance;
use Fagus\Diagnostic;
use Fagus\Document;
use Fagus\Engine;
use Fagus\Exception\EngineException;
use Fagus\Exception\FagusException;
use Fagus\Exception\InvalidArgumentException;
use Fagus\Exception\RenderException;
use Fagus\Fagus;
use Fagus\InvoiceProfile;
use Fagus\Layout\Box;
use Fagus\Layout\Element;
use Fagus\Layout\Page;
use Fagus\Layout\Section;
use Fagus\Permission;
use Fagus\Relationship;
use Fagus\Severity;
use PHPUnit\Framework\Attributes\RunInSeparateProcess;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;
use RuntimeException;

/**
 * Runs against the fagus binary in FAGUS_BINARY, or the release build of this repository.
 */
final class FagusTest extends TestCase
{
    private const string FONTS = __DIR__.'/../../../tests/fonts';

    private static Engine $engine;

    private static string $binary;

    public static function setUpBeforeClass(): void
    {
        $binary = getenv('FAGUS_BINARY') ?: __DIR__.'/../../../target/release/'.Binary::name();
        if (! is_executable($binary)) {
            self::markTestSkipped("No fagus binary at {$binary}; build it with `cargo build --release`.");
        }
        self::$binary = $binary;
        self::$engine = new Engine($binary, [self::FONTS]);
        Fagus::useEngine(self::$engine);
    }

    public static function tearDownAfterClass(): void
    {
        Fagus::useEngine(null);
    }

    #[Test]
    public function it_renders_html_to_pdf(): void
    {
        $pdf = Fagus::html('<style>body { font-family: Inter }</style><h1>Rechnung</h1><p>Hallo</p>')->render();

        $this->assertStringStartsWith('%PDF-', $pdf->bytes());
        $this->assertSame(1, $pdf->pages());
        $this->assertSame([], $pdf->diagnostics());
        $this->assertArrayHasKey('layout', $pdf->timings());
    }

    #[Test]
    public function it_keeps_the_worker_running_between_documents(): void
    {
        $first = Fagus::html('<p>Eins</p>')->render();
        $second = Fagus::html('<p style="break-after: page">Zwei</p><p>Drei</p>')->render();

        $this->assertSame(1, $first->pages());
        $this->assertSame(2, $second->pages());
        $this->assertMatchesRegularExpression('/^\d+\.\d+\.\d+/', self::$engine->version());
    }

    #[Test]
    public function it_asks_the_resolver_for_resources(): void
    {
        $asked = [];
        $font = (string) file_get_contents(self::FONTS.'/Inter-Bold.ttf');
        $pdf = Fagus::html('<style>@font-face { font-family: Hausschrift; src: url(fonts/haus.ttf); } p { font-family: Hausschrift }</style><p>Hallo</p>')
            ->resolver(function (string $url) use (&$asked, $font): ?string {
                $asked[] = $url;

                return $url === 'fonts/haus.ttf' ? $font : null;
            })
            ->render();

        $this->assertSame(['fonts/haus.ttf'], $asked);
        $this->assertSame([], $pdf->diagnostics());
    }

    #[Test]
    public function it_uses_the_pages_of_a_pdf_as_page_templates(): void
    {
        $media = __DIR__.'/../../../tests/corpus/images/media';
        $html = '<style>@page { margin: 50mm 20mm 30mm; background: url(letterhead.pdf#page=2) 0 0 / 100% 100% no-repeat }'
            .' @page :first { background-image: url(letterhead.pdf) } p { break-after: page }</style><p>Eins</p><p>Zwei</p>';
        $letterhead = (string) file_get_contents($media.'/letterhead.pdf');

        // From a file inside the root, from the resolver and handed over: the same PDF.
        $created = new DateTimeImmutable('2026-11-19 12:00:00', new DateTimeZone('UTC'));
        $asked = [];
        $written = [];
        foreach ([
            Fagus::html($html)->baseDirectory($media),
            Fagus::html($html)->resolver(function (string $url) use (&$asked, $letterhead): ?string {
                $asked[] = $url;

                return $url === 'letterhead.pdf' ? $letterhead : null;
            }),
            Fagus::html($html)->resource('letterhead.pdf', $letterhead),
        ] as $document) {
            $pdf = $document->created($created)->render();

            $this->assertSame([], $pdf->diagnostics());
            $this->assertSame(2, $pdf->pages());
            $written[] = $pdf->bytes();
        }
        $this->assertSame($written[0], $written[1]);
        $this->assertSame($written[0], $written[2]);
        // Both pages of the letterhead, as form XObjects with the forms in them.
        $this->assertGreaterThanOrEqual(2, preg_match_all('#/Subtype\s*/Form#', $written[0]));
        // The file is loaded once for both pages; a resolver that does not know the URL with
        // its fragment is asked for the file without it.
        $this->assertContains('letterhead.pdf', $asked);
        $this->assertLessThanOrEqual(2, count($asked));

        $missing = Fagus::html('<img src="letterhead.pdf#page=3">')->resource('letterhead.pdf', $letterhead)->render();
        $this->assertSame('image.broken', $missing->diagnostics()[0]->code);
    }

    #[Test]
    public function it_uses_resources_that_are_handed_over(): void
    {
        $pixel = (string) base64_decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==');
        $pdf = Fagus::html('<img src="logo.png" width="20" height="20">')->resource('logo.png', $pixel)->render();

        $this->assertSame([], $pdf->diagnostics());
        $this->assertMatchesRegularExpression('#/Subtype\s*/Image#', $pdf->bytes());
    }

    #[Test]
    public function it_reports_diagnostics_and_fails_in_strict_mode(): void
    {
        $document = Fagus::html('<style>p { colr: red }</style><img src="fehlt.png"><p>x</p>');
        $diagnostics = $document->render()->diagnostics();

        $this->assertCount(2, $diagnostics);
        $this->assertSame(Severity::Warning, $diagnostics[0]->severity);
        $this->assertSame('css.unknown-property', $diagnostics[0]->code);

        try {
            $document->strict()->render();
            $this->fail('No exception in strict mode.');
        } catch (RenderException $exception) {
            $this->assertCount(2, $exception->diagnostics());
            $this->assertStringContainsString('css.unknown-property', $exception->getMessage());
        }
    }

    #[Test]
    public function it_writes_factur_x_invoices(): void
    {
        $xml = (string) file_get_contents(__DIR__.'/../../../tests/corpus/invoice/factur-x.xml');
        $pdf = Fagus::html('<p>Rechnung</p>')
            ->facturX($xml)
            ->attach('notiz.txt', 'Danke!', 'text/plain', Relationship::Supplement, 'Eine Notiz')
            ->created(new DateTimeImmutable('2026-11-02 13:00:00', new DateTimeZone('Europe/Berlin')))
            ->render();

        $bytes = $pdf->bytes();
        $this->assertStringContainsString('/F(factur-x.xml)', self::decoded($bytes));
        $this->assertStringContainsString('/F(notiz.txt)', self::decoded($bytes));
        $this->assertStringContainsString('<pdfaid:part>3</pdfaid:part>', $bytes);
        $this->assertStringContainsString('<fx:ConformanceLevel>EN 16931</fx:ConformanceLevel>', $bytes);
        $this->assertStringContainsString('2026-11-02T12:00:00', $bytes, 'the date in UTC');
    }

    #[Test]
    public function it_writes_the_version_and_description_of_invoices(): void
    {
        $xml = '<rsm:CrossIndustryInvoice><ram:GuidelineSpecifiedDocumentContextParameter>'
            .'<ram:ID>urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0</ram:ID>'
            .'</ram:GuidelineSpecifiedDocumentContextParameter></rsm:CrossIndustryInvoice>';
        $bytes = Fagus::html('<p>Rechnung</p>')->facturX($xml, version: '3.0', description: 'XRechnung Invoice')->render()->bytes();
        $this->assertStringContainsString('<fx:Version>3.0</fx:Version>', $bytes);
        $this->assertStringContainsString('<fx:ConformanceLevel>XRECHNUNG</fx:ConformanceLevel>', $bytes);
        $this->assertStringContainsString('/F(xrechnung.xml)', self::decoded($bytes));
        $this->assertStringContainsString('/Desc(XRechnung Invoice)', self::decoded($bytes));

        $bytes = Fagus::html('<p>Rechnung</p>')->facturX($xml)->render()->bytes();
        $this->assertStringContainsString('<fx:Version>1.0</fx:Version>', $bytes, 'the default');
        $this->assertStringContainsString('/Desc(Factur-X/ZUGFeRD invoice)', self::decoded($bytes));

        [$header] = Fagus::html('<p>Rechnung</p>')->facturX($xml, InvoiceProfile::XRechnung, '3.0', 'XRechnung Invoice')->request(1);
        $this->assertSame(['blob' => 1, 'profile' => 'xrechnung', 'version' => '3.0', 'description' => 'XRechnung Invoice'], $header['facturX']);
        [$header] = Fagus::html('<p>Rechnung</p>')->facturX($xml)->request(1);
        $this->assertSame(['blob' => 1], $header['facturX']);

        try {
            Fagus::html('<p>Rechnung</p>')->facturX($xml, version: ' ');
            $this->fail('No exception for an empty version.');
        } catch (InvalidArgumentException $exception) {
            $this->assertSame('The invoice version must not be empty.', $exception->getMessage());
        }

        $this->expectException(RenderException::class);
        $this->expectExceptionMessage('the invoice description contains control characters');
        Fagus::html('<p>Rechnung</p>')->facturX($xml, description: "XRechnung\nInvoice")->render();
    }

    #[Test]
    public function it_writes_pdf_a(): void
    {
        $pdf = Fagus::html('<p>Archiv</p>')->pdfA(Conformance::PdfA2u)->render();
        $this->assertStringContainsString('<pdfaid:part>2</pdfaid:part><pdfaid:conformance>U</pdfaid:conformance>', $pdf->bytes());

        $pdf = Fagus::html('<p>Archiv</p>')->pdfA('3B')->facturX('<x/>', InvoiceProfile::Extended)->render();
        $this->assertStringContainsString('<fx:ConformanceLevel>EXTENDED</fx:ConformanceLevel>', $pdf->bytes());
    }

    #[Test]
    public function it_writes_pdf_a_1_and_pdf_a_4(): void
    {
        $pdf = Fagus::html('<p>Archiv</p>')->pdfA(Conformance::PdfA1b)->render();
        $this->assertStringStartsWith('%PDF-1.4', $pdf->bytes());
        $this->assertStringContainsString('<pdfaid:part>1</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>', $pdf->bytes());

        $pdf = Fagus::html('<p>Archiv</p>')->pdfA('4F')->render();
        $this->assertStringStartsWith('%PDF-2.0', $pdf->bytes());
        $this->assertStringContainsString('<pdfaid:part>4</pdfaid:part><pdfaid:rev>2020</pdfaid:rev><pdfaid:conformance>F</pdfaid:conformance>', $pdf->bytes());

        try {
            Fagus::html('<p style="opacity: 0.5">Archiv</p>')->pdfA('1b')->render();
            $this->fail('No exception for transparency in PDF/A-1b.');
        } catch (RenderException $exception) {
            $this->assertStringContainsString('PDF/A-1 forbids transparency, which the document uses for `opacity`, first on page 1', $exception->getMessage());
        }

        $this->expectException(RenderException::class);
        $this->expectExceptionMessage('the invoice must be PDF/A-3, not PDF/A-4f');
        Fagus::html('<p>Rechnung</p>')->pdfA(Conformance::PdfA4f)->facturX('<x/>', InvoiceProfile::Extended)->render();
    }

    #[Test]
    public function it_writes_tagged_and_accessible_pdf(): void
    {
        $html = '<!doctype html><html lang="de"><title>Rechnung</title><h1>Rechnung</h1><p>Danke.</p>';
        $this->assertStringContainsString('/StructTreeRoot', self::decoded(Fagus::html($html)->render()->bytes()), 'tagged by default');
        $this->assertStringNotContainsString('/StructTreeRoot', self::decoded(Fagus::html($html)->tagged(false)->render()->bytes()));

        $pdf = Fagus::html($html)->accessible()->pdfA(Conformance::PdfA3a)->render();
        $this->assertStringContainsString('<pdfuaid:part>1</pdfuaid:part>', $pdf->bytes());
        $this->assertStringContainsString('<pdfaid:conformance>A</pdfaid:conformance>', $pdf->bytes());

        $this->expectException(RenderException::class);
        $this->expectExceptionMessage('has no alternative text');
        Fagus::html($html.'<img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGNQd7wJRAwQCgAgYgUFHdy6BAAAAABJRU5ErkJggg==">')
            ->accessible()
            ->render();
    }

    #[Test]
    public function it_makes_form_fields(): void
    {
        $html = '<!doctype html><p><input name="name" value="Ada"> <input type="checkbox" name="ok" checked>';
        $this->assertStringNotContainsString('/AcroForm', self::decoded(Fagus::html($html)->render()->bytes()), 'off by default');

        $document = Fagus::html($html);
        $forms = $document->forms();
        $this->assertNotSame($document, $forms);
        $pdf = self::decoded($forms->render()->bytes());
        $this->assertStringContainsString('/AcroForm', $pdf);
        $this->assertStringContainsString('/T (name)', $pdf);
        $this->assertStringContainsString('/V (Ada)', $pdf);
        $this->assertStringContainsString('/FT/Btn/V /on', $pdf);
        $this->assertStringNotContainsString('/AcroForm', self::decoded($forms->forms(false)->render()->bytes()));
    }

    #[Test]
    public function it_saves_files(): void
    {
        $path = sys_get_temp_dir().DIRECTORY_SEPARATOR.'fagus '.bin2hex(random_bytes(6)).'.pdf';
        try {
            $pdf = Fagus::html('<p>Datei</p>')->save($path);
            $this->assertSame($pdf->bytes(), file_get_contents($path));
        } finally {
            @unlink($path);
        }
    }

    #[Test]
    #[RunInSeparateProcess]
    public function it_sends_pdfs_to_the_browser(): void
    {
        ob_start();
        $inline = Fagus::html('<p>Browser</p>')->inline('invoice.pdf');
        $download = Fagus::html('<p>Download</p>')->download('invoice.pdf');
        $output = (string) ob_get_clean();

        $this->assertSame($inline->bytes().$download->bytes(), $output);
        $this->assertStringStartsWith('%PDF-', $output);
    }

    #[Test]
    public function it_reports_where_elements_ended_up_when_asked(): void
    {
        $rows = '';
        for ($row = 0; $row < 20; $row++) {
            $rows .= "<tr><td>Row {$row}</td></tr>";
        }
        $document = Fagus::html(
            '<!DOCTYPE html><style>@page { size: 400px 300px; margin: 20px } body { margin: 0; font: 14px/20px Inter }
             h1 { margin: 0; height: 50px; font-size: 14px } table { border-spacing: 0 } td, th { padding: 0 }</style>
             <h1 id="title">Invoice</h1><table id="items"><thead><tr><th>Item</th></tr></thead><tbody>'.$rows.'</tbody></table>
             <p id="total" style="margin: 0">Total</p><span id="hidden" style="display: none"></span>',
        );

        try {
            $document->render()->layout();
            $this->fail('A layout without reportLayout().');
        } catch (FagusException $exception) {
            $this->assertSame('The layout was not reported; render the document with reportLayout() to read it, with a fagus binary of this release.', $exception->getMessage());
        }

        $layout = $document->reportLayout()->render()->layout();

        $this->assertCount(2, $layout->pages);
        $this->assertEquals(new Page(2, 300.0, 225.0), $layout->page(2));
        $this->assertSame(['title', 'items', 'total', 'hidden'], array_map(fn (Element $element) => $element->id, $layout->elements));
        $title = $layout->element('title');
        $this->assertEquals([new Box(1, 15.0, 15.0, $title?->boxes[0]->width ?? 0.0, 37.5)], $title?->boxes);
        // The page area is 195pt tall: the title, the header and nine rows fit on the first page.
        $items = $layout->element('items');
        $this->assertSame('table', $items?->tag);
        $this->assertSame([1, 2], $items->pages());
        $this->assertSame(Section::Header, $items->rows[0]->section);
        $this->assertSame([1, 2], $items->rows[0]->pages());
        $this->assertSame([[1], [2]], [$items->rows[9]->pages(), $items->rows[10]->pages()]);
        $this->assertSame(9, $items->rows[10]->index);
        $this->assertCount(10, $items->rowsOnPage(1));
        $total = $layout->element('total');
        $this->assertSame(2, $total?->lastPage());
        $this->assertSame(195.0, $total->boxes[0]->y);
        $this->assertSame(210.0, $total->boxes[0]->bottom());
        $this->assertSame([], $layout->element('hidden')?->boxes);
        $this->assertNull($layout->element('hidden')->firstPage());
        $this->assertNull($layout->element('missing'));
    }

    #[Test]
    public function it_grows_a_page_with_its_content(): void
    {
        $pdf = Fagus::html(
            '<!DOCTYPE html><style>@page { size: 80mm 40mm; margin: 4mm; -fagus-page-height: fit-content }
             body { margin: 0 } div { height: 400px }</style><div id="receipt"></div>',
        )->reportLayout()->render();

        $this->assertSame(1, $pdf->pages());
        $this->assertSame([], $pdf->diagnostics());
        $page = $pdf->layout()->page(1);
        $this->assertSame(226.77, $page?->width);
        // 400px and 8mm of margins are 322.68pt, rounded up to whole points.
        $this->assertSame(323.0, $page->height);
    }

    #[Test]
    public function documents_do_not_change(): void
    {
        $document = Document::html('<p>x</p>');
        $strict = $document->strict();

        $this->assertNotSame($document, $strict);
        $this->assertSame([], $document->render()->diagnostics());
    }

    #[Test]
    public function it_recovers_when_the_resolver_fails(): void
    {
        $document = Fagus::html('<img src="bild.png">')->resolver(function (): ?string {
            throw new RuntimeException('Kein Zugriff');
        });

        try {
            $document->render();
            $this->fail('No exception from the resolver.');
        } catch (RuntimeException $exception) {
            $this->assertSame('Kein Zugriff', $exception->getMessage());
        }
        $this->assertSame(1, Fagus::html('<p>Weiter</p>')->render()->pages(), 'a new worker takes over');
    }

    #[Test]
    public function it_adds_fonts(): void
    {
        $this->assertGreaterThan(0, self::$engine->addFont((string) file_get_contents(self::FONTS.'/Inter-Italic.ttf')));

        $this->expectException(EngineException::class);
        self::$engine->addFont('keine Schrift');
    }

    /**
     * The bytes of a PDF followed by the decoded contents of its object streams, where the
     * dictionaries of the document are.
     */
    private static function decoded(string $pdf): string
    {
        preg_match_all('~/Type/ObjStm/N \d+/First \d+/Filter/FlateDecode/Length (\d+)>>\nstream\n~', $pdf, $streams, PREG_OFFSET_CAPTURE);
        $decoded = $pdf;
        foreach ($streams[0] as $index => [$header, $offset]) {
            $data = substr($pdf, $offset + strlen($header), (int) $streams[1][$index][0]);
            $decoded .= (string) gzuncompress($data);
        }

        return $decoded;
    }

    /**
     * The streams of a PDF that hold what it draws (contents, fonts, images): all but the
     * metadata, the object streams and the cross-reference stream.
     *
     * @return list<string>
     */
    private static function drawn(string $pdf): array
    {
        preg_match_all('~<<(.*?)>>\nstream\n~s', $pdf, $streams, PREG_OFFSET_CAPTURE);
        $drawn = [];
        foreach ($streams[0] as $index => [$header, $offset]) {
            $dictionary = $streams[1][$index][0];
            if (preg_match('~/Type/(Metadata|ObjStm|XRef)\b~', $dictionary) === 1 || preg_match('~/Length (\d+)~', $dictionary, $length) !== 1) {
                continue;
            }
            $drawn[] = substr($pdf, $offset + strlen($header), (int) $length[1]);
        }

        return $drawn;
    }

    #[Test]
    public function it_writes_form_controls_in_the_engines_locale_unless_lang_says_otherwise(): void
    {
        // What is drawn; the metadata differ, as the locale is the language of a document without one.
        $render = static fn (Engine $engine, string $html): array => self::drawn(Document::html($html)
            ->tagged(false)
            ->created(new DateTimeImmutable('2026-11-05 12:00:00', new DateTimeZone('Europe/Berlin')))
            ->render($engine)
            ->bytes());
        $controls = '<input type=submit> <input type=date value=2026-11-04>';
        $german = new Engine(self::$binary, [self::FONTS], locale: 'de-DE');

        $this->assertNotSame($render(self::$engine, $controls), $render($german, $controls));
        $this->assertSame($render(self::$engine, '<body lang=de>'.$controls), $render($german, $controls));
        $this->assertSame($render(self::$engine, $controls), $render($german, '<body lang=en>'.$controls));
    }

    #[Test]
    public function it_renders_in_the_sandbox_unless_turned_off(): void
    {
        $free = new Engine(self::$binary, [self::FONTS], sandbox: false);

        $this->assertSame('off', $free->sandbox());
        $this->assertStringStartsWith('%PDF-', Document::html('<p>x</p>')->render($free)->bytes());
        if (PHP_OS_FAMILY === 'Linux') {
            $this->assertContains(self::$engine->sandbox(), ['on', 'partial']);
        } else {
            $this->assertSame('off', self::$engine->sandbox());
        }
    }

    #[Test]
    public function it_explains_a_missing_binary(): void
    {
        $this->expectException(EngineException::class);
        $this->expectExceptionMessage('could not be started');
        (new Engine('/nicht/vorhanden/fagus'))->render(Document::html('<p>x</p>'));
    }

    #[Test]
    public function it_renders_xhtml(): void
    {
        // An empty-element tag closes the `div` in XHTML only; in HTML it holds the paragraph.
        $markup = '<html xmlns="http://www.w3.org/1999/xhtml"><body><div style="break-after: page"/><p>Zwei</p></body></html>';

        $this->assertSame(2, Fagus::xhtml($markup)->render()->pages());
        $this->assertSame(1, Fagus::html($markup)->render()->pages());
    }

    #[Test]
    public function it_keeps_added_fonts_for_a_new_worker(): void
    {
        $engine = new Engine(self::$binary);
        $html = '<p style="font: italic bold 12px Inter">Kursiv</p>';
        $this->assertStringNotContainsString('Inter-BoldItalic', self::decoded(Document::html($html)->render($engine)->bytes()));

        $engine->addFont((string) file_get_contents(self::FONTS.'/Inter-BoldItalic.ttf'));
        $this->assertStringContainsString('Inter-BoldItalic', self::decoded(Document::html($html)->render($engine)->bytes()));

        $engine->close();
        $this->assertStringContainsString('Inter-BoldItalic', self::decoded(Document::html($html)->render($engine)->bytes()), 'the new worker has the font');
    }

    #[Test]
    public function it_replaces_workers_that_reach_their_limits(): void
    {
        $html = '<p style="font: italic bold 12px Inter">Kursiv</p>';
        // Every worker has more than a megabyte, where the binary tells (Linux, macOS); one
        // document each otherwise.
        foreach ([new Engine(self::$binary, maxMemory: 1), new Engine(self::$binary, maxDocuments: 1)] as $engine) {
            $engine->addFont((string) file_get_contents(self::FONTS.'/Inter-BoldItalic.ttf'));
            for ($i = 0; $i < 3; $i++) {
                $this->assertStringContainsString('Inter-BoldItalic', self::decoded(Document::html($html)->render($engine)->bytes()), 'each new worker has the font');
            }
            $engine->close();
        }
    }

    #[Test]
    public function it_passes_large_documents_and_binary_data_unchanged(): void
    {
        // Bytes that text mode would change on Windows (line ends, end of file), and many more.
        $data = "\r\n\n\r\x1a\x00".random_bytes(4 << 20)."\r\n";
        $html = '<!-- '.str_repeat('Füllung ', 1 << 20).' --><p>Groß</p>';

        $pdf = Fagus::html($html)->attach('daten.bin', $data)->render();

        $this->assertSame(1, $pdf->pages());
        $this->assertStringContainsString($data, $pdf->bytes());
        $this->assertStringEndsWith("%%EOF\n", $pdf->bytes());
    }

    #[Test]
    public function it_stops_a_worker_that_does_not_answer_in_time(): void
    {
        if (PHP_OS_FAMILY === 'Windows') {
            $this->markTestSkipped('PHP cannot wait for pipes with a time limit on Windows.');
        }
        // Without a time limit in the worker, which would end the document first.
        $engine = new Engine(self::$binary, [self::FONTS], timeout: 0.2, timeLimit: 0);
        $html = str_repeat('<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>', 100_000);
        $start = microtime(true);

        try {
            Document::html($html)->render($engine);
            $this->fail('No timeout.');
        } catch (EngineException $exception) {
            $this->assertStringContainsString('did not answer within 0.2 seconds', $exception->getMessage());
        }
        // Stopped at once rather than waited for until it has rendered the document.
        $this->assertLessThan(2.0, microtime(true) - $start);
        $this->assertSame(1, Document::html('<p>Weiter</p>')->render($engine)->pages(), 'a new worker takes over');
    }

    #[Test]
    public function it_ends_documents_past_their_limits_and_keeps_the_worker(): void
    {
        $long = '<style>p { border: 1px solid }</style>'.str_repeat('<p>Lorem ipsum dolor sit amet.</p>', 20_000);
        foreach ([
            'limit.memory' => new Engine(self::$binary, [self::FONTS], memoryBudget: 1),
            'limit.time' => new Engine(self::$binary, [self::FONTS], timeLimit: 0.05),
        ] as $code => $engine) {
            try {
                Document::html($long)->render($engine);
                $this->fail("No {$code}.");
            } catch (RenderException $exception) {
                $this->assertSame([$code], array_map(static fn (Diagnostic $diagnostic): string => $diagnostic->code, $exception->diagnostics()));
            }
            $this->assertSame(1, Document::html('<p>Weiter</p>')->render($engine)->pages(), 'the worker goes on');
        }
    }

    #[Test]
    public function it_explains_why_the_worker_stopped(): void
    {
        if (PHP_OS_FAMILY === 'Windows') {
            $this->markTestSkipped('The stand-in binary is a shell script.');
        }
        // A binary that fails at once, with a message on its error output.
        $binary = sys_get_temp_dir().DIRECTORY_SEPARATOR.'fagus-failing-'.bin2hex(random_bytes(6));
        file_put_contents($binary, "#!/bin/sh\necho 'Error: reading fonts in /kaputt' >&2\nexit 1\n");
        chmod($binary, 0755);
        try {
            (new Engine($binary))->render(Document::html('<p>x</p>'));
            $this->fail('No exception from a worker that stopped.');
        } catch (EngineException $exception) {
            $this->assertMatchesRegularExpression('~stopped\. It exited with code 1\. .*reading fonts in /kaputt~s', $exception->getMessage());
        } finally {
            @unlink($binary);
        }
    }

    #[Test]
    public function it_names_a_font_directory_that_does_not_exist(): void
    {
        $engine = new Engine(self::$binary, [self::FONTS, '/nicht/vorhanden']);

        $this->expectException(EngineException::class);
        $this->expectExceptionMessage('The font directory `/nicht/vorhanden` does not exist.');
        $engine->render(Document::html('<p>x</p>'));
    }

    #[Test]
    public function it_describes_attachments_by_their_names(): void
    {
        // PDF/A-3 and PDF/UA require a description; without one, the name describes the file.
        $html = '<!doctype html><html lang="de"><title>Anhang</title><p>Anhang</p>';
        $archive = Fagus::html($html)->attach('daten.csv', "a;b\n", 'text/csv', Relationship::Data)->pdfA('3b')->render();
        $accessible = Fagus::html($html)->attach('daten.csv', "a;b\n", 'text/csv')->accessible()->render();

        $this->assertStringContainsString('/Desc(daten.csv)', self::decoded($archive->bytes()));
        $this->assertStringContainsString('/Desc(daten.csv)', self::decoded($accessible->bytes()));
    }

    #[Test]
    public function it_keeps_the_diagnostics_of_documents_that_fail(): void
    {
        try {
            Fagus::html('<style>p { colr: red }</style><p>Brief</p>')->accessible()->render();
            $this->fail('No exception for a document that is not PDF/UA.');
        } catch (RenderException $exception) {
            $this->assertStringContainsString('PDF/UA-1', $exception->getMessage());
            $this->assertSame(['css.unknown-property'], array_map(static fn ($diagnostic): string => $diagnostic->code, $exception->diagnostics()));
        }
    }

    #[Test]
    public function it_counts_diagnostics_in_strict_mode(): void
    {
        $this->expectException(RenderException::class);
        $this->expectExceptionMessageMatches('~^1 diagnostic in strict mode:\n~');
        Fagus::html('<style>p { colr: red }</style><p>x</p>')->strict()->render();
    }

    #[Test]
    public function it_encrypts_pdfs_with_passwords(): void
    {
        $html = '<title>Gehalt</title><p>Gehaltsabrechnung</p>';
        $pdf = Fagus::html($html)->encrypt('user', 'owner', [Permission::Print, 'COPY'])->render();
        $bytes = $pdf->bytes();
        $this->assertSame(1, $pdf->pages());
        $this->assertMatchesRegularExpression('~/Filter/Standard/V 5/R 6/.*/P -1324/~', $bytes, 'print and copy');
        $this->assertStringNotContainsString('Gehalt', $bytes, 'the metadata is encrypted too');
        $this->assertNotSame($bytes, Fagus::html($html)->encrypt('user', 'owner', [Permission::Print, 'COPY'])->render()->bytes(), 'random salts');

        [$header] = Fagus::html($html)->encrypt(ownerPassword: 'owner')->request(1);
        $this->assertSame(['ownerPassword' => 'owner'], $header['encryption']);
        [$header] = Fagus::html($html)->encrypt('user', permissions: [])->request(1);
        $this->assertSame(['userPassword' => 'user', 'permissions' => []], $header['encryption']);

        $qpdf = trim((string) shell_exec('command -v qpdf 2>/dev/null'));
        if ($qpdf !== '') {
            $file = tempnam(sys_get_temp_dir(), 'fagus');
            $pdf->save($file);
            $run = static function (string $password, string $action = '--show-encryption') use ($qpdf, $file): array {
                exec(escapeshellarg($qpdf).' --password='.escapeshellarg($password).' '.$action.' '.escapeshellarg($file).' 2>&1', $output, $status);

                return [$status, implode("\n", $output)];
            };
            [$status, $output] = $run('user');
            $this->assertSame(0, $status, $output);
            $this->assertStringContainsString('print high resolution: allowed', $output);
            $this->assertStringContainsString('extract for any purpose: allowed', $output);
            $this->assertStringContainsString('modify other: not allowed', $output);
            $this->assertSame(0, $run('user', '--check')[0]);
            $this->assertStringContainsString('Supplied password is owner password', $run('owner')[1]);
            $this->assertSame(2, $run('wrong', '--check')[0], 'a wrong password fails');
            unlink($file);
        }

        // Only an owner password: opens without one, restricted.
        $bytes = Fagus::html($html)->encrypt(ownerPassword: 'owner', permissions: [])->render()->bytes();
        $this->assertMatchesRegularExpression('~/P -3392/~', $bytes);

        try {
            Fagus::html($html)->encrypt('', null);
            $this->fail('No exception without a password.');
        } catch (InvalidArgumentException $exception) {
            $this->assertSame('Encryption needs a user password or an owner password.', $exception->getMessage());
        }
        try {
            Fagus::html($html)->encrypt('user', permissions: ['everything']);
            $this->fail('No exception for an unknown permission.');
        } catch (InvalidArgumentException $exception) {
            $this->assertStringStartsWith("Unknown permission 'everything'. The permissions are 'print', 'print-low-quality'", $exception->getMessage());
        }

        $this->expectException(RenderException::class);
        $this->expectExceptionMessage('the document does not meet PDF/A-3b: the PDF cannot be encrypted (PDF/A forbids encryption)');
        Fagus::html($html)->pdfA()->encrypt('user')->render();
    }

    #[Test]
    public function it_encrypts_accessible_pdfs_with_passwords_beyond_ascii(): void
    {
        $html = '<!DOCTYPE html><html lang="de"><title>Gehalt</title><h1>Gehalt</h1><p>Gehaltsabrechnung</p>';
        // Decomposed umlauts and a no-break space: SASLprep makes them "Pässwörd 1".
        $pdf = Fagus::html($html)->accessible()->encrypt("Pa\u{308}sswo\u{308}rd\u{a0}1", permissions: [])->render();
        // Nothing allowed but the extraction for accessibility, which PDF/UA requires.
        $this->assertMatchesRegularExpression('~/P -3392/~', $pdf->bytes());

        $qpdf = trim((string) shell_exec('command -v qpdf 2>/dev/null'));
        if ($qpdf !== '') {
            $file = tempnam(sys_get_temp_dir(), 'fagus');
            $pdf->save($file);
            exec(escapeshellarg($qpdf).' --password='.escapeshellarg('Pässwörd 1').' --show-encryption '.escapeshellarg($file).' 2>&1', $output, $status);
            $this->assertSame(0, $status, implode("\n", $output));
            $this->assertContains('extract for accessibility: allowed', $output);
            unlink($file);
        }

        $this->expectException(RenderException::class);
        $this->expectExceptionMessage('the user password cannot be used: it contains a character that SASLprep');
        Fagus::html($html)->encrypt("geheim\u{7}")->render();
    }

    #[Test]
    public function it_names_the_pdf_a_levels_for_an_unknown_one(): void
    {
        try {
            Fagus::html('<p>x</p>')->pdfA('4z');
            $this->fail('No exception for an unknown level.');
        } catch (InvalidArgumentException $exception) {
            $this->assertInstanceOf(FagusException::class, $exception);
            $this->assertSame("Unknown PDF/A level '4z'. The levels are '1b', '2b', '2u', '2a', '3b', '3u', '3a', '4', '4e', '4f'.", $exception->getMessage());
        }
    }

    #[Test]
    public function it_takes_resources_handed_over_then_the_resolver_then_files(): void
    {
        $directory = sys_get_temp_dir().DIRECTORY_SEPARATOR.'fagus-resources-'.bin2hex(random_bytes(6));
        mkdir($directory);
        // Each unknown property names the source that was used.
        foreach (['handed.css' => 'datei', 'resolved.css' => 'datei', 'file.css' => 'datei'] as $name => $source) {
            file_put_contents($directory.DIRECTORY_SEPARATOR.$name, "p { {$source}-{$name[0]}: 1 }");
        }
        $asked = [];
        try {
            $pdf = Fagus::html('<link rel=stylesheet href=handed.css><link rel=stylesheet href=resolved.css><link rel=stylesheet href=file.css><p>x</p>')
                ->baseDirectory($directory)
                ->resource('handed.css', 'p { uebergeben-h: 1 }')
                ->resolver(function (string $url) use (&$asked): ?string {
                    $asked[] = $url;

                    return $url === 'resolved.css' ? 'p { resolver-r: 1 }' : null;
                })
                ->render();
        } finally {
            array_map(unlink(...), (array) glob($directory.DIRECTORY_SEPARATOR.'*.css'));
            rmdir($directory);
        }

        $this->assertSame(['resolved.css', 'file.css'], $asked);
        $messages = implode("\n", array_map(strval(...), $pdf->diagnostics()));
        $this->assertStringContainsString('uebergeben-h', $messages);
        $this->assertStringContainsString('resolver-r', $messages);
        $this->assertStringContainsString('datei-f', $messages);
        $this->assertStringNotContainsString('datei-h', $messages);
        $this->assertStringNotContainsString('datei-r', $messages);
    }

    #[Test]
    public function it_finds_the_binary(): void
    {
        $directory = sys_get_temp_dir().DIRECTORY_SEPARATOR.'fagus path '.bin2hex(random_bytes(6));
        mkdir($directory);
        $link = $directory.DIRECTORY_SEPARATOR.Binary::name();
        if (! @symlink((string) realpath(self::$binary), $link)) {
            copy(self::$binary, $link);
        }
        $path = getenv('PATH');
        $environment = getenv('FAGUS_BINARY');
        try {
            putenv('FAGUS_BINARY');
            putenv('PATH='.$directory.PATH_SEPARATOR.$path);
            $this->assertSame($link, Binary::locate());
            $this->assertMatchesRegularExpression('/^\d+\.\d+\.\d+/', (new Engine)->version(), 'on the PATH, in a directory with a space');

            putenv('FAGUS_BINARY='.$directory.DIRECTORY_SEPARATOR.'fehlt');
            try {
                Binary::locate();
                $this->fail('A missing binary in FAGUS_BINARY was passed over.');
            } catch (EngineException $exception) {
                $this->assertStringContainsString('fehlt` could not be started', $exception->getMessage());
            }
        } finally {
            putenv('PATH='.$path);
            putenv($environment === false ? 'FAGUS_BINARY' : 'FAGUS_BINARY='.$environment);
            unlink($link);
            rmdir($directory);
        }
    }
}
