<?php

declare(strict_types=1);

namespace Fagus\Composer;

use Composer\Composer;
use Composer\EventDispatcher\EventSubscriberInterface;
use Composer\EventDispatcher\ScriptExecutionException;
use Composer\IO\IOInterface;
use Composer\Plugin\PluginInterface;
use Composer\Script\ScriptEvents;
use Fagus\Exception\FagusException;
use Fagus\Exception\InstallException;
use Fagus\Installer;
use Fagus\Internal\Release;

/**
 * Installs the fagus binary after `composer install` and `composer update`, so that requiring the
 * package is all it takes. The binary is taken from Composer's cache when it was downloaded
 * before; Composer's GitHub token (`github-oauth`) opens a private fork. If the binary
 * cannot be installed, Composer goes on and says why: `vendor/bin/fagus-install` tries again.
 * With `FAGUS_INSTALL_STRICT=1` in the environment, or `"extra": {"fagus": {"install-strict":
 * true}}` in the root composer.json, Composer fails instead, so that a deployment stops there
 * and not at the first PDF. `FAGUS_INSTALL_STRICT=0` turns the setting of composer.json off.
 *
 * @internal
 */
final class Plugin implements EventSubscriberInterface, PluginInterface
{
    private ?Composer $composer = null;

    private ?IOInterface $io = null;

    public function activate(Composer $composer, IOInterface $io): void
    {
        $this->composer = $composer;
        $this->io = $io;
    }

    public function deactivate(Composer $composer, IOInterface $io): void {}

    public function uninstall(Composer $composer, IOInterface $io): void {}

    /** @return array<string, string> */
    public static function getSubscribedEvents(): array
    {
        return [
            ScriptEvents::POST_INSTALL_CMD => 'installBinary',
            ScriptEvents::POST_UPDATE_CMD => 'installBinary',
        ];
    }

    public function installBinary(): void
    {
        if ($this->composer === null || $this->io === null) {
            return;
        }
        $package = $this->composer->getRepositoryManager()->getLocalRepository()->findPackage('nforst/fagus', '*');
        $directory = $package === null ? null : $this->composer->getInstallationManager()->getInstallPath($package);
        if ($directory === null) {
            return;
        }
        if (Release::BINARIES === []) {
            $strict = $this->strict();
            if ($strict !== null) {
                $this->fail(InstallException::developmentVersion(Release::VERSION), $strict);
            }
            $this->io->write('fagus: a development version, without binaries to install.', true, IOInterface::VERBOSE);

            return;
        }
        $config = $this->composer->getConfig();
        $tokens = $config->get('github-oauth');
        $cache = $config->get('cache-dir');
        try {
            $path = (new Installer(
                token: is_array($tokens) && is_string($tokens['github.com'] ?? null) ? $tokens['github.com'] : null,
                cacheDirectory: is_string($cache) && $cache !== '' ? $cache.'/fagus' : null,
            ))->install($directory.'/php/fagus/bin');
            $this->io->write("fagus: the binary is installed at {$path}.", true, IOInterface::VERBOSE);
        } catch (FagusException $exception) {
            $strict = $this->strict();
            if ($strict !== null) {
                $this->fail($exception, $strict);
            }
            $this->io->writeError('<warning>fagus: '.$exception->getMessage().'</warning>');
        }
    }

    /**
     * What makes a binary that cannot be installed fail Composer, if anything:
     * `FAGUS_INSTALL_STRICT` in the environment (`0`, `false`, `no` or `off` turn it off), else
     * `extra.fagus.install-strict` of the root package.
     */
    private function strict(): ?string
    {
        $value = getenv('FAGUS_INSTALL_STRICT');
        if ($value !== false && trim($value) !== '') {
            return in_array(strtolower(trim($value)), ['0', 'false', 'no', 'off'], true) ? null : 'FAGUS_INSTALL_STRICT';
        }
        $extra = $this->composer?->getPackage()->getExtra() ?? [];
        $settings = is_array($extra['fagus'] ?? null) ? $extra['fagus'] : [];

        return ($settings['install-strict'] ?? false) === true ? 'extra.fagus.install-strict' : null;
    }

    /**
     * Ends Composer with an error that says why the binary was not installed, as a failing script
     * does: the message, and the exit status 1.
     */
    private function fail(FagusException $exception, string $setting): never
    {
        $this->io?->writeError("<error>fagus: {$exception->getMessage()}</error>");
        $this->io?->writeError("<error>fagus: Composer fails because {$setting} is set. `vendor/bin/fagus-install` tries again.</error>");

        throw new ScriptExecutionException('fagus: the binary was not installed.', 1);
    }
}
