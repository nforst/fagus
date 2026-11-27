<?php

declare(strict_types=1);

namespace Fagus\Laravel;

use Fagus\Document;
use Fagus\Engine;
use Fagus\Merge;
use Fagus\Pdf;
use Illuminate\Contracts\View\Factory as ViewFactory;

/**
 * Starts PDFs from HTML, XHTML or Blade views, with the base directory of the configuration.
 */
final readonly class PdfFactory
{
    /**
     * @param  array<string, mixed>  $config
     */
    public function __construct(
        private Engine $engine,
        private ViewFactory $views,
        private array $config,
    ) {}

    public function html(string $html): PendingPdf
    {
        return $this->pending(Document::html($html));
    }

    /**
     * An XHTML document, parsed as XML.
     */
    public function xhtml(string $xml): PendingPdf
    {
        return $this->pending(Document::xhtml($xml));
    }

    /**
     * @param  array<string, mixed>  $data
     * @param  array<string, mixed>  $mergeData
     */
    public function view(string $view, array $data = [], array $mergeData = []): PendingPdf
    {
        return $this->html($this->views->make($view, $data, $mergeData)->render());
    }

    /**
     * PDFs to merge into one, every page of each, one after another: PDF files as strings,
     * rendered {@see Pdf}s, or {@see PendingPdf}s, which are rendered now.
     *
     * @param  iterable<Pdf|PendingPdf|string>  $pdfs
     */
    public function merge(iterable $pdfs = []): PendingMerge
    {
        $merge = new PendingMerge($this->engine, Merge::of());
        foreach ($pdfs as $pdf) {
            $merge = $merge->add($pdf);
        }

        return $merge;
    }

    public function engine(): Engine
    {
        return $this->engine;
    }

    /**
     * A document with the base directory, root, tagging and image quality of the configuration.
     */
    private function pending(Document $document): PendingPdf
    {
        if (is_string($this->config['base_directory'] ?? null)) {
            $document = $document->baseDirectory($this->config['base_directory']);
        }
        if (is_string($this->config['root'] ?? null)) {
            $document = $document->root($this->config['root']);
        }
        $document = $document->tagged(($this->config['tagged'] ?? true) !== false);
        if (is_numeric($this->config['image_quality'] ?? null)) {
            $document = $document->imageQuality((int) $this->config['image_quality']);
        }

        return new PendingPdf($this->engine, $document);
    }
}
