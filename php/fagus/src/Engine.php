<?php

declare(strict_types=1);

namespace Fagus;

use Closure;
use Fagus\Exception\EngineException;
use Fagus\Exception\InvalidArgumentException;
use Fagus\Exception\RenderException;
use Fagus\Internal\Channel;
use Fagus\Internal\Release;
use Throwable;

/**
 * The fagus binary, running as a worker process. The process starts with the first document and
 * keeps running, with its fonts and caches warm, until the engine is closed or destroyed: in a
 * queue worker or under Octane it serves every document, under PHP-FPM those of one request.
 */
final class Engine
{
    public const PROTOCOL = 1;

    /** The first release of the binary that merges PDFs. */
    public const MERGE_SINCE = '1.2.0';

    /** Seconds a worker gets to end after its input is closed, before it is terminated. */
    private const GRACE = 2.0;

    /** The most of the worker's error output that goes into a message, in bytes. */
    private const MAX_OUTPUT = 4096;

    /** @var resource|null */
    private $process = null;

    /** @var array<int, resource> */
    private array $pipes = [];

    /**
     * A temporary file with the worker's error output. A file rather than a pipe: a pipe that
     * nobody reads fills up and blocks the worker, and PHP cannot read pipes without blocking
     * on Windows.
     *
     * @var resource|null
     */
    private $errors = null;

    /** Where the error output not yet reported begins in {@see self::$errors}. */
    private int $errorsRead = 0;

    private ?Channel $channel = null;

    private ?string $version = null;

    /** The path of the running binary. */
    private ?string $located = null;

    /** How much of its sandbox the running worker is in, as its hello says. */
    private string $sandboxed = 'off';

    private int $requests = 0;

    /** The documents the running worker has rendered. */
    private int $documents = 0;

    /** @var list<string> the fonts added with {@see self::addFont()}, for a new worker */
    private array $fonts = [];

    /**
     * The binaries of another release this process has warned about, by path and version, so
     * that each is reported once rather than with every request.
     *
     * @var array<string, true>
     */
    private static array $warned = [];

    /**
     * @param  ?string  $binary  the path of the fagus binary; found through FAGUS_BINARY, next to this package or on the PATH if null
     * @param  list<string>  $fontDirectories  directories with fonts (TTF, OTF, TTC, OTC, WOFF, WOFF2), searched
     *                                         with their subdirectories; each must exist
     * @param  ?string  $sansSerif  the family of `sans-serif`
     * @param  ?string  $serif  the family of `serif`
     * @param  ?string  $monospace  the family of `monospace`
     * @param  float  $timeout  seconds to wait for each answer of the worker before it is stopped
     *                          (not on Windows, where PHP cannot wait for a pipe with a time limit)
     * @param  bool  $systemFonts  whether the fonts installed on the system serve too, as in Chrome
     *                             (the output then depends on the machine)
     * @param  ?string  $locale  the language of the texts form controls show of their own (button labels,
     *                           date formats) where documents give no `lang`, e.g. "de-DE"; en-US if null
     * @param  ?int  $maxDocuments  the documents a worker renders before it is replaced by a new one; no limit if null
     * @param  ?int  $maxMemory  megabytes of resident memory above which a worker is replaced after a document,
     *                           as far as the system tells the worker (Linux: now, macOS and FreeBSD: at its peak; not Windows)
     * @param  ?Closure(string): void  $onWarning  receives warnings, such as a binary of another release than
     *                                           this package; written to PHP's error log (error_log()) if null
     * @param  bool  $sandbox  whether the worker renders in its sandbox on Linux (docs/cli.md#sandbox)
     * @param  ?int  $memoryBudget  megabytes a document may take, all its parts together (an estimate, see
     *                              docs/php.md#limits); one that needs more fails with `limit.memory`.
     *                              2048 if null, none if 0
     * @param  ?float  $timeLimit  seconds a document may take in all; one that takes longer fails with
     *                             `limit.time` and the worker goes on. A tenth less than $timeout if null,
     *                             so that the worker ends the document before the engine stops it; none if 0
     */
    public function __construct(
        private readonly ?string $binary = null,
        private readonly array $fontDirectories = [],
        private readonly ?string $sansSerif = null,
        private readonly ?string $serif = null,
        private readonly ?string $monospace = null,
        private readonly float $timeout = 60.0,
        private readonly bool $systemFonts = false,
        private readonly ?string $locale = null,
        private readonly ?int $maxDocuments = null,
        private readonly ?int $maxMemory = null,
        private readonly ?Closure $onWarning = null,
        private readonly bool $sandbox = true,
        private readonly ?int $memoryBudget = null,
        private readonly ?float $timeLimit = null,
    ) {
        foreach (['maxDocuments' => $maxDocuments, 'maxMemory' => $maxMemory] as $name => $limit) {
            if ($limit !== null && $limit < 1) {
                throw InvalidArgumentException::notPositive($name, $limit);
            }
        }
        foreach (['memoryBudget' => $memoryBudget, 'timeLimit' => $timeLimit] as $name => $limit) {
            if ($limit !== null && $limit < 0) {
                throw InvalidArgumentException::negative($name, $limit);
            }
        }
    }

