<?php

declare(strict_types=1);

return [

    /*
    | The fagus binary (FAGUS_BINARY). Without a path, it is the one the package installed (with
    | Composer or `php artisan fagus:install`), else the one on the PATH.
    */
    'binary' => env('FAGUS_BINARY'),

    /*
    | Directories with fonts for every document: TTF, OTF, TTC, OTC, WOFF and WOFF2 files, also
    | in subdirectories. Directories that do not exist are skipped. Documents can bring their own
    | fonts with @font-face too.
    */
    'fonts' => [
        resource_path('fonts'),
    ],

    /*
    | Whether the fonts installed on the system serve too, as in Chrome: for the families that
    | documents name, the generic families and characters that other fonts lack. Off by default,
    | so that documents look the same on every machine.
    */
    'system_fonts' => env('FAGUS_SYSTEM_FONTS', false),

    /*
    | The language of the texts form controls show of their own (the labels of submit buttons,
    | the placeholders and formats of date fields …) where a document gives no lang, e.g. "de-DE".
    | Chrome writes them in the language of its user interface; American English if null.
    */
    'locale' => env('FAGUS_LOCALE'),

    /*
    | The families used for the generic families sans-serif, serif and monospace, by the names of
    | fonts in the directories above. serif is also the font of text without a font-family. Where a
    | family is null, Fagus takes the first font of the directories, else the built-in Inter.
    */
    'families' => [
        'sans-serif' => null,
        'serif' => null,
        'monospace' => null,
    ],

    /*
    | The directory relative URLs in documents (stylesheets, images, fonts) resolve against. Only
    | files inside it, or inside `root` if set, are read.
    */
    'base_directory' => public_path(),

    'root' => null,

    /*
    | Whether PDFs are tagged: with the logical structure of the document, which screen readers
    | read. Single documents can say otherwise with tagged() or ask for PDF/UA with accessible().
    */
    'tagged' => true,

    /*
    | Seconds to wait for each answer of the worker before it is stopped. Not on Windows, where
    | PHP cannot wait for a pipe with a time limit.
    */
    'timeout' => 60,

    /*
    | How much memory (megabytes, FAGUS_MEMORY_BUDGET) and time (seconds, FAGUS_TIME_LIMIT) one
    | document may take, all its parts together. A document that needs more fails with a
    | RenderException (diagnostic limit.memory or limit.time) and the worker goes on with the next.
    | The memory is an estimate of what images, layout, fonts and resources hold, 2048 if null.
    | The time limit is a tenth less than the timeout if null, so that the worker ends a document
    | before it is stopped. 0 turns either off.
    */
    'memory_budget' => env('FAGUS_MEMORY_BUDGET'),

    'time_limit' => env('FAGUS_TIME_LIMIT'),

    /*
    | When the worker process is replaced by a new one, which takes 3 to 4 ms: after as many
    | documents (FAGUS_MAX_DOCUMENTS), or after a document that left it with more resident memory
    | than as many megabytes (FAGUS_MAX_MEMORY; as far as the system tells: Linux, macOS, FreeBSD).
    | A long-running worker (queue, Octane) keeps memory its allocator holds after large
    | documents; a new one starts small. Never if null.
    */
    'max_documents' => env('FAGUS_MAX_DOCUMENTS'),

    'max_memory' => env('FAGUS_MAX_MEMORY'),

    /*
    | Whether the worker renders in its sandbox on Linux: without network, without starting
    | programs and without writing files (docs/cli.md#sandbox). Turn it off only to debug.
    */
    'sandbox' => env('FAGUS_SANDBOX', true),

];
