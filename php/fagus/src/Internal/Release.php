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
    public const string VERSION = '1.0.0';

    /** The GitHub repository whose releases hold the binaries. */
    public const string REPOSITORY = 'nforst/fagus';

    /**
     * The SHA-256 of the binary for each target. Empty in a development version, which has no
     * built binaries.
     *
     * @var array<string, string>
     */
    public const array BINARIES = [
        'aarch64-apple-darwin' => '66cfddbc9c8487f70cc490edc7acef0ac3b3d818a26ee0b3a7331a3d0dfe81d1',
        'aarch64-unknown-linux-gnu' => '6b4714022aac7f9f5be83a8b82b550a3638649bb0130e5efdea7746e44a4f85f',
        'aarch64-unknown-linux-musl' => '16cab327b4b66cb572e94d621bc17e5c5e7238da2eecbcc37adedcf2c0e0a5c2',
        'x86_64-apple-darwin' => '8874d3970d4d360ff57a84c99d037ae4c7cbf9cd6d4913186dfd09e8a72b2891',
        'x86_64-pc-windows-msvc' => '7f6f2b2542d5ad158181e52bee39bd3346f6c9789163de83bcae8e8f571a20ac',
        'x86_64-unknown-freebsd' => '2db00da8409c8a53f2668056defb4d3ad76b545c73b054ed40d9d0c6562d0190',
        'x86_64-unknown-linux-gnu' => '36e5ed9bee4534a79be4e467becb2acdf74f0aff216a8a11ccfe5f55592de2d9',
        'x86_64-unknown-linux-musl' => 'c583150da5ed0d3d3ba5557d5f947b9f67d9ad46f6378556c485430c69769572',
    ];
}
