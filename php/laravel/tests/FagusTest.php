<?php

declare(strict_types=1);

namespace Fagus\Laravel\Tests;

use DateTimeImmutable;
use DateTimeZone;
use Fagus\Engine;
use Fagus\Internal\Release;
use Fagus\Laravel\Facades\Fagus;
use Fagus\Laravel\PdfFactory;
use Fagus\Laravel\PendingPdf;
use Fagus\PageLayout;
use Fagus\PageMode;
use Fagus\Permission;
use Fagus\Signature;
use Fagus\Tests\Fixtures\FakeWorker;
use Fagus\Zoom;
use Illuminate\Http\Client\Pool;
use Illuminate\Http\Client\Response;
use Illuminate\Support\Facades\Cache;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Log;
use Illuminate\Support\Facades\Route;
use Illuminate\Support\Facades\Storage;
use Orchestra\Testbench\Attributes\DefineEnvironment;
use PHPUnit\Framework\Attributes\Test;

final class FagusTest extends TestCase
{
    #[Test]
    public function it_renders_blade_views(): void
    {
        $pdf = Fagus::view('invoice', ['number' => 'RE-1', 'lines' => ['Beratung', 'Entwicklung']])->render();

        $this->assertStringStartsWith('%PDF-', $pdf->bytes());
        $this->assertSame(1, $pdf->pages());
        $this->assertSame([], $pdf->diagnostics());
    }

    #[Test]
    public function it_reports_the_layout(): void
    {
        $pdf = Fagus::view('invoice', ['number' => 'RE-1', 'lines' => ['Beratung']])->reportLayout()->render();

        $this->assertSame(1, $pdf->layout()->page(1)?->number);
        $this->assertCount($pdf->pages(), $pdf->layout()->pages);
    }

    #[Test]
    public function it_downloads_and_shows_pdfs(): void
    {
        $download = Fagus::html('<p>Hallo</p>')->name('rechnung.pdf')->download();
        $this->assertSame('application/pdf', $download->headers->get('Content-Type'));
        $this->assertSame('attachment; filename=rechnung.pdf', $download->headers->get('Content-Disposition'));

        $inline = Fagus::html('<p>Hallo</p>')->inline('Übersicht März.pdf');
        $this->assertStringStartsWith('inline; filename=', (string) $inline->headers->get('Content-Disposition'));
        $this->assertStringContainsString("filename*=utf-8''%C3%9Cbersicht%20M%C3%A4rz.pdf", (string) $inline->headers->get('Content-Disposition'));
    }

    #[Test]
    public function controllers_can_return_pending_pdfs(): void
    {
        Route::get('/rechnung', fn (): PendingPdf => Fagus::view('invoice', ['number' => 'RE-2', 'lines' => []])->name('re-2.pdf'));

        $response = $this->get('/rechnung');

        $response->assertOk();
        $response->assertHeader('Content-Type', 'application/pdf');
        $this->assertStringStartsWith('%PDF-', (string) $response->getContent());
    }

    #[Test]
    public function it_stores_pdfs_on_disks(): void
    {
        Storage::fake('local');

        $pdf = Fagus::html('<p>Archiv</p>')->pdfA('3b')->store('rechnungen/re-3.pdf');

        Storage::disk('local')->assertExists('rechnungen/re-3.pdf');
        $this->assertSame($pdf->bytes(), Storage::disk('local')->get('rechnungen/re-3.pdf'));
    }

    #[Test]
    public function it_shares_one_engine(): void
    {
        $this->assertSame($this->app->make(Engine::class), Fagus::engine());
        $this->assertSame(Fagus::engine(), $this->app->make('fagus')->engine());
    }

    #[Test]
    #[DefineEnvironment('withOctane')]
    public function it_warms_the_engine_under_octane(): void
    {
        $this->assertSame(['config', 'view', Engine::class], $this->app['config']->get('octane.warm'));

        // What Octane does: it resolves the services in `warm` when its worker boots, gives each
        // request a copy of the container and flushes the copy when the request ends.
        $engine = $this->app->make(Engine::class);
        $sandbox = clone $this->app;
        $this->assertSame($engine, $sandbox->make(Engine::class));
        $sandbox->flush();
        $this->assertSame($engine, $this->app->make(Engine::class));
    }

    #[Test]
    #[DefineEnvironment('withOctaneWarmingTheEngine')]
    public function it_warms_the_engine_once(): void
    {
        $this->assertSame([Engine::class, 'view'], $this->app['config']->get('octane.warm'));
    }

