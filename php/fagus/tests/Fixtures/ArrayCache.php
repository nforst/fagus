<?php

declare(strict_types=1);

namespace Fagus\Tests\Fixtures;

use DateInterval;
use InvalidArgumentException;
use Psr\SimpleCache\CacheInterface;

/**
 * A PSR-16 cache in an array, which records the calls it gets and refuses the keys PSR-16
 * forbids.
 */
final class ArrayCache implements CacheInterface
{
    /** @var array<string, mixed> */
    public array $entries = [];

    /** @var list<array{0: string, 1: list<string>, 2?: DateInterval|int|null}> */
    public array $calls = [];

    public function get(string $key, mixed $default = null): mixed
    {
        $this->calls[] = ['get', [$key]];

        return $this->entries[self::valid($key)] ?? $default;
    }

    public function set(string $key, mixed $value, DateInterval|int|null $ttl = null): bool
    {
        $this->calls[] = ['set', [$key], $ttl];
        $this->entries[self::valid($key)] = $value;

        return true;
    }

    public function delete(string $key): bool
    {
        unset($this->entries[self::valid($key)]);

        return true;
    }

    public function clear(): bool
    {
        $this->entries = [];

        return true;
    }

    public function getMultiple(iterable $keys, mixed $default = null): iterable
    {
        $keys = [...$keys];
        $this->calls[] = ['getMultiple', $keys];
        $values = [];
        foreach ($keys as $key) {
            $values[$key] = $this->entries[self::valid($key)] ?? $default;
        }

        return $values;
    }

    public function setMultiple(iterable $values, DateInterval|int|null $ttl = null): bool
    {
        $values = [...$values];
        $this->calls[] = ['setMultiple', array_map(strval(...), array_keys($values)), $ttl];
        foreach ($values as $key => $value) {
            $this->entries[self::valid((string) $key)] = $value;
        }

        return true;
    }

    public function deleteMultiple(iterable $keys): bool
    {
        foreach ($keys as $key) {
            $this->delete($key);
        }

        return true;
    }

    public function has(string $key): bool
    {
        return isset($this->entries[self::valid($key)]);
    }

    private static function valid(string $key): string
    {
        if ($key === '' || strpbrk($key, '{}()/\@:') !== false) {
            throw new InvalidArgumentException("`{$key}` is no key of a PSR-16 cache.");
        }

        return $key;
    }
}
