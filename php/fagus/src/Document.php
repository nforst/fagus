<?php

declare(strict_types=1);

namespace Fagus;

use Closure;
use DateInterval;
use DateTimeImmutable;
use DateTimeInterface;
use DateTimeZone;
use Fagus\Exception\InvalidArgumentException;
use Fagus\Exception\RenderException;
use Psr\SimpleCache\CacheInterface;
use SensitiveParameter;
use TypeError;

/**
 * A document to render. Every method returns a changed copy; the document itself never changes.
 */
final class Document
{
    private ?string $base = null;

    private ?string $root = null;

    /** @var list<array{name: string, css: string}> */
    private array $stylesheets = [];

    /** @var array<string, string> */
    private array $resources = [];

    /** @var (Closure(string): ?string)|null */
    private ?Closure $resolver = null;

    /** @var (Closure(list<string>): array<string, string>)|null */
    private ?Closure $manyResolver = null;

    private ?CacheInterface $cache = null;

    private DateInterval|int|null $cacheTtl = null;

    private string $cachePrefix = 'fagus.resource.';

    private ?Conformance $conformance = null;

    private bool $tagged = true;

    private bool $accessible = false;

    private bool $forms = false;

    /** @var list<array{name: string, data: string, mime: string, relationship: Relationship, description: ?string}> */
    private array $attachments = [];

    private ?string $invoice = null;

    private ?InvoiceProfile $profile = null;

    private ?string $invoiceVersion = null;

    private ?string $invoiceDescription = null;

    private ?DateTimeInterface $created = null;

    private ?Signature $signature = null;

    private bool $strict = false;

    private bool $reportLayout = false;

    /** @var array{userPassword: ?string, ownerPassword: ?string, permissions: ?list<Permission>}|null */
    private ?array $encryption = null;

    private function __construct(
        private readonly string $html,
        private readonly bool $xhtml = false,
    ) {}

    public static function html(string $html): self
    {
        return new self($html);
    }

    /**
     * An XHTML document, parsed as XML as browsers parse `.xhtml` files: CDATA sections and
     * empty-element tags (`<div/>`) work, names are case-sensitive and nothing is implied.
     */
    public static function xhtml(string $xml): self
    {
        return new self($xml, xhtml: true);
    }

    /**
     * The directory that relative URLs (stylesheets, images, fonts) resolve against. Only files
     * inside it are read, unless {@see self::root()} allows a larger directory.
     */
    public function baseDirectory(string $directory): self
    {
        $copy = clone $this;
        $copy->base = $directory;

        return $copy;
    }

    /**
     * The directory that files may be read from; it must contain the base directory.
     */
    public function root(string $directory): self
    {
        $copy = clone $this;
        $copy->root = $directory;

        return $copy;
    }

    /**
     * Adds a stylesheet after those of the document.
     */
    public function stylesheet(string $css, string $name = 'stylesheet'): self
    {
        $copy = clone $this;
        $copy->stylesheets[] = ['name' => $name, 'css' => $css];

        return $copy;
    }

    /**
     * Hands over the data of a resource the document refers to, by its URL as written there.
     */
    public function resource(string $url, string $data): self
    {
        $copy = clone $this;
        $copy->resources[$url] = $data;

        return $copy;
    }

    /**
     * Answers requests for resources that are not handed over with {@see self::resource()}: the
     * callback gets the URL as written in the document and returns the data, or null. It is
     * asked before a file is looked for; after null, the file inside the base directory (or
     * root) is read. Fagus itself never goes to the network. It replaces a resolver set with
     * {@see self::resolveMany()}.
     *
     * @param  callable(string): ?string  $resolver
     */
    public function resolver(callable $resolver): self
    {
        $copy = clone $this;
        $copy->resolver = $resolver(...);
        $copy->manyResolver = null;

        return $copy;
    }

    /**
     * Answers requests for resources as {@see self::resolver()} does, many at once: the callback
     * gets the URLs the engine asks for together (the linked stylesheets, then the fonts, then
     * the images of a document, at most 64 at a time) and returns the data of those it has, by
     * URL. A URL it leaves out is not found. That lets it fetch them concurrently. It replaces a
     * resolver set with {@see self::resolver()}.
     *
     * @param  callable(list<string>): array<string, string>  $resolver
     */
    public function resolveMany(callable $resolver): self
    {
        $copy = clone $this;
        $copy->manyResolver = $resolver(...);
        $copy->resolver = null;

        return $copy;
    }