    public function __destruct()
    {
        $this->close();
    }

    public function render(Document $document): Pdf
    {
        $channel = $this->start();
        [$header, $blobs] = $document->request(++$this->requests);
        $header += $this->limits();
        try {
            $channel->write('R', $header, $blobs);
            while (true) {
                [$kind, $reply, $data] = $channel->read();
                switch ($kind) {
                    case '?':
                        $resource = $document->resolve((string) ($reply['url'] ?? ''));
                        $channel->write('r', ['found' => $resource !== null], $resource === null ? [] : [$resource]);
                        break;
                    case 'Q':
                        $urls = array_values(array_filter((array) ($reply['urls'] ?? []), is_string(...)));
                        $resources = $document->resolveAll($urls);
                        $channel->write(
                            'q',
                            ['found' => array_map(static fn (?string $resource): bool => $resource !== null, $resources)],
                            array_values(array_filter($resources, static fn (?string $resource): bool => $resource !== null)),
                        );
                        break;
                    case 'S':
                        $signature = $document->signature() ?? throw EngineException::unexpected($kind);
                        $channel->write('s', [], [$signature->signDigest($data[0] ?? '', (string) ($reply['algorithm'] ?? ''))]);
                        break;
                    case 'T':
                        $signature = $document->signature() ?? throw EngineException::unexpected($kind);
                        $channel->write('t', [], [$signature->timestampDigest($data[0] ?? '')]);
                        break;
                    case 'V':
                        $signature = $document->signature() ?? throw EngineException::unexpected($kind);
                        $channel->write('v', [], $signature->revocationData($reply, $data));
                        break;
                    case 'P':
                        // A binary older than encryption ignores the passwords: never hand
                        // out such a PDF as protected.
                        if (isset($header['encryption']) && ($reply['encrypted'] ?? false) !== true) {
                            throw EngineException::withoutEncryption();
                        }
                        // Nor one without the signature as signed.
                        if (isset($header['signature']) && ($reply['signed'] ?? false) !== true) {
                            throw EngineException::withoutSignature();
                        }
                        // Nor one with the controls printed as fields.
                        if (($header['forms'] ?? false) === true && ($reply['forms'] ?? false) !== true) {
                            throw EngineException::withoutForms();
                        }
                        $this->rendered($reply);

                        return Pdf::fromReply($reply, $data[0] ?? '');
                    case 'E':
                        $this->rendered($reply);

                        throw new RenderException(
                            (string) ($reply['message'] ?? 'The document could not be rendered.'),
                            array_values(array_map(Diagnostic::fromArray(...), $reply['diagnostics'] ?? [])),
                        );
                    default:
                        throw EngineException::unexpected($kind);
                }
            }
        } catch (RenderException $exception) {
            throw $exception;
        } catch (Throwable $exception) {
            // Whatever happened in between, the worker may still be rendering or wait for an
            // answer: it is stopped, and the next document starts a new one.
            $exception = $exception instanceof EngineException ? $this->withOutput($exception) : $exception;
            $this->stop();

            throw $exception;
        }
    }

    /**
     * Merges PDFs into one, in the worker as documents render.
     *
     * @throws EngineException when the binary is older than merging
     * @throws RenderException when a PDF cannot be merged (it cannot be read, is encrypted, or
     *                         lacks a page to take), or the merge exceeds its limits
     */
    public function merge(Merge $merge): Pdf
    {
        $channel = $this->start();
        // A binary older than merging answers the request as a message it does not know; the
        // binary of this package's release merges, also before its version says 1.2.0.
        if (version_compare((string) $this->version, self::MERGE_SINCE, '<') && $this->version !== Release::VERSION) {
            throw EngineException::cannotMerge($this->located, $this->version);
        }
        [$header, $blobs] = $merge->request(++$this->requests);
        $header += $this->limits();
        try {
            $channel->write('M', $header, $blobs);
            [$kind, $reply, $data] = $channel->read();
        } catch (Throwable $exception) {
            // The worker may still be merging: it is stopped, and the next request starts a
            // new one.
            $exception = $exception instanceof EngineException ? $this->withOutput($exception) : $exception;
            $this->stop();

            throw $exception;
        }
        if ($kind === 'P') {
            $this->rendered($reply);

            return Pdf::fromReply($reply, $data[0] ?? '');
        }
        // A worker that does not know the request answers without its `id`, and goes on.
        if ($kind === 'E' && ! isset($reply['id'])) {
            throw EngineException::cannotMerge($this->located, $this->version);
        }
        if ($kind === 'E') {
            $this->rendered($reply);

            throw new RenderException(
                (string) ($reply['message'] ?? 'The PDFs could not be merged.'),
                array_values(array_map(Diagnostic::fromArray(...), $reply['diagnostics'] ?? [])),
            );
        }
        $exception = $this->withOutput(EngineException::unexpected($kind));
        $this->stop();

        throw $exception;
    }

