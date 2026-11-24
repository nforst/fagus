<?php

declare(strict_types=1);

namespace Fagus;

/**
 * How large a viewer shows the pages when it opens the PDF, where it is no number of percent.
 */
enum Zoom: string
{
    /** The whole page fits the window. */
    case FitPage = 'fit-page';

    /** The width of the page fits the window. */
    case FitWidth = 'fit-width';
}