    #[Test]
    public function it_leaves_the_configuration_alone_without_octane(): void
    {
        $this->assertNull($this->app['config']->get('octane'));
    }

    protected function withOctane($app): void
    {
        $app['config']->set('octane.warm', ['config', 'view']);
    }

    protected function withOctaneWarmingTheEngine($app): void
    {
        $app['config']->set('octane.warm', [Engine::class, 'view']);
    }

    #[Test]
    public function it_reads_files_from_the_base_directory(): void
    {
        $this->app['config']->set('fagus.base_directory', __DIR__.'/../../../tests');
        $this->app->forgetInstance('fagus');
        $this->app->forgetInstance(PdfFactory::class);

        $pdf = Fagus::html('<img src="corpus/images/logo.png" width="40">')->render();

        $this->assertSame([], $pdf->diagnostics());
    }

    #[Test]
    public function it_fetches_many_resources_at_once_and_keeps_them_in_a_cache_store(): void
    {
        // The callback of the documentation: the URLs of a batch through `Http::pool()`.
        Http::fake([
            'cdn.example.com/a.svg' => Http::response('<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>'),
            'cdn.example.com/b.svg' => Http::response('<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="blue"/></svg>'),
            '*' => Http::response('', 404),
        ]);
        $batches = [];
        $pending = Fagus::html('<img src="https://cdn.example.com/a.svg"><img src="https://cdn.example.com/b.svg"><img src="https://cdn.example.com/c.svg">')
            ->resolveMany(function (array $urls) use (&$batches): array {
                $batches[] = $urls;
                $responses = Http::pool(fn (Pool $pool) => array_map(
                    fn (string $url) => $pool->as($url)->withoutRedirecting()->timeout(5)->get($url),
                    $urls,
                ));
                $found = [];
                foreach ($responses as $url => $response) {
                    if ($response instanceof Response && $response->status() === 200) {
                        $found[$url] = $response->body();
                    }
                }

                return $found;
            })
            ->cache(Cache::store('array'), 60);
        $this->assertInstanceOf(PendingPdf::class, $pending);

        $first = $pending->render();
        $this->assertSame([['https://cdn.example.com/a.svg', 'https://cdn.example.com/b.svg', 'https://cdn.example.com/c.svg']], $batches);
        $this->assertSame(['resource.not-loaded'], array_map(static fn ($diagnostic): string => $diagnostic->code, $first->diagnostics()));
        $this->assertTrue(Cache::store('array')->has('fagus.resource.'.hash('sha256', 'https://cdn.example.com/a.svg')));
        Http::assertSentCount(3);

        // The second document asks only for the image that was missing.
        $second = $pending->render();
        $this->assertSame(['https://cdn.example.com/c.svg'], $batches[1]);
        Http::assertSentCount(4);
        $this->assertSame(array_map(strval(...), $first->diagnostics()), array_map(strval(...), $second->diagnostics()));
    }

    #[Test]
    public function it_makes_form_fields(): void
    {
        $pdf = Fagus::html('<input name="name" value="Ada">')->forms()->render();

        $this->assertStringContainsString('/T (name)', self::decoded($pdf->bytes()));
        $this->assertStringNotContainsString('/AcroForm', self::decoded(Fagus::html('<input name="name">')->render()->bytes()));
    }

    #[Test]
    public function it_writes_form_controls_in_the_configured_locale(): void
    {
        // What is drawn; the metadata differ, as the locale is the language of a document without one.
        $render = static fn (string $html): array => self::drawn(Fagus::html($html)
            ->tagged(false)
            ->created(new DateTimeImmutable('2026-11-05 12:00:00', new DateTimeZone('Europe/Berlin')))
            ->render()
            ->bytes());
        $controls = '<input type=submit> <input type=date value=2026-11-04>';
        $english = $render($controls);
        $german = $render('<body lang=de>'.$controls);

        $this->app['config']->set('fagus.locale', 'de-DE');
        $this->app->forgetInstance(Engine::class);
        $this->app->forgetInstance('fagus');
        $this->app->forgetInstance(PdfFactory::class);
        Fagus::clearResolvedInstances();

        $this->assertNotSame($english, $render($controls));
        $this->assertSame($german, $render($controls));
    }

    #[Test]
    public function it_turns_the_sandbox_off_when_configured(): void
    {
        $this->app['config']->set('fagus.sandbox', false);
        $this->app->forgetInstance(Engine::class);

        $this->assertSame('off', $this->app->make(Engine::class)->sandbox());
    }