    /**
     * The memory budget (bytes) and time limit (milliseconds) of a render request, 0 for none.
     *
     * @return array{memoryBudget?: int, timeLimit: float}
     */
    private function limits(): array
    {
        $limits = ['timeLimit' => round(($this->timeLimit ?? $this->timeout * 0.9) * 1000, 3)];
        if ($this->memoryBudget !== null) {
            $limits['memoryBudget'] = $this->memoryBudget * 1024 * 1024;
        }

        return $limits;
    }

    /**
     * Counts a document the worker has answered, and replaces the worker by a new one when it
     * has rendered as many documents as it may or has grown beyond the memory it may have: it is
     * closed, and the next document starts a new one.
     *
     * @param  array<string, mixed>  $reply
     */
    private function rendered(array $reply): void
    {
        $this->documents++;
        // The resident size now where the worker knows it (Linux), else the peak (macOS, FreeBSD).
        $resident = $reply['rss'] ?? $reply['maxRss'] ?? null;
        if (
            ($this->maxDocuments !== null && $this->documents >= $this->maxDocuments)
            || ($this->maxMemory !== null && is_int($resident) && $resident > $this->maxMemory * 1024 * 1024)
        ) {
            $this->close();
        }
    }

    /**
     * Adds a font for all later documents. Returns the number of faces in it. The engine keeps
     * the font and hands it to the worker again whenever a new one starts.
     */
    public function addFont(string $data): int
    {
        $faces = $this->sendFont($this->start(), $data);
        $this->fonts[] = $data;

        return $faces;
    }

    /**
     * The version of the running binary.
     */
    public function version(): string
    {
        $this->start();

        return (string) $this->version;
    }

    /**
     * How much of its sandbox the running worker is in: "on", "partial" (the kernel lacks Landlock
     * or seccomp) or "off" (turned off, or not Linux).
     */
    public function sandbox(): string
    {
        $this->start();

        return $this->sandboxed;
    }

    /**
     * Stops the worker process; the next document starts a new one. The worker ends when its
     * input closes; one that has not ended after two seconds is terminated.
     */
    public function close(): void
    {
        $this->shutDown(self::GRACE);
    }

    /**
     * Terminates the worker process at once, for a worker that may be busy or stuck.
     */
    private function stop(): void
    {
        $this->shutDown(0.0);
    }

    private function shutDown(float $grace): void
    {
        foreach ($this->pipes as $pipe) {
            if (is_resource($pipe)) {
                fclose($pipe);
            }
        }
        if (is_resource($this->process)) {
            $deadline = microtime(true) + $grace;
            while (proc_get_status($this->process)['running'] && microtime(true) < $deadline) {
                usleep(1000);
            }
            if (proc_get_status($this->process)['running']) {
                proc_terminate($this->process, 9);
            }
            // Waits for the process to end, so that it does not linger as a zombie.
            proc_close($this->process);
        }
        if (is_resource($this->errors)) {
            fclose($this->errors);
        }
        $this->process = null;
        $this->pipes = [];
        $this->errors = null;
        $this->channel = null;
    }