    /**
     * Keeps what the resolver ({@see self::resolver()}, {@see self::resolveMany()}) found in a
     * PSR-16 cache, and answers from it before the resolver is asked. An entry lives for `$ttl`
     * (seconds or an interval; null for the default of the cache) under the key `$prefix` and
     * the SHA-256 of the URL. What the resolver did not find is not kept, and neither is data
     * handed over with {@see self::resource()}. Needs `psr/simple-cache`.
     */
    public function cache(CacheInterface $cache, DateInterval|int|null $ttl = null, string $prefix = 'fagus.resource.'): self
    {
        $copy = clone $this;
        $copy->cache = $cache;
        $copy->cacheTtl = $ttl;
        $copy->cachePrefix = $prefix;

        return $copy;
    }

    /**
     * Writes the PDF as PDF/A. A level given as a string is a value of {@see Conformance}
     * ('1b', '2b', '2u', '2a', '3b', '3u', '3a', '4', '4e' or '4f', case-insensitive).
     *
     * @throws InvalidArgumentException for a string that is no level
     */
    public function pdfA(Conformance|string $level = Conformance::PdfA3b): self
    {
        if (is_string($level)) {
            $level = Conformance::tryFrom(strtolower($level)) ?? throw InvalidArgumentException::unknownConformance($level);
        }
        $copy = clone $this;
        $copy->conformance = $level;

        return $copy;
    }

    /**
     * Whether the PDF is tagged: with the logical structure of the document (headings,
     * paragraphs, lists, tables, images and links in reading order), which screen readers read.
     * On by default; without it, a PDF with many table cells is somewhat smaller and faster.
     */
    public function tagged(bool $tagged = true): self
    {
        $copy = clone $this;
        $copy->tagged = $tagged;

        return $copy;
    }

    /**
     * Writes the PDF as PDF/UA-1, the standard for accessible PDF. The document needs a
     * `<title>`, a language (`<html lang>`) and alternative text for every image (`alt`, empty
     * for decoration); without them, rendering fails with a {@see RenderException}.
     */
    public function accessible(bool $accessible = true): self
    {
        $copy = clone $this;
        $copy->accessible = $accessible;

        return $copy;
    }

    /**
     * Makes the form controls fields of an interactive form, which readers fill in in a PDF
     * viewer: text fields, text areas, checkboxes, radio buttons, drop-down lists, list boxes and
     * the buttons that submit (to an absolute `action`) or reset a form, named after their
     * `name`, with their values. Off by default: without it, controls print as they look,
     * as in Chrome. Controls in an `inert` element stay as they look.
     */
    public function forms(bool $forms = true): self
    {
        $copy = clone $this;
        $copy->forms = $forms;

        return $copy;
    }

    /**
     * Attaches a file to the PDF. Without a description, the name describes it (PDF/A-3 and
     * PDF/UA require one).
     */
    public function attach(
        string $name,
        string $data,
        string $mime = 'application/octet-stream',
        Relationship $relationship = Relationship::Unspecified,
        ?string $description = null,
    ): self {
        $copy = clone $this;
        $copy->attachments[] = compact('name', 'data', 'mime', 'relationship', 'description');

        return $copy;
    }

    /**
     * Makes the PDF an electronic invoice of ZUGFeRD or Factur-X: the XML is attached under the
     * name its profile requires and described in the XMP metadata, and the PDF conforms to
     * PDF/A-3. The profile is the one the XML declares unless it is given. `$version` is the
     * `Version` of the Factur-X XMP metadata ('1.0' by default), `$description` the description
     * of the attached XML ('Factur-X/ZUGFeRD invoice' by default).
     *
     * @throws InvalidArgumentException for an empty version or description
     */
    public function facturX(
        string $xml,
        ?InvoiceProfile $profile = null,
        ?string $version = null,
        ?string $description = null,
    ): self {
        if ($version !== null && trim($version) === '') {
            throw InvalidArgumentException::emptyInvoiceField('version');
        }
        if ($description !== null && trim($description) === '') {
            throw InvalidArgumentException::emptyInvoiceField('description');
        }
        $copy = clone $this;
        $copy->invoice = $xml;
        $copy->profile = $profile;
        $copy->invoiceVersion = $version;
        $copy->invoiceDescription = $description;

        return $copy;
    }

