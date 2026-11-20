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
    public const string VERSION = '1.1.0';

    /** The GitHub repository whose releases hold the binaries. */
    public const string REPOSITORY = 'nforst/fagus';

    /**
     * The SHA-256 of the binary for each target. Empty in a development version, which has no
     * built binaries.
     *
     * @var array<string, string>
     */
    public const array BINARIES = [
        'aarch64-apple-darwin' => '50521a1efd6d5a01b9d1cdf52988495c1543f0a49f3469fc047c903ba65d6190',
        'aarch64-unknown-linux-gnu' => 'eb9b83b80c54b0aacbc17e983d8faa3611430dc768fee7a1fa133312164a5785',
        'aarch64-unknown-linux-musl' => '492d5bce1af2c0cffca4f36c596fbbea1f9844b79f62019307bfa8837a084dab',
        'x86_64-apple-darwin' => '9536730c2c588a93f862e1eb21393e663f68629376e456ab25400d413ba19319',
        'x86_64-pc-windows-msvc' => 'b88282447bb520ed38e9eea5618824b84b91a29a58d856b9a06bff0b4d739dd5',
        'x86_64-unknown-freebsd' => 'c08240399eb57933b27b6d411812647524ff0bb06cf466342bdb49e583300b68',
        'x86_64-unknown-linux-gnu' => '390858a05e9e67890fd48775bd5ac08c04fd8e9037ce25b61bd9cee6e6e96b96',
        'x86_64-unknown-linux-musl' => '5dee322bb568817f899c3a4eadb0da6d3e60c421bd03c5dc40a230210cd21a17',
    ];
}
