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
    public const string VERSION = '1.2.1';

    /** The GitHub repository whose releases hold the binaries. */
    public const string REPOSITORY = 'nforst/fagus';

    /**
     * The SHA-256 of the binary for each target. Empty in a development version, which has no
     * built binaries.
     *
     * @var array<string, string>
     */
    public const array BINARIES = [
        'aarch64-apple-darwin' => '4069f2dadfeef39abf9443b7c40826a993c574154e0ea8eae8a74a9345200978',
        'aarch64-unknown-linux-gnu' => '98d399d005de9400107e1d3fad2474dfabc4a5c33367f1de62ad2eb820c3fa5a',
        'aarch64-unknown-linux-musl' => 'd40f3e59f8bc49ded643cdee7a83b5156e1a8065a3121f8c52bfd144a9e76551',
        'x86_64-apple-darwin' => '8ec2b281099924547551bc1954cb83bf2e42012d8ab1f0fcf593b93155c95acc',
        'x86_64-pc-windows-msvc' => '9d7f47e956fed2bc4fc54379ee421b854eb2b32977661fdf3fb6427ad925a371',
        'x86_64-unknown-freebsd' => 'f40f5313073a98da988e7c1af47e7fdec79ba8823c62b59d3b8a61f4e2ae3d1a',
        'x86_64-unknown-linux-gnu' => '3ab8aaffd385f4a492cc196e97171aab6bcdec0d9ad3a546b50b6de20981f6d4',
        'x86_64-unknown-linux-musl' => '0b7c5484dfd35a0dbb21c0da29dfe47b5b1ffd1e65768a4f287585dff8a6b619',
    ];
}