    /**
     * Protects the PDF with passwords, encrypted with AES-256 as PDF 2.0 defines it. The user
     * password is needed to open the PDF; without one, it opens without a password but with
     * only the permissions given. The owner password lifts those restrictions; without one, a
     * random password nobody knows is used. The permissions are what the reader may do without
     * the owner password: everything if null, nothing beyond reading if empty. A permission
     * given as a string is a value of {@see Permission} ('print', 'print-low-quality', 'modify',
     * 'copy', 'annotate', 'fill-forms', 'assemble'). Passwords are prepared with SASLprep, as
     * PDF 2.0 requires, so that they open the PDF however their characters were typed; one with
     * a character SASLprep forbids (such as a control character) fails to render. The bytes of
     * an encrypted PDF differ from run to run. PDF/A and Factur-X cannot be encrypted: rendering
     * them fails with a {@see RenderException}. An accessible PDF ({@see self::accessible()})
     * can; assistive technology may always extract its text.
     *
     * @param  list<Permission|string>|null  $permissions
     *
     * @throws InvalidArgumentException without a password, or for a string that is no permission
     */
    public function encrypt(
        #[SensitiveParameter] ?string $userPassword = null,
        #[SensitiveParameter] ?string $ownerPassword = null,
        ?array $permissions = null,
    ): self {
        if (($userPassword ?? '') === '' && ($ownerPassword ?? '') === '') {
            throw InvalidArgumentException::noPassword();
        }
        if ($permissions !== null) {
            $permissions = array_values(array_map(
                static fn (mixed $permission): Permission => match (true) {
                    $permission instanceof Permission => $permission,
                    is_string($permission) => Permission::tryFrom(strtolower($permission)) ?? throw InvalidArgumentException::unknownPermission($permission),
                    default => throw InvalidArgumentException::unknownPermission($permission),
                },
                $permissions,
            ));
        }
        $copy = clone $this;
        $copy->encryption = compact('userPassword', 'ownerPassword', 'permissions');

        return $copy;
    }

    /**
     * The creation date written into the PDF (now by default). A fixed date makes the output
     * reproducible.
     */
    public function created(DateTimeInterface $date): self
    {
        $copy = clone $this;
        $copy->created = $date;

        return $copy;
    }

    /**
     * Signs the PDF digitally (PAdES). An encrypted PDF (see {@see self::encrypt()}) is signed
     * with its file key: the signature's update is encrypted as the rest of the file is.
     */
    public function sign(Signature $signature): self
    {
        $copy = clone $this;
        $copy->signature = $signature;

        return $copy;
    }

    /**
     * Fails with a {@see RenderException} when the document has diagnostics (CSS that is
     * ignored, resources that are missing).
     */
    public function strict(bool $strict = true): self
    {
        $copy = clone $this;
        $copy->strict = $strict;

        return $copy;
    }

    /**
     * Reports where the elements with an `id` ended up, in {@see Pdf::layout()}: their pages
     * and boxes, and the rows of their tables. Off by default, as it takes a little time.
     */
    public function reportLayout(bool $reportLayout = true): self
    {
        $copy = clone $this;
        $copy->reportLayout = $reportLayout;

        return $copy;
    }

    public function render(?Engine $engine = null): Pdf
    {
        $pdf = ($engine ?? Fagus::engine())->render($this);
        if ($this->strict && $pdf->diagnostics() !== []) {
            throw RenderException::strict($pdf->diagnostics());
        }

        return $pdf;
    }

    public function save(string $path, ?Engine $engine = null): Pdf
    {
        $pdf = $this->render($engine);
        $pdf->save($path);

        return $pdf;
    }

    /**
     * Renders the document and sends the PDF to the browser to show it, as {@see Pdf::inline()}.
     */
    public function inline(string $filename = 'document.pdf', ?Engine $engine = null): Pdf
    {
        $pdf = $this->render($engine);
        $pdf->inline($filename);

        return $pdf;
    }

    /**
     * Renders the document and sends the PDF to the browser as a download, as
     * {@see Pdf::download()}.
     */
    public function download(string $filename = 'document.pdf', ?Engine $engine = null): Pdf
    {
        $pdf = $this->render($engine);
        $pdf->download($filename);

        return $pdf;
    }