    #[Test]
    public function it_passes_the_version_and_description_of_invoices_on(): void
    {
        $xml = (string) file_get_contents(__DIR__.'/../../../tests/corpus/invoice/factur-x.xml');
        $bytes = Fagus::html('<p>Rechnung</p>')->facturX($xml, version: '1.0', description: 'Factur-X/ZUGFeRD Invoice')->render()->bytes();
        $this->assertStringContainsString('<fx:Version>1.0</fx:Version>', $bytes);
        $this->assertStringContainsString('/Desc(Factur-X/ZUGFeRD Invoice)', self::decoded($bytes));
    }

    #[Test]
    public function it_encrypts_pdfs(): void
    {
        $pdf = Fagus::html('<p>Gehalt</p>')->encrypt(userPassword: 'user', permissions: [Permission::Print])->name('gehalt.pdf');
        $bytes = $pdf->render()->bytes();
        $this->assertMatchesRegularExpression('~/Filter/Standard/V 5/R 6/.*/P -1340/~', $bytes);
        $this->assertStringNotContainsString('Gehalt', $bytes, 'the metadata is encrypted too');
        $this->assertSame(['userPassword' => 'user', 'permissions' => ['print']], $pdf->document()->request(1)[0]['encryption']);
    }

    #[Test]
    public function it_chooses_how_the_pdf_opens(): void
    {
        $pdf = Fagus::html('<p>Handbuch</p>')->initialView(PageMode::Thumbnails, PageLayout::OneColumn, Zoom::FitPage, 1, centerWindow: true);
        $this->assertSame(
            ['pageMode' => 'thumbnails', 'pageLayout' => 'one-column', 'zoom' => 'fit-page', 'page' => 1, 'centerWindow' => true],
            $pdf->document()->request(1)[0]['view'],
        );
        $this->assertStringContainsString('/PageMode/UseThumbs', self::decoded($pdf->render()->bytes()));
    }

    #[Test]
    public function it_signs_pdfs(): void
    {
        $signature = Signature::fromPkcs12(__DIR__.'/../../../tests/signing/ecdsa.p12', 'fagus-test-only')->reason('Rechnung');
        $bytes = Fagus::html('<p>Rechnung</p>')->sign($signature)->pdfA('3b')->render()->bytes();
        $this->assertStringContainsString('/SubFilter/ETSI.CAdES.detached', $bytes);
        $this->assertStringContainsString('/Reason(Rechnung)', $bytes);
    }

    #[Test]
    public function it_tags_pdfs_unless_configured_otherwise(): void
    {
        $tagged = fn (PendingPdf $pdf): bool => str_contains(self::decoded($pdf->render()->bytes()), '/StructTreeRoot');
        $this->assertTrue($tagged(Fagus::html('<p>Hallo</p>')));
        $this->assertFalse($tagged(Fagus::html('<p>Hallo</p>')->tagged(false)));

        $this->app['config']->set('fagus.tagged', false);
        $this->app->forgetInstance('fagus');
        $this->app->forgetInstance(PdfFactory::class);
        Fagus::clearResolvedInstances();

        $this->assertFalse($tagged(Fagus::html('<p>Hallo</p>')));
        $this->assertTrue($tagged(Fagus::html('<p>Hallo</p>')->tagged()));
    }

    #[Test]
    public function it_writes_images_as_jpegs_of_the_configured_quality(): void
    {
        $photo = (string) file_get_contents(__DIR__.'/../../../tests/corpus/images/photo.webp');
        $jpeg = static fn (PendingPdf $pdf): bool => str_contains($pdf->resource('photo.webp', $photo)->render()->bytes(), '/DCTDecode');
        $html = '<!doctype html><img src="photo.webp" width="200">';

        $this->assertNull($this->app['config']->get('fagus.image_quality'));
        $this->assertFalse($jpeg(Fagus::html($html)), 'without loss by default');
        $this->assertTrue($jpeg(Fagus::html($html)->imageQuality(80)));

        $this->app['config']->set('fagus.image_quality', 75);
        $this->app->forgetInstance('fagus');
        $this->app->forgetInstance(PdfFactory::class);
        Fagus::clearResolvedInstances();

        [$header] = Fagus::html($html)->document()->request(1);
        $this->assertSame(75, $header['imageQuality']);
        $this->assertTrue($jpeg(Fagus::html($html)));
        $this->assertFalse($jpeg(Fagus::html($html)->imageQuality(null)));
    }

