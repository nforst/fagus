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
    public const string VERSION = '1.1.1';

    /** The GitHub repository whose releases hold the binaries. */
    public const string REPOSITORY = 'nforst/fagus';

    /**
     * The SHA-256 of the binary for each target. Empty in a development version, which has no
     * built binaries.
     *
     * @var array<string, string>
     */
    public const array BINARIES = [
        'aarch64-apple-darwin' => '5195beeefa11f669ab906ccf257a0d5b57bc9d028c4fb95879f6db3d042532ae',
        'aarch64-unknown-linux-gnu' => 'dfab848957511d07be8f75c78c44fd2c16e012e604eaf9c33fd9f1ab30077f3b',
        'aarch64-unknown-linux-musl' => 'c9a817a688cf0637059e3e4b189523d4599860d058c5b757e46dc194e45a4d2e',
        'x86_64-apple-darwin' => 'ecc58e6416d9882d465f667c3cbe9ff63f83fce44d75d6b6996190ea0e7ee8bf',
        'x86_64-pc-windows-msvc' => '9774cdf69fb2e56d8779e937ba15bafe42d1489cf3e77098cbe6116093bce396',
        'x86_64-unknown-freebsd' => 'fca4fe1515e1c09f79575d2d8fcdb5a350125aae54db407f3c33eb1156cbec8b',
        'x86_64-unknown-linux-gnu' => 'fa101210047cd05d46c80f37c936499ad3ed2b89abd629ec91263022f6831712',
        'x86_64-unknown-linux-musl' => '2aa2188179be6d78c89f0e4f873d147bbb4dc377bc589e8f4faf84786ef6aed9',
    ];
}
