<?php

declare(strict_types=1);

namespace Fagus\Laravel;

use DateInterval;
use DateTimeInterface;
use Fagus\Conformance;
use Fagus\Document;
use Fagus\Engine;
use Fagus\InvoiceProfile;
use Fagus\Pdf;
use Fagus\Permission;
use Fagus\Relationship;
use Fagus\Signature;
use Illuminate\Contracts\Support\Responsable;
use Illuminate\Http\Response;
use Illuminate\Support\Facades\Storage;
use Psr\SimpleCache\CacheInterface;
use SensitiveParameter;
use Symfony\Component\HttpFoundation\HeaderUtils;

/**
 * A PDF to render: the methods of {@see Document}, plus responses and storage. Every method
 * returns a changed copy. Returned from a controller, it shows the PDF in the browser.
 */
final class PendingPdf implements Responsable
{
    private string $filename = 'document.pdf';

    public function __construct(
        private readonly Engine $engine,
        private readonly Document $document,
    ) {}

    public function baseDirectory(string $directory): self
    {
        return $this->with($this->document->baseDirectory($directory));
    }

    public function root(string $directory): self
    {
        return $this->with($this->document->root($directory));
    }

    public function stylesheet(string $css, string $name = 'stylesheet'): self
    {
        return $this->with($this->document->stylesheet($css, $name));
    }

    public function resource(string $url, string $data): self
    {
        return $this->with($this->document->resource($url, $data));
    }

    /**
     * @param  callable(string): ?string  $resolver
     */
    public function resolver(callable $resolver): self
    {
        return $this->with($this->document->resolver($resolver));
    }

    /**
     * @param  callable(list<string>): array<string, string>  $resolver
     */
    public function resolveMany(callable $resolver): self
    {
        return $this->with($this->document->resolveMany($resolver));
    }

    public function cache(CacheInterface $cache, DateInterval|int|null $ttl = null, string $prefix = 'fagus.resource.'): self
    {
        return $this->with($this->document->cache($cache, $ttl, $prefix));
    }

    public function pdfA(Conformance|string $level = Conformance::PdfA3b): self
    {
        return $this->with($this->document->pdfA($level));
    }

    public function tagged(bool $tagged = true): self
    {
        return $this->with($this->document->tagged($tagged));
    }

    public function accessible(bool $accessible = true): self
    {
        return $this->with($this->document->accessible($accessible));
    }

    public function forms(bool $forms = true): self
    {
        return $this->with($this->document->forms($forms));
    }

    public function attach(
        string $name,
        string $data,
        string $mime = 'application/octet-stream',
        Relationship $relationship = Relationship::Unspecified,
        ?string $description = null,
    ): self {
        return $this->with($this->document->attach($name, $data, $mime, $relationship, $description));
    }

    public function facturX(
        string $xml,
        ?InvoiceProfile $profile = null,
        ?string $version = null,
        ?string $description = null,
    ): self {
        return $this->with($this->document->facturX($xml, $profile, $version, $description));
    }

    /**
     * @param  list<Permission|string>|null  $permissions
     */
    public function encrypt(
        #[SensitiveParameter] ?string $userPassword = null,
        #[SensitiveParameter] ?string $ownerPassword = null,
        ?array $permissions = null,
    ): self {
        return $this->with($this->document->encrypt($userPassword, $ownerPassword, $permissions));
    }

    public function created(DateTimeInterface $date): self
    {
        return $this->with($this->document->created($date));
    }

    public function sign(Signature $signature): self
    {
        return $this->with($this->document->sign($signature));
    }

    public function strict(bool $strict = true): self
    {
        return $this->with($this->document->strict($strict));
    }

    public function reportLayout(bool $reportLayout = true): self
    {
        return $this->with($this->document->reportLayout($reportLayout));
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
        return $this->document->render($this->engine);
    }

    public function save(string $path): Pdf
    {
        return $this->document->save($path, $this->engine);
    }

    /**
     * Stores the PDF on a filesystem disk.
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

    public function document(): Document
    {
        return $this->document;
    }

    private function with(Document $document): self
    {
        $copy = new self($this->engine, $document);
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
