<?php

declare(strict_types=1);

namespace Fagus;

use DateTimeImmutable;
use DateTimeInterface;
use DateTimeZone;
use Fagus\Exception\FagusException;
use Fagus\Exception\InvalidArgumentException;

/**
 * PDFs to merge into one: their pages, or some of them, one after another. Every method returns a
 * changed copy; the merge itself never changes.
 *
 *     Fagus::merge([$invoice, $terms])->save('rechnung.pdf');
 *
 * The pages keep their content, size and rotation. Their links, form fields, comments and
 * signatures are left out, as are the tags, outline, attachments (the invoice XML of Factur-X),
 * layers and metadata of the PDFs; the merged PDF is neither PDF/A nor PDF/UA. Encrypted PDFs
 * cannot be merged.
 */
final class Merge
{
    /** @var list<array{data: string, pages: ?string}> */
    private array $inputs = [];

    private ?string $title = null;

    /** @var list<string> */
    private array $authors = [];

    private ?string $language = null;

    private ?DateTimeInterface $created = null;

    private function __construct() {}

    /**
     * A merge of the PDFs `$pdfs`, every page of each: PDF files as strings, or rendered
     * {@see Pdf}s.
     *
     * @param  iterable<Pdf|string>  $pdfs
     */
    public static function of(iterable $pdfs = []): self
    {
        $merge = new self;
        foreach ($pdfs as $pdf) {
            $merge = $merge->add($pdf);
        }

        return $merge;
    }

    /**
     * Adds the PDF `$pdf` (a PDF file as a string, or a rendered {@see Pdf}): every page, or the
     * pages `$pages`, counted from 1, such as `2`, `'1-3'`, `'4-'` (to the last page) or
     * `'1-3,5,8-'`, in that order.
     */
    public function add(Pdf|string $pdf, int|string|null $pages = null): self
    {
        $copy = clone $this;
        $copy->inputs[] = ['data' => $pdf instanceof Pdf ? $pdf->bytes() : $pdf, 'pages' => self::pages($pages)];

        return $copy;
    }

    /**
     * Adds the PDF file at `$path`, read now, as {@see self::add()} adds the data of one.
     *
     * @throws FagusException when the file cannot be read
     */
    public function addFile(string $path, int|string|null $pages = null): self
    {
        $data = is_file($path) ? @file_get_contents($path) : false;
        if ($data === false) {
            throw new FagusException("The PDF file `{$path}` could not be read.");
        }

        return $this->add($data, $pages);
    }

    /**
     * The title of the merged PDF, which viewers show in place of the file name.
     */
    public function title(?string $title): self
    {
        $copy = clone $this;
        $copy->title = $title;

        return $copy;
    }

    /**
     * The authors of the merged PDF.
     */
    public function author(string ...$authors): self
    {
        $copy = clone $this;
        $copy->authors = array_values($authors);

        return $copy;
    }

    /**
     * The language of the merged PDF, such as "de-DE".
     */
    public function language(?string $language): self
    {
        $copy = clone $this;
        $copy->language = $language;

        return $copy;
    }

    /**
     * The creation date written into the merged PDF. Without one, it has none, and the same PDFs
     * give the same bytes.
     */
    public function created(DateTimeInterface $date): self
    {
        $copy = clone $this;
        $copy->created = $date;

        return $copy;
    }

    /**
     * How many PDFs are added.
     */
    public function count(): int
    {
        return count($this->inputs);
    }

    /**
     * @throws InvalidArgumentException when no PDF is added
     */
    public function render(?Engine $engine = null): Pdf
    {
        if ($this->inputs === []) {
            throw InvalidArgumentException::nothingToMerge();
        }

        return ($engine ?? Fagus::engine())->merge($this);
    }

    public function save(string $path, ?Engine $engine = null): Pdf
    {
        $pdf = $this->render($engine);
        $pdf->save($path);

        return $pdf;
    }

    /**
     * Merges the PDFs and sends the PDF to the browser to show it, as {@see Pdf::inline()}.
     */
    public function inline(string $filename = 'document.pdf', ?Engine $engine = null): Pdf
    {
        $pdf = $this->render($engine);
        $pdf->inline($filename);

        return $pdf;
    }

    /**
     * Merges the PDFs and sends the PDF to the browser as a download, as {@see Pdf::download()}.
     */
    public function download(string $filename = 'document.pdf', ?Engine $engine = null): Pdf
    {
        $pdf = $this->render($engine);
        $pdf->download($filename);

        return $pdf;
    }

    /**
     * The merge request of the worker protocol (`M`): its header and data blocks. A PDF added
     * more than once goes once.
     *
     * @return array{0: array<string, mixed>, 1: list<string>}
     *
     * @internal
     */
    public function request(int $id): array
    {
        $blobs = [];
        $header = ['id' => $id, 'inputs' => []];
        foreach ($this->inputs as $input) {
            $blob = array_search($input['data'], $blobs, true);
            if ($blob === false) {
                $blobs[] = $input['data'];
                $blob = count($blobs) - 1;
            }
            $header['inputs'][] = $input['pages'] === null ? ['blob' => $blob] : ['blob' => $blob, 'pages' => $input['pages']];
        }
        if ($this->title !== null) {
            $header['title'] = $this->title;
        }
        if ($this->authors !== []) {
            $header['authors'] = $this->authors;
        }
        if ($this->language !== null) {
            $header['language'] = $this->language;
        }
        if ($this->created !== null) {
            $header['created'] = DateTimeImmutable::createFromInterface($this->created)
                ->setTimezone(new DateTimeZone('UTC'))
                ->format('Y-m-d\TH:i:s\Z');
        }

        return [$header, $blobs];
    }

    /**
     * The pages `$pages` as the protocol takes them, checked as the engine checks them: page
     * numbers from 1 and ranges of them (`1-3`, `4-`), separated by commas.
     */
    private static function pages(int|string|null $pages): ?string
    {
        if ($pages === null) {
            return null;
        }
        $text = (string) $pages;
        foreach (explode(',', $text) as $item) {
            $numbers = explode('-', $item, 2);
            if (count($numbers) === 2 && trim($numbers[1]) === '') {
                $numbers = [$numbers[0]];
            }
            foreach ($numbers as $number) {
                if (preg_match('/^\s*\d+\s*$/', $number) !== 1) {
                    throw InvalidArgumentException::invalidPages($text, 'expected page numbers and ranges like `1-3,5,8-`');
                }
                if ((int) $number === 0) {
                    throw InvalidArgumentException::invalidPages($text, 'pages count from 1');
                }
            }
            if (count($numbers) === 2 && (int) $numbers[1] < (int) $numbers[0]) {
                throw InvalidArgumentException::invalidPages($text, 'a range ends before it begins');
            }
        }

        return $text;
    }
}
