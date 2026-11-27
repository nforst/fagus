<?php

declare(strict_types=1);

namespace Fagus\Laravel;

use DateTimeInterface;
use Fagus\Engine;
use Fagus\Exception\FagusException;
use Fagus\Merge;
use Fagus\Pdf;
use Illuminate\Contracts\Support\Responsable;
use Illuminate\Http\Response;
use Illuminate\Support\Facades\Storage;
use Symfony\Component\HttpFoundation\HeaderUtils;

/**
 * PDFs to merge into one: the methods of {@see Merge}, plus responses and storage. Every method
 * returns a changed copy. Returned from a controller, it shows the PDF in the browser.
 */
final class PendingMerge implements Responsable
{
    private string $filename = 'document.pdf';

    public function __construct(
        private readonly Engine $engine,
        private readonly Merge $merge,
    ) {}

    /**
     * Adds a PDF: a PDF file as a string, a rendered {@see Pdf}, or a {@see PendingPdf}, which is
     * rendered now. Every page, or the pages `$pages`, such as `'1-3,5,8-'`.
     */
    public function add(Pdf|PendingPdf|string $pdf, int|string|null $pages = null): self
    {
        return $this->with($this->merge->add($pdf instanceof PendingPdf ? $pdf->render() : $pdf, $pages));
    }

    /**
     * Adds the PDF file at `$path`, read now.
     */
    public function addFile(string $path, int|string|null $pages = null): self
    {
        return $this->with($this->merge->addFile($path, $pages));
    }

    /**
     * Adds the PDF file at `$path` on a filesystem disk, read now.
     *
     * @throws FagusException when the disk has no such file
     */
    public function addFromDisk(string $path, ?string $disk = null, int|string|null $pages = null): self
    {
        $data = Storage::disk($disk)->get($path)
            ?? throw new FagusException("The PDF file `{$path}` could not be read from the disk.");

        return $this->with($this->merge->add($data, $pages));
    }

    public function title(?string $title): self
    {
        return $this->with($this->merge->title($title));
    }

    public function author(string ...$authors): self
    {
        return $this->with($this->merge->author(...$authors));
    }

    public function language(?string $language): self
    {
        return $this->with($this->merge->language($language));
    }

    public function created(DateTimeInterface $date): self
    {
        return $this->with($this->merge->created($date));
    }

    /**
     * The file name for downloads and the browser.
     */
    public function name(string $filename): self
    {
        $copy = clone $this;
        $copy->filename = $filename;

        return $copy;
    }

    public function render(): Pdf
    {
        return $this->merge->render($this->engine);
    }

    public function save(string $path): Pdf
    {
        return $this->merge->save($path, $this->engine);
    }

    /**
     * Stores the merged PDF on a filesystem disk.
     */
    public function store(string $path, ?string $disk = null): Pdf
    {
        $pdf = $this->render();
        Storage::disk($disk)->put($path, $pdf->bytes());

        return $pdf;
    }

    public function download(?string $filename = null): Response
    {
        return $this->response('attachment', $filename ?? $this->filename);
    }

    public function inline(?string $filename = null): Response
    {
        return $this->response('inline', $filename ?? $this->filename);
    }

    public function toResponse($request): Response
    {
        return $this->inline();
    }

    public function merge(): Merge
    {
        return $this->merge;
    }

    private function with(Merge $merge): self
    {
        $copy = new self($this->engine, $merge);
        $copy->filename = $this->filename;

        return $copy;
    }

    private function response(string $disposition, string $filename): Response
    {
        $fallback = preg_replace('/[^\x20-\x7e]|[\/\\\\%]/', '_', $filename) ?? 'document.pdf';

        return new Response($this->render()->bytes(), 200, [
            'Content-Type' => 'application/pdf',
            'Content-Disposition' => HeaderUtils::makeDisposition($disposition, $filename, $fallback),
        ]);
    }
}
