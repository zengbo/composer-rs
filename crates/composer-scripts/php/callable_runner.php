<?php
/**
 * composer-rs runner for Composer PHP callable scripts (`ClassName::method`).
 *
 * Official Composer invokes these in-process with a real Composer\Script\Event.
 * We spawn `php` and provide a small Event/Config/IO stub covering what typical
 * handlers (Laravel Illuminate\Foundation\ComposerScripts, Config::disableProcessTimeout,
 * Aws\Script\Composer\Composer::removeUnusedServices) actually call.
 *
 * argv: vendor-dir, bin-dir, class, method, event-name, dev-mode (0|1), extra...
 */

namespace Composer {
    class Config
    {
        /** @var array<string, mixed> */
        private $values;

        public function __construct(array $values)
        {
            $this->values = $values;
        }

        public function get($key, $flags = 0)
        {
            return array_key_exists($key, $this->values) ? $this->values[$key] : null;
        }

        public function has($key)
        {
            return array_key_exists($key, $this->values);
        }

        public static function disableProcessTimeout()
        {
            putenv('COMPOSER_PROCESS_TIMEOUT=0');
            if (function_exists('ini_set')) {
                @ini_set('max_execution_time', '0');
            }
        }
    }

    class RootPackage
    {
        /** @var string */
        private $name;
        /** @var array<string, mixed> */
        private $extra;

        public function __construct($name, array $extra)
        {
            $this->name = $name;
            $this->extra = $extra;
        }

        public function getName()
        {
            return $this->name;
        }

        public function getExtra()
        {
            return $this->extra;
        }
    }

    class Composer
    {
        /** @var Config */
        private $config;
        /** @var RootPackage */
        private $package;

        public function __construct(Config $config, RootPackage $package)
        {
            $this->config = $config;
            $this->package = $package;
        }

        public function getConfig()
        {
            return $this->config;
        }

        public function getPackage()
        {
            return $this->package;
        }
    }
}

namespace Composer\IO {
    interface IOInterface
    {
        public const QUIET = 1;
        public const NORMAL = 2;
        public const VERBOSE = 4;
        public const VERY_VERBOSE = 8;
        public const DEBUG = 16;
    }

    class NullIO
    {
        public function write($messages, $newline = true, $verbosity = 2)
        {
            $this->out(STDOUT, $messages, $newline);
        }

        public function writeError($messages, $newline = true, $verbosity = 2)
        {
            $this->out(STDERR, $messages, $newline);
        }

        public function writeRaw($messages, $newline = true, $verbosity = 2)
        {
            $this->write($messages, $newline, $verbosity);
        }

        public function writeErrorRaw($messages, $newline = true, $verbosity = 2)
        {
            $this->writeError($messages, $newline, $verbosity);
        }

        public function overwrite($messages, $newline = true, $size = null, $verbosity = 2)
        {
            $this->write($messages, $newline, $verbosity);
        }

        public function overwriteError($messages, $newline = true, $size = null, $verbosity = 2)
        {
            $this->writeError($messages, $newline, $verbosity);
        }

        public function isVerbose()
        {
            return false;
        }

        public function isVeryVerbose()
        {
            return false;
        }

        public function isDebug()
        {
            return false;
        }

        public function isDecorated()
        {
            return false;
        }

        public function isInteractive()
        {
            return false;
        }

        private function out($handle, $messages, $newline)
        {
            foreach ((array) $messages as $message) {
                fwrite($handle, $message . ($newline ? "\n" : ''));
            }
        }
    }
}

namespace Composer\EventDispatcher {
    class Event
    {
        /** @var string */
        private $name;
        /** @var array<int, mixed> */
        private $args;
        /** @var array<string, mixed> */
        private $flags;
        /** @var bool */
        private $propagationStopped = false;

        public function __construct($name, array $args = [], array $flags = [])
        {
            $this->name = $name;
            $this->args = $args;
            $this->flags = $flags;
        }

        public function getName()
        {
            return $this->name;
        }

        public function getArguments()
        {
            return $this->args;
        }

        public function getFlags()
        {
            return $this->flags;
        }

        public function isPropagationStopped()
        {
            return $this->propagationStopped;
        }

        public function stopPropagation()
        {
            $this->propagationStopped = true;
        }
    }
}

namespace Composer\Script {
    class Event extends \Composer\EventDispatcher\Event
    {
        /** @var \Composer\Composer */
        private $composer;
        /** @var object */
        private $io;
        /** @var bool */
        private $devMode;
        /** @var mixed */
        private $originatingEvent;

