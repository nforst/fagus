<?php

declare(strict_types=1);

namespace Fagus;

/**
 * How a viewer lays the pages out when it opens the PDF.
 */
enum PageLayout: string
{
    /** One page at a time. */
    case SinglePage = 'single-page';

    /** The pages in one column, scrolling. */
    case OneColumn = 'one-column';

    /** The pages in two columns, scrolling, the odd pages on the left. */
    case TwoColumnLeft = 'two-column-left';

    /** The pages in two columns, scrolling, the odd pages on the right: the first page stands alone, as a cover. */
    case TwoColumnRight = 'two-column-right';

    /** Two pages at a time, the odd pages on the left. PDF/A-1b does not have it. */
    case TwoPageLeft = 'two-page-left';

    /** Two pages at a time, the odd pages on the right. PDF/A-1b does not have it. */
    case TwoPageRight = 'two-page-right';
}
