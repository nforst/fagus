<?php

declare(strict_types=1);

namespace Fagus;

/**
 * HTML and CSS to PDF.
 *
 *     Fagus::html('<h1>Rechnung</h1>')->save('rechnung.pdf');
 */
final class Fagus
{
    private static ?Engine $engine = null;

    public static function html(string $html): Document
    {
        return Document::html($html);
    }

    /**
     * An XHTML document, parsed as XML.
     */
    public static function xhtml(string $xml): Document
    {
        return Document::xhtml($xml);
    }

    /**
     * PDFs to merge into one, every page of each, one after another: PDF files as strings, or
     * rendered {@see Pdf}s. {@see Merge::add()} adds more, or some pages of one.
     *
     * @param  iterable<Pdf|string>  $pdfs
     */
    public static function merge(iterable $pdfs = []): Merge
    {
        return Merge::of($pdfs);
    }

    /**
     * The engine documents render with unless one is given: the one set with
     * {@see self::useEngine()}, or one with the binary from FAGUS_BINARY, the installer or the
     * PATH.
     */
    public static function engine(): Engine
    {
        return self::$engine ??= new Engine;
    }

    public static function useEngine(?Engine $engine): void
    {
        self::$engine = $engine;
    }
}
