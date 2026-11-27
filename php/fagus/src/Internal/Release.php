<?php

declare(strict_types=1);

namespace Fagus\Internal;

/**
 * The release of the fagus binary that this package works with. `tools/pin-binaries.php` writes
 * this file when the binaries of a release are built.
 *
 * @internal
 */
final class Release
{
    /** The version of the binary. */
    public const string VERSION = '1.2.0';

    /** The GitHub repository whose releases hold the binaries. */
    public const string REPOSITORY = 'nforst/fagus';

    /**
     * The SHA-256 of the binary for each target. Empty in a development version, which has no
     * built binaries.
     *
     * @var array<string, string>
     */
    public const array BINARIES = [
        'aarch64-apple-darwin' => '0e08571b4f3db5fc5bb13109ec8c22fe0c4e721f3a6b8f4a7bdd5b82aab24bdb',
        'aarch64-unknown-linux-gnu' => '64877d1fb1fbe9b548a0ee51140fb1558be7066851634977ba0a9eea60d07916',
        'aarch64-unknown-linux-musl' => '5698a987536cde82e608f0ebbd86ce191767c48002ca8b3afacbc3c74fab164f',
        'x86_64-apple-darwin' => '3da81a18c273e13b3cd0b7ae04b39bcc5e1b023eb668ca9611c63c468a347bd6',
        'x86_64-pc-windows-msvc' => '1dcf9a048329e262bd6cfdda33e40e9d561924c9e68097a4ca35f0e4576c5f62',
        'x86_64-unknown-freebsd' => '559910af066baffef0723c9ed0974ac392730f5a8e6c3f82a92a7843cce195fd',
        'x86_64-unknown-linux-gnu' => 'e9d628f41dd3ef32a22e094e61c78668cdcbe8c18051627ed5639a2a010168ff',
        'x86_64-unknown-linux-musl' => '5293e68a4f6ed39c2ff48c363ed3f5478d0d8de719ef3f3483c8fa848b6dbd10',
    ];
}