    /**
     * The bytes of a PDF followed by the decoded contents of its object streams.
     */
    private static function decoded(string $pdf): string
    {
        preg_match_all('~/Type/ObjStm/N \d+/First \d+/Filter/FlateDecode/Length (\d+)>>\nstream\n~', $pdf, $streams, PREG_OFFSET_CAPTURE);
        $decoded = $pdf;
        foreach ($streams[0] as $index => [$header, $offset]) {
            $decoded .= (string) gzuncompress(substr($pdf, $offset + strlen($header), (int) $streams[1][$index][0]));
        }

        return $decoded;
    }

    /**
     * The streams of a PDF that hold what it draws: all but the metadata, the object streams and
     * the cross-reference stream.
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
    public function it_renders_xhtml_with_the_configuration(): void
    {
        $this->app['config']->set('fagus.base_directory', __DIR__.'/../../../tests');
        $this->app->forgetInstance('fagus');
        $this->app->forgetInstance(PdfFactory::class);
        Fagus::clearResolvedInstances();

        $pdf = Fagus::xhtml('<html xmlns="http://www.w3.org/1999/xhtml"><body><div style="break-after: page"/><img src="corpus/images/logo.png" width="40"/></body></html>')->render();

        $this->assertSame(2, $pdf->pages(), 'the empty div ends before the image');
        $this->assertSame([], $pdf->diagnostics(), 'the image is found in the base directory');
    }

    #[Test]
    public function it_logs_a_binary_of_another_release(): void
    {
        if (PHP_OS_FAMILY === 'Windows') {
            $this->markTestSkipped('The stand-in binary is a shell script.');
        }
        $binary = FakeWorker::create(version: '0.0.3');
        $this->app['config']->set('fagus.binary', $binary);
        $this->app->forgetInstance(Engine::class);
        Log::shouldReceive('warning')->once()->withArgs(static fn (string $message): bool => str_contains($message, "`{$binary}` is version 0.0.3"));
        try {
            $this->assertSame('0.0.3', $this->app->make(Engine::class)->version());
        } finally {
            $this->app->make(Engine::class)->close();
            unlink($binary);
        }
    }

    #[Test]
    public function it_replaces_the_worker_as_configured(): void
    {
        if (PHP_OS_FAMILY === 'Windows') {
            $this->markTestSkipped('The stand-in binary is a shell script.');
        }
        $binary = FakeWorker::create(version: Release::VERSION, memory: ['rss' => [1 << 20, 3 << 20]]);
        // As environment variables give them.
        $this->app['config']->set('fagus.binary', $binary);
        $this->app['config']->set('fagus.max_documents', '3');
        $this->app['config']->set('fagus.max_memory', '2');
        $this->app->forgetInstance(Engine::class);
        $this->app->forgetInstance('fagus');
        $this->app->forgetInstance(PdfFactory::class);
        Fagus::clearResolvedInstances();
        try {
            $numbers = [];
            for ($i = 0; $i < 4; $i++) {
                $numbers[] = FakeWorker::parse(Fagus::html('<p>x</p>')->render()->bytes())[1];
            }
            $this->assertSame([1, 2, 1, 2], $numbers, 'replaced when above 2 MB');

            $this->app['config']->set('fagus.max_memory', '');
            $this->app->make(Engine::class)->close();
            $this->app->forgetInstance(Engine::class);
            $this->app->forgetInstance('fagus');
            $this->app->forgetInstance(PdfFactory::class);
            Fagus::clearResolvedInstances();
            $numbers = [];
            for ($i = 0; $i < 4; $i++) {
                $numbers[] = FakeWorker::parse(Fagus::html('<p>x</p>')->render()->bytes())[1];
            }
            $this->assertSame([1, 2, 3, 1], $numbers, 'replaced after three documents');
        } finally {
            $this->app->make(Engine::class)->close();
            unlink($binary);
        }
    }

    #[Test]
    public function it_reports_why_artisan_cannot_install_the_binary(): void
    {
        // A source without the binary: the installer fails, with or without pinned binaries.
        $empty = sys_get_temp_dir().'/fagus-install-'.bin2hex(random_bytes(4));
        mkdir($empty);
        putenv("FAGUS_DOWNLOAD_URL={$empty}");
        try {
            $this->artisan('fagus:install', ['directory' => $empty.'/bin'])
                ->expectsOutputToContain(Release::BINARIES === [] ? 'a development version without built binaries' : 'could not be downloaded')
                ->assertFailed();
        } finally {
            putenv('FAGUS_DOWNLOAD_URL');
            rmdir($empty);
        }
    }
}