    private function start(): Channel
    {
        if ($this->channel !== null && is_resource($this->process) && proc_get_status($this->process)['running']) {
            return $this->channel;
        }
        $this->stop();
        // The worker would end at once, with a message that is hard to read.
        foreach ($this->fontDirectories as $directory) {
            if (! is_dir($directory)) {
                throw EngineException::fontDirectoryNotFound($directory);
            }
        }
        $binary = Binary::locate($this->binary);
        $command = [$binary, 'worker'];
        foreach ($this->fontDirectories as $directory) {
            array_push($command, '--font-dir', $directory);
        }
        if ($this->systemFonts) {
            $command[] = '--system-fonts';
        }
        if ($this->locale !== null && $this->locale !== '') {
            array_push($command, '--locale', $this->locale);
        }
        foreach (['--sans-serif' => $this->sansSerif, '--serif' => $this->serif, '--monospace' => $this->monospace] as $option => $family) {
            if ($family !== null) {
                array_push($command, $option, $family);
            }
        }
        if (! $this->sandbox) {
            $command[] = '--no-sandbox';
        }
        if (! function_exists('proc_open')) {
            throw EngineException::notStarted($binary, 'PHP does not allow proc_open() (disable_functions).');
        }
        $errors = tmpfile();
        if ($errors === false) {
            throw EngineException::notStarted($binary, 'There is no temporary file for its error output.');
        }
        // The array form starts the binary without a shell and quotes the arguments for the
        // platform (paths with spaces, on Windows too). The pipes are binary, so that Windows
        // does not translate line ends in PDFs and fonts.
        $process = @proc_open($command, [0 => ['pipe', 'rb'], 1 => ['pipe', 'wb'], 2 => $errors], $pipes);
        if (! is_resource($process)) {
            fclose($errors);

            throw EngineException::notStarted($binary, error_get_last()['message'] ?? '');
        }
        $this->process = $process;
        $this->pipes = $pipes;
        $this->errors = $errors;
        $this->errorsRead = 0;
        $this->documents = 0;
        $channel = new Channel($pipes[1], $pipes[0], $this->timeout);
        try {
            [$kind, $hello] = $channel->read();
            $version = $kind === 'H' && is_string($hello['version'] ?? null) ? $hello['version'] : null;
            if ($kind !== 'H' || ($hello['protocol'] ?? null) !== self::PROTOCOL) {
                throw EngineException::incompatible($kind === 'H' ? ($hello['protocol'] ?? null) : null, $binary, $version);
            }
            foreach ($this->fonts as $font) {
                $this->sendFont($channel, $font);
            }
        } catch (EngineException $exception) {
            $exception = $this->withOutput($exception);
            $this->stop();

            throw $exception;
        }
        $this->version = (string) $version;
        $this->located = $binary;
        if ($this->version !== Release::VERSION) {
            $this->warnOnce($binary, $this->version);
        }
        $this->sandboxed = (string) ($hello['sandbox'] ?? 'off');

        return $this->channel = $channel;
    }

    /**
     * Warns that the binary is of another release than this package, once per binary and
     * version in this process: under PHP-FPM every request starts a worker of its own.
     */
    private function warnOnce(string $binary, string $version): void
    {
        $key = $binary."\0".$version;
        if (isset(self::$warned[$key])) {
            return;
        }
        self::$warned[$key] = true;
        $message = EngineException::otherRelease($binary, $version === '' ? 'unknown' : $version);
        if ($this->onWarning !== null) {
            ($this->onWarning)($message);
        } else {
            error_log($message);
        }
    }

    /**
     * Sends a font to the worker and returns the number of faces it found.
     */
    private function sendFont(Channel $channel, string $data): int
    {
        try {
            $channel->write('F', [], [$data]);
            [$kind, $reply] = $channel->read();
        } catch (EngineException $exception) {
            $exception = $this->withOutput($exception);
            $this->stop();

            throw $exception;
        }
        if ($kind === 'E') {
            throw new EngineException((string) ($reply['message'] ?? 'The font could not be read.'));
        }

        return (int) ($reply['faces'] ?? 0);
    }

    /**
     * The exception with the worker's exit code, if it has ended, and what it wrote to its error
     * output since the last exception.
     */
    private function withOutput(EngineException $exception): EngineException
    {
        $details = [];
        if (is_resource($this->process)) {
            // A worker that closed its output is about to end.
            $deadline = microtime(true) + 0.1;
            while (($status = proc_get_status($this->process))['running'] && microtime(true) < $deadline) {
                usleep(1000);
            }
            if (! $status['running'] && $status['exitcode'] >= 0) {
                $details[] = "It exited with code {$status['exitcode']}.";
            }
        }
        if (is_resource($this->errors)) {
            $stat = fstat($this->errors);
            $end = $stat === false ? 0 : $stat['size'];
            $start = max($this->errorsRead, $end - self::MAX_OUTPUT);
            if ($end > $start && fseek($this->errors, $start) === 0) {
                $output = trim((string) fread($this->errors, $end - $start));
                if ($output !== '') {
                    $details[] = $output;
                }
            }
            // The worker shares the file position: it goes on writing at the end.
            fseek($this->errors, 0, SEEK_END);
            $this->errorsRead = $end;
        }

        return $details === [] ? $exception : new EngineException($exception->getMessage().' '.implode(' ', $details), 0, $exception);
    }
}
