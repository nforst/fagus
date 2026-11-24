<?php

declare(strict_types=1);

namespace Fagus;

/**
 * What a viewer shows beside the pages when it opens the PDF.
 */
enum PageMode: string
{
    /** Nothing but the pages. */
    case None = 'none';

    /** The document outline (the bookmarks). */
    case Outlines = 'outlines';

    /** The thumbnails of the pages. */
    case Thumbnails = 'thumbnails';

    /** The pages in full-screen mode, without menus or window controls. */
    case FullScreen = 'full-screen';

    /** The attachments. PDF/A-1b does not have it. */
    case Attachments = 'attachments';

    /** The layers. PDF/A-1b does not have it. */
    case Layers = 'layers';
}
