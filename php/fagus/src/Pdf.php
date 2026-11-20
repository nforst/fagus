<?php

declare(strict_types=1);

namespace Fagus;

use Fagus\Exception\FagusException;

/**
 * A rendered PDF with what Fagus noticed while rendering it.
 */
final readonly class Pdf
{
    /**
     * @param  list<Diagnostic>  $diagnostics
     * @param  array<string, float>  $timings  milliseconds per phase: parse, style, layout, paint, pdf
     * @param  ?Layout  $layout  where the elements ended up, if the document asked for it
     */
    public function __construct(
        private string $bytes,
        private int $pages,
        private array $diagnostics = [],
        private array $timings = [],
        private ?Layout $layout = null,
    ) {}

    /**
     * @param  array<string, mixed>  $reply
     *
     * @internal
     */
    public static function fromReply(array $reply, string $bytes): self
    {
        return new self(
            $bytes,
            (int) ($reply['pages'] ?? 0),
            array_values(array_map(Diagnostic::fromArray(...), $reply['diagnostics'] ?? [])),
            array_map(floatval(...), $reply['timings'] ?? []),
            is_array($reply['layout'] ?? null) ? Layout::fromArray($reply['layout']) : null,
        );
    }

    /**
     * The PDF file.
     */
    public function bytes(): string
    {
        return $this->bytes;
    }

    public function pages(): int
    {
        return $this->pages;
    }

    /**
     * @return list<Diagnostic>
     */
    public function diagnostics(): array
    {
        return $this->diagnostics;
    }

    /**
     * @return array<string, float>
     */
    public function timings(): array
    {
        return $this->timings;
    }

    /**
     * Where the elements with an `id` ended up: their pages and boxes, and the rows of their
     * tables.
     *
     * @throws FagusException when the document was rendered without {@see Document::reportLayout()}
     */
    public function layout(): Layout
    {
        return $this->layout ?? throw new FagusException(
            'The layout was not reported; render the document with reportLayout() to read it, with a fagus binary of this release.',
        );
    }

    public function save(string $path): void
    {
        if (@file_put_contents($path, $this->bytes) === false) {
            throw new FagusException("The PDF could not be written to `{$path}`.");
        }
    }

    /**
     * Sends the PDF to the browser to show it: `Content-Type`, `Content-Disposition: inline`
     * with `$filename`, `Content-Length` and the PDF.
     *
     * @throws FagusException when output was already sent, so that headers cannot be
     */
    public function inline(string $filename = 'document.pdf'): void
    {
        $this->send('inline', $filename);
    }

    /**
     * Sends the PDF to the browser as a download (`Content-Disposition: attachment`) named
     * `$filename`.
     *
     * @throws FagusException when output was already sent, so that headers cannot be
     */
    public function download(string $filename = 'document.pdf'): void
    {
        $this->send('attachment', $filename);
    }

    /**
     * The `Content-Disposition` header for `$filename`: control characters and slashes are left
     * out, and a name that is not plain ASCII also goes as `filename*` (RFC 6266), with an ASCII
     * `filename` for older clients.
     *
     * @internal
     */
    public static function contentDisposition(string $disposition, string $filename): string
    {
        $filename = str_replace(['/', '\\'], '_', (string) preg_replace('/[\x00-\x1F\x7F]/', '', $filename));
        if (trim($filename) === '') {
            $filename = 'document.pdf';
        }

        // One `_` for each character, or for each byte of a name that is not UTF-8.
        $fallback = (string) (preg_replace('/[^\x20-\x7E]|[%"]/u', '_', $filename)
            ?? preg_replace('/[^\x20-\x7E]|[%"]/', '_', $filename));
        $header = "{$disposition}; filename=\"{$fallback}\"";

        return $fallback === $filename ? $header : $header."; filename*=UTF-8''".rawurlencode($filename);
    }

    private function send(string $disposition, string $filename): void
    {
        if (headers_sent($file, $line)) {
            throw new FagusException("The PDF cannot be sent: output already started at {$file}:{$line}, before its headers.");
        }

        header('Content-Type: application/pdf');
        header('Content-Disposition: '.self::contentDisposition($disposition, $filename));
        header('Content-Length: '.strlen($this->bytes));
        echo $this->bytes;
    }
}
