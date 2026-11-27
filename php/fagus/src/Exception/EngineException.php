<?php

declare(strict_types=1);

namespace Fagus\Exception;

use Fagus\Engine;
use Fagus\Internal\Release;

/**
 * The engine could not be found, started or talked to.
 */
final class EngineException extends FagusException
{
    public static function binaryNotFound(): self
    {
        return new self('The fagus binary was not found. Install it with `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`), set its path in the Engine or in the FAGUS_BINARY environment variable, or put it on the PATH.');
    }

    public static function notStarted(string $binary, string $reason = ''): self
    {
        return new self(trim("The fagus binary `{$binary}` could not be started. {$reason}"));
    }

    public static function fontDirectoryNotFound(string $directory): self
    {
        return new self("The font directory `{$directory}` does not exist. Create it or remove it from the engine's font directories.");
    }

    public static function stopped(string $output = ''): self
    {
        return new self(trim('The fagus worker stopped. '.$output));
    }

    public static function timedOut(float $seconds): self
    {
        return new self("The fagus worker did not answer within {$seconds} seconds and was stopped.");
    }

    /**
     * @param  mixed  $protocol  the protocol the binary named in its greeting, null if it named none
     * @param  ?string  $binary  the path of the binary
     * @param  ?string  $version  the version of the binary, if it named one
     */
    public static function incompatible(mixed $protocol, ?string $binary = null, ?string $version = null): self
    {
        $which = $binary === null ? 'The fagus binary' : "The fagus binary `{$binary}`";
        $which .= $version === null || $version === '' ? '' : " (version {$version})";
        $speaks = $protocol === null ? 'does not greet as a fagus worker' : 'speaks protocol '.json_encode($protocol);

        return new self("{$which} {$speaks}; this package, nforst/fagus ".Release::VERSION.', needs protocol '.Engine::PROTOCOL.'. '.self::installHint());
    }

    /**
     * The message for a binary of another release than this package, which speaks its protocol.
     *
     * @internal
     */
    public static function otherRelease(string $binary, string $version): string
    {
        return "The fagus binary `{$binary}` is version {$version}, this package nforst/fagus ".Release::VERSION.'. '
            .'Both speak protocol '.Engine::PROTOCOL.', so documents render, but a binary of another release ignores options '
            .'newer than itself and may lay out differently. '.self::installHint();
    }

    private static function installHint(): string
    {
        return 'Install the binary of the package\'s release with `vendor/bin/fagus-install` (in Laravel: `php artisan fagus:install`), '
            .'or point FAGUS_BINARY at the binary of version '.Release::VERSION.'.';
    }

    public static function withoutEncryption(): self
    {
        return new self('The fagus binary cannot encrypt PDFs and wrote the PDF without a password. Install the binary of the same release.');
    }

    public static function withoutSignature(): self
    {
        return new self('The fagus binary cannot sign PDFs and wrote the PDF without a signature. Install the binary of the same release.');
    }

    public static function withoutForms(): self
    {
        return new self('The fagus binary cannot make form fields and wrote the controls as they look. Install the binary of the same release.');
    }

    /**
     * The message for a binary older than merging, which answers the merge request as a message
     * it does not know.
     *
     * @param  ?string  $binary  the path of the binary
     * @param  ?string  $version  the version of the binary, if it named one
     */
    public static function cannotMerge(?string $binary, ?string $version): self
    {
        $which = $binary === null ? 'The fagus binary' : "The fagus binary `{$binary}`";
        $which .= $version === null || $version === '' ? '' : " (version {$version})";

        return new self("{$which} cannot merge PDFs, which needs version ".Engine::MERGE_SINCE.' or later. '.self::installHint());
    }

    public static function unexpected(string $kind): self
    {
        return new self("The fagus worker sent an unexpected message `{$kind}`.");
    }
}
