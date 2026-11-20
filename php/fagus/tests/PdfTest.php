<?php

declare(strict_types=1);

namespace Fagus\Tests;

use Fagus\Exception\FagusException;
use Fagus\Pdf;
use PHPUnit\Framework\Attributes\RunInSeparateProcess;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\TestCase;

/**
 * Sending a PDF to the browser; needs no binary.
 */
final class PdfTest extends TestCase
{
    #[Test]
    public function the_disposition_names_the_file(): void
    {
        $this->assertSame('inline; filename="invoice.pdf"', Pdf::contentDisposition('inline', 'invoice.pdf'));
        $this->assertSame('attachment; filename="invoice.pdf"', Pdf::contentDisposition('attachment', 'invoice.pdf'));
    }

    #[Test]
    public function a_name_that_is_not_ascii_goes_as_utf8_with_an_ascii_fallback(): void
    {
        $this->assertSame(
            "inline; filename=\"Rechnung_M_rz.pdf\"; filename*=UTF-8''Rechnung%C3%9CM%C3%A4rz.pdf",
            Pdf::contentDisposition('inline', 'RechnungÜMärz.pdf'),
        );
        $this->assertSame(
            "inline; filename=\"a_b_c.pdf\"; filename*=UTF-8''a%22b%25c.pdf",
            Pdf::contentDisposition('inline', 'a"b%c.pdf'),
        );
        $this->assertSame(
            "inline; filename=\"__.pdf\"; filename*=UTF-8''%FF%FE.pdf",
            Pdf::contentDisposition('inline', "\xFF\xFE.pdf"),
        );
    }

    #[Test]
    public function control_characters_and_slashes_stay_out_of_the_header(): void
    {
        $this->assertSame(
            'inline; filename="_etc_passwdX-Evil: 1.pdf"',
            Pdf::contentDisposition('inline', "/etc\\passwd\r\nX-Evil: 1.pdf"),
        );
        $this->assertSame('inline; filename="document.pdf"', Pdf::contentDisposition('inline', "\n "));
    }

    #[Test]
    #[RunInSeparateProcess]
    public function inline_sends_the_pdf(): void
    {
        $this->expectOutputString('%PDF-1.7 test');

        (new Pdf('%PDF-1.7 test', 1))->inline('invoice.pdf');
    }

    #[Test]
    #[RunInSeparateProcess]
    public function download_sends_the_pdf(): void
    {
        $this->expectOutputString('%PDF-1.7 test');

        (new Pdf('%PDF-1.7 test', 1))->download('invoice.pdf');
    }

    #[Test]
    #[RunInSeparateProcess]
    public function the_pdf_is_not_sent_after_output(): void
    {
        $level = ob_get_level();
        while (ob_get_level() > 0) {
            ob_end_flush();
        }
        echo "\n";
        flush();
        for ($i = 0; $i < $level; $i++) {
            ob_start();
        }

        $this->expectException(FagusException::class);
        $this->expectExceptionMessageMatches('/^The PDF cannot be sent: output already started at .*PdfTest\.php:\d+, before its headers\.$/');
        (new Pdf('%PDF-1.7 test', 1))->inline();
    }
}
