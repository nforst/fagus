<?php

declare(strict_types=1);

namespace Fagus\Exception;

use Fagus\Conformance;
use Fagus\Permission;

/**
 * A value given to Fagus that it cannot use, such as an unknown PDF/A level.
 */
final class InvalidArgumentException extends FagusException
{
    public static function unknownConformance(string $level): self
    {
        $levels = implode(', ', array_map(static fn (Conformance $level): string => "'{$level->value}'", Conformance::cases()));

        return new self("Unknown PDF/A level '{$level}'. The levels are {$levels}.");
    }

    public static function unknownPermission(mixed $permission): self
    {
        $permissions = implode(', ', array_map(static fn (Permission $permission): string => "'{$permission->value}'", Permission::cases()));
        $given = is_string($permission) ? "'{$permission}'" : get_debug_type($permission);

        return new self("Unknown permission {$given}. The permissions are {$permissions}.");
    }

    public static function noPassword(): self
    {
        return new self('Encryption needs a user password or an owner password.');
    }

    public static function invalidZoom(float $zoom): self
    {
        return new self("The zoom must be a number of percent above 0, Zoom::FitPage or Zoom::FitWidth, not {$zoom}.");
    }

    public static function invalidPage(int $page): self
    {
        return new self("The page to open at must be at least 1, not {$page}.");
    }

    public static function notPositive(string $name, int $value): self
    {
        return new self("The engine's {$name} must be at least 1 or null, not {$value}.");
    }

    public static function negative(string $name, int|float $value): self
    {
        return new self("The engine's {$name} must be at least 0 or null, not {$value}.");
    }

    public static function imageQuality(int $quality): self
    {
        return new self("The image quality must be between 1 and 100 or null, not {$quality}.");
    }

    public static function emptyInvoiceField(string $field): self
    {
        return new self("The invoice {$field} must not be empty.");
    }

    public static function invalidPages(string $pages, string $reason): self
    {
        return new self("Invalid pages `{$pages}`: {$reason}.");
    }

    public static function nothingToMerge(): self
    {
        return new self('There are no PDFs to merge; add them with add() or addFile().');
    }
}