        public function __construct($name, $composer, $io, $devMode = false, array $args = [], array $flags = [])
        {
            parent::__construct($name, $args, $flags);
            $this->composer = $composer;
            $this->io = $io;
            $this->devMode = $devMode;
        }

        public function getComposer()
        {
            return $this->composer;
        }

        public function getIO()
        {
            return $this->io;
        }

        public function isDevMode()
        {
            return $this->devMode;
        }

        public function getOriginatingEvent()
        {
            return $this->originatingEvent;
        }

        public function setOriginatingEvent($event)
        {
            $this->originatingEvent = $event;

            return $this;
        }
    }
}

namespace {
    if ($argc < 7) {
        fwrite(STDERR, "composer-rs callable runner: missing arguments\n");
        exit(64);
    }

    $vendorDir = $argv[1];
    $binDir = $argv[2];
    $className = $argv[3];
    $methodName = $argv[4];
    $eventName = $argv[5];
    $devMode = $argv[6] === '1';
    $extraArgs = array_slice($argv, 7);

    // Official Composer runs script handlers in-process with the full autoloader
    // (PSR/classmap *and* `files`). Handlers such as Laravel ComposerScripts may
    // require vendor/autoload.php themselves; require_once makes that a no-op.
    $autoload = rtrim($vendorDir, '/\\') . DIRECTORY_SEPARATOR . 'autoload.php';
    if (is_file($autoload)) {
        require_once $autoload;
    } else {
        // Minimal vendor trees (tests) may only expose composer/*.php maps.
        $composerDir = rtrim($vendorDir, '/\\') . DIRECTORY_SEPARATOR . 'composer';
        $classLoaderFile = $composerDir . DIRECTORY_SEPARATOR . 'ClassLoader.php';
        if (is_file($classLoaderFile)) {
            require_once $classLoaderFile;
            $loader = new Composer\Autoload\ClassLoader();
            $psr4File = $composerDir . DIRECTORY_SEPARATOR . 'autoload_psr4.php';
            if (is_file($psr4File)) {
                $map = require $psr4File;
                foreach ($map as $namespace => $paths) {
                    if ($namespace === '') {
                        continue;
                    }
                    $loader->addPsr4($namespace, $paths);
                }
            }
            $psr0File = $composerDir . DIRECTORY_SEPARATOR . 'autoload_namespaces.php';
            if (is_file($psr0File)) {
                $map = require $psr0File;
                foreach ($map as $namespace => $paths) {
                    $loader->add($namespace, $paths);
                }
            }
            $classMapFile = $composerDir . DIRECTORY_SEPARATOR . 'autoload_classmap.php';
            if (is_file($classMapFile)) {
                $map = require $classMapFile;
                if (is_array($map) && $map) {
                    if (method_exists($loader, 'addClassMap')) {
                        $loader->addClassMap($map);
                    } else {
                        $loader->classMap = $map + (array) $loader->classMap;
                    }
                }
            }
            $loader->register(true);
        }
    }

    if (!class_exists($className)) {
        exit(2);
    }
    if (!is_callable([$className, $methodName])) {
        exit(3);
    }

    $config = new Composer\Config([
        'vendor-dir' => $vendorDir,
        'bin-dir' => $binDir,
    ]);
    $rootName = '__root__';
    $rootExtra = [];
    $composerJson = getcwd() . DIRECTORY_SEPARATOR . 'composer.json';
    if (is_file($composerJson)) {
        $manifest = json_decode((string) file_get_contents($composerJson), true);
        if (is_array($manifest)) {
            if (isset($manifest['name']) && is_string($manifest['name']) && $manifest['name'] !== '') {
                $rootName = $manifest['name'];
            }
            if (isset($manifest['extra']) && is_array($manifest['extra'])) {
                $rootExtra = $manifest['extra'];
            }
        }
    }
    $composer = new Composer\Composer($config, new Composer\RootPackage($rootName, $rootExtra));
    $io = new Composer\IO\NullIO();
    $event = new Composer\Script\Event($eventName, $composer, $io, $devMode, $extraArgs);

    try {
        $ref = new ReflectionMethod($className, $methodName);
        if ($ref->getNumberOfParameters() === 0) {
            $result = $className::$methodName();
        } else {
            $result = $className::$methodName($event);
        }
    } catch (Throwable $e) {
        fwrite(STDERR, $e->getMessage() . "\n" . $e->getTraceAsString() . "\n");
        exit(4);
    }

    if ($result === false) {
        exit(1);
    }
}
