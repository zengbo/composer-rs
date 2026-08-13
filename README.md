# composer-rs

High-performance, Composer-compatible PHP package manager written in Rust.

Inspired by [libretto](https://github.com/libretto-pm/libretto) and the storage models of **pnpm** / **uv**, with two primary goals:

1. **Parallel high-speed downloads** (HTTP/2, adaptive concurrency)
2. **Package sharing via content-addressable cache** — multiple git worktrees hardlink the same package files instead of each keeping a full `vendor/` copy

> Status: early but usable for `install` / `update` from Packagist. Not a full drop-in for every Composer plugin workflow.

## Why

| Problem | Composer | composer-rs |
|--------|----------|-------------|
| Many worktrees | Each has its own `vendor/` | Shared CAS + hardlinks |
| Cold install | Limited parallelism | Adaptive concurrency (16–128) |
| Warm install | Re-extract archives | Instant hardlink from cache |
| Disk use | Linear in worktree count | Packages stored once |

### Content-addressable cache layout

```
~/.cache/composer-rs/
├── cas/
│   └── ab/
│       └── abcd1234.../     # extracted package tree (keyed by dist hash)
│           ├── src/
│           └── .composer-rs-complete
└── archives/
    └── <blake3>.zip         # raw dist downloads
```

On install, files are **hardlinked** from CAS into `vendor/vendor/name/`. A second worktree paying for `symfony/console` costs almost no extra disk.

`composer-rs cache prune` (alias: `gc`) deletes CAS trees whose files have `nlink == 1` — nothing in any `vendor/` still points at them. It does not scan worktree paths and does not touch `archives/` or `metadata/`. After every worktree that used a package has dropped its `vendor/` copy, prune reclaims that CAS entry.

If install reports `copies > 0` (hardlink failed, usually a different filesystem / APFS volume), prune may reclaim those CAS trees early. `vendor/` is unaffected, but the warm cache for those files is gone.

Override cache location with `COMPOSER_RS_CACHE`.

## Install

```bash
cargo install --path crates/composer-cli
# binary: composer-rs
```

Or build from source:

```bash
cargo build --release -p composer-cli
./target/release/composer-rs --help
```

## Usage

```bash
# Install from composer.lock (or resolve if missing)
composer-rs install

# Resolve latest matching versions, rewrite lock, install
composer-rs update
composer-rs update vendor/package        # partial: only listed package
composer-rs update vendor/package -w     # also its non-root dependencies
composer-rs update vendor/package -W     # also all dependencies (incl. root reqs)
composer-rs update --lock                # refresh content-hash only (no version changes)

# Cache paths
composer-rs cache dir
composer-rs cache repo

# Add / remove packages
composer-rs require psr/log
composer-rs require phpunit/phpunit --dev
composer-rs remove psr/log

# Autoloader / validate / search
composer-rs dump-autoload -o
composer-rs validate
composer-rs search monolog
composer-rs show psr/log

# Day-to-day extras
composer-rs outdated
composer-rs why monolog/monolog
composer-rs depends psr/log
composer-rs check-platform-reqs
composer-rs reinstall vendor/package
composer-rs run-script test
composer-rs audit
composer-rs licenses
composer-rs diagnose
composer-rs show -t
composer-rs bump
composer-rs fund
composer-rs exec phpunit -- --version
composer-rs global require phpunit/phpunit

# Cache
composer-rs cache info
composer-rs cache prune          # drop CAS packages with no vendor hardlinks (alias: gc)
composer-rs cache prune --dry-run
composer-rs cache clear          # wipe CAS + archives + metadata
```

### Useful flags

| Flag | Description |
|------|-------------|
| `--concurrency N` | Cap parallel downloads (default: `cores×8`, clamped 16–128) |
| `--no-dev` | Skip `require-dev` |
| `--ignore-platform-reqs` | Skip all `php` / `ext-*` checks |
| `--ignore-platform-req=ext-*` | Skip matching platform reqs (repeatable) |
| `--prefer-source` | Prefer VCS clone over dist zip |
| `-w` / `--with-dependencies` | Partial update: also free non-root deps of listed packages |
| `-W` / `--with-all-dependencies` | Partial update: also free all deps (including root reqs) |
| `--no-scripts` | Skip lifecycle / named scripts |
| `--no-autoloader` | Skip autoload generation |
| `--verify-checksums` | Fail on dist shasum mismatch |
| `--dry-run` | Resolve / plan only |
| `-o` / `--optimize-autoloader` | Classmap optimization |

See **[docs/COMPOSER_PARITY.md](docs/COMPOSER_PARITY.md)** for full Composer parity status.  
See **[docs/BENCHMARK.md](docs/BENCHMARK.md)** for how to compare cold/warm install vs Composer.

## Architecture

```
crates/
├── composer-cli         # CLI binary
├── composer-core        # PackageId, versions, constraints, errors
├── composer-manifest    # composer.json
├── composer-lock        # composer.lock
├── composer-auth        # auth.json / COMPOSER_AUTH
├── composer-cache       # CAS + hardlink install
├── composer-repo        # Packagist Composer API v2 client
├── composer-resolver    # Parallel metadata fetch + PubGrub solver
├── composer-download    # HTTP/2 parallel download + zip/tar extract + bins
├── composer-autoload    # vendor/autoload.php generation
└── composer-scripts     # scripts + lifecycle hooks
```

## Advanced features

### PubGrub resolver
`update` / unlocked `install` use a **PubGrub** solver over Packagist metadata (plus path/VCS packages). Conflicts that cannot be solved fail with a PubGrub error instead of silently picking an invalid set.

### Path & VCS repositories
```json
{
  "repositories": [
    { "type": "path", "url": "../packages/my-lib" },
    { "type": "vcs", "url": "https://github.com/acme/private.git" }
  ],
  "require": {
    "acme/my-lib": "*",
    "acme/private": "dev-main"
  }
}
```
Path packages are **symlinked** into `vendor/` by default. VCS packages are cloned under the global cache then copied into place.

### installer-paths
```json
{
  "extra": {
    "installer-paths": {
      "wp-content/plugins/{$name}/": ["type:wordpress-plugin"],
      "modules/{$name}/": ["type:drupal-module"]
    }
  }
}
```

## Compatibility notes

- Reads/writes standard `composer.json` and `composer.lock`
- Installs dist archives from Packagist (zip / tar.gz / …)
- Path + VCS repositories, `extra.installer-paths`, PubGrub resolution
- Platform requirements (`php`, `ext-*`) with `config.platform` overrides
- `vendor/bin` links, scripts (`post-autoload-dump`, `run-script`, `@php`, `Class::method`), auth.json
- Generates a usable PSR-4 / classmap autoloader with `platform_check.php`
- **Not supported:** PHP Composer plugins / Flex recipes (see [docs/adr/0001-plugin-execution.md](docs/adr/0001-plugin-execution.md) and [docs/symfony-flex-spike.md](docs/symfony-flex-spike.md) for hybrid CI workflows)
- `install --audit` / `update --audit` **fail the command** (exit 1) when advisories are found

For plugin-heavy projects keep using official Composer; use composer-rs where install speed and worktree disk matter most (CI, monorepos, many branches).

## License

MIT