    /**
     * The render request of the worker protocol: its header and data blocks.
     *
     * @return array{0: array<string, mixed>, 1: list<string>}
     *
     * @internal
     */
    public function request(int $id): array
    {
        $blobs = [$this->html];
        $add = static function (string $data) use (&$blobs): int {
            $blobs[] = $data;

            return count($blobs) - 1;
        };
        $header = ['id' => $id, 'html' => 0, 'ask' => $this->resolver !== null || $this->manyResolver !== null];
        if ($header['ask']) {
            // The engine may ask for many resources at once (`Q`); a binary older than that
            // asks for one after the other.
            $header['askMany'] = true;
        }
        if ($this->xhtml) {
            $header['xhtml'] = true;
        }
        if ($this->base !== null) {
            $header['base'] = $this->base;
        }
        if ($this->root !== null) {
            $header['root'] = $this->root;
        }
        foreach ($this->stylesheets as $stylesheet) {
            $header['stylesheets'][] = ['name' => $stylesheet['name'], 'blob' => $add($stylesheet['css'])];
        }
        foreach ($this->resources as $url => $data) {
            $header['resources'][] = ['url' => (string) $url, 'blob' => $add($data)];
        }
        if ($this->conformance !== null) {
            $header['conformance'] = $this->conformance->value;
        }
        $header['tagged'] = $this->tagged;
        if ($this->accessible) {
            $header['accessible'] = true;
        }
        if ($this->forms) {
            $header['forms'] = true;
        }
        foreach ($this->attachments as $attachment) {
            $header['attachments'][] = [
                'name' => $attachment['name'],
                'mime' => $attachment['mime'],
                'relationship' => $attachment['relationship']->value,
                'description' => $attachment['description'],
                'blob' => $add($attachment['data']),
            ];
        }
        if ($this->invoice !== null) {
            $header['facturX'] = ['blob' => $add($this->invoice)];
            if ($this->profile !== null) {
                $header['facturX']['profile'] = $this->profile->value;
            }
            if ($this->invoiceVersion !== null) {
                $header['facturX']['version'] = $this->invoiceVersion;
            }
            if ($this->invoiceDescription !== null) {
                $header['facturX']['description'] = $this->invoiceDescription;
            }
        }
        if ($this->reportLayout) {
            $header['layout'] = true;
        }
        if ($this->encryption !== null) {
            $header['encryption'] = array_filter([
                'userPassword' => $this->encryption['userPassword'],
                'ownerPassword' => $this->encryption['ownerPassword'],
                'permissions' => $this->encryption['permissions'] === null ? null : array_map(
                    static fn (Permission $permission): string => $permission->value,
                    $this->encryption['permissions'],
                ),
            ], static fn (mixed $value): bool => $value !== null);
        }
        if ($this->created !== null) {
            $header['created'] = DateTimeImmutable::createFromInterface($this->created)
                ->setTimezone(new DateTimeZone('UTC'))
                ->format('Y-m-d\TH:i:s\Z');
        }
        if ($this->signature !== null) {
            $header['signature'] = $this->signature->header($add);
        }

        return [$header, $blobs];
    }

    /**
     * The data of a resource the engine asks for.
     *
     * @internal
     */
    public function resolve(string $url): ?string
    {
        return $this->resolveAll([$url])[0];
    }

    /**
     * The data of the resources the engine asks for at once, in the order of the URLs; null for
     * one that is not found.
     *
     * @param  list<string>  $urls
     * @return list<?string>
     *
     * @internal
     */
    public function resolveAll(array $urls): array
    {
        if ($this->resolver === null && $this->manyResolver === null) {
            return array_fill(0, count($urls), null);
        }
        // From the cache first, then the resolver for the rest, which is kept for the next time.
        $found = [];
        $keys = [];
        if ($this->cache !== null && $urls !== []) {
            foreach ($urls as $url) {
                $keys[$this->cachePrefix.hash('sha256', $url)] = $url;
            }
            foreach ($this->cache->getMultiple(array_map(strval(...), array_keys($keys))) as $key => $data) {
                if (is_string($data) && isset($keys[$key])) {
                    $found[$keys[$key]] = $data;
                }
            }
        }
        $missing = array_values(array_filter($urls, static fn (string $url): bool => ! isset($found[$url])));
        $fresh = $this->ask($missing);
        if ($this->cache !== null && $fresh !== []) {
            $entries = [];
            foreach ($fresh as $url => $data) {
                $entries[$this->cachePrefix.hash('sha256', (string) $url)] = $data;
            }
            $this->cache->setMultiple($entries, $this->cacheTtl);
        }
        $found += $fresh;

        return array_map(static fn (string $url): ?string => $found[$url] ?? null, $urls);
    }

    /**
     * What the resolver has of `$urls`, by URL.
     *
     * @param  list<string>  $urls
     * @return array<string, string>
     */
    private function ask(array $urls): array
    {
        $found = [];
        if ($urls === []) {
            return $found;
        }
        if ($this->manyResolver !== null) {
            $answers = ($this->manyResolver)($urls);
            if (! is_array($answers)) {
                throw new TypeError('The callback of resolveMany() must return an array of data by URL, '.get_debug_type($answers).' returned.');
            }
            foreach ($urls as $url) {
                if (! isset($answers[$url])) {
                    continue;
                }
                if (! is_string($answers[$url])) {
                    throw new TypeError("The callback of resolveMany() must return the data of `{$url}` as a string, ".get_debug_type($answers[$url]).' returned.');
                }
                $found[$url] = $answers[$url];
            }
        } elseif ($this->resolver !== null) {
            foreach ($urls as $url) {
                if (! isset($found[$url]) && ($data = self::data(($this->resolver)($url))) !== null) {
                    $found[$url] = $data;
                }
            }
        }

        return $found;
    }

    /**
     * What a resolver answered: data or null.
     */
    private static function data(?string $data): ?string
    {
        return $data;
    }

    /**
     * The signature, for the engine's questions while it signs.
     *
     * @internal
     */
    public function signature(): ?Signature
    {
        return $this->signature;
    }
}
