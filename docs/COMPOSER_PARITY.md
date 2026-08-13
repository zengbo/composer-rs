# Composer Parity Roadmap

Comparison of **official Composer** vs **composer-rs**, plus a task breakdown for closing functional gaps.

**Status legend**

| Symbol | Meaning |
|--------|---------|
| ✅ | Supported (Composer-compatible for typical use) |
| ⚠️ | Partial / limited parity |
| ❌ | Not implemented |
| — | Intentionally out of scope |

**Last reviewed:** 2026-08-13 (PHP `Class::method` callable scripts)

---

## 1. Core dependency workflow

| Feature | Composer | composer-rs | Notes |
|---------|----------|-------------|-------|
| `install` (from lock) | ✅ | ✅ | |
| `install` (no lock → resolve) | ✅ | ✅ | |
| `update` (full) | ✅ | ✅ | PubGrub solver |
| `update` (partial, listed only) | ✅ | ✅ | Default partial update |
| `update -w` / `--with-dependencies` | ✅ | ✅ | Skips root requirements |
| `update -W` / `--with-all-dependencies` | ✅ | ✅ | Includes root requirements |
| Partial update walks `require-dev` | ✅ | ✅ | When `--no-dev` is off |
| `require` / `remove` | ✅ | ✅ | Triggers resolve + install |
| `--no-dev` | ✅ | ✅ | install / update / require / remove |
| `--dry-run` | ✅ | ✅ | install / update |
| `--prefer-dist` / `--prefer-source` | ✅ | ✅ | |
| `--ignore-platform-reqs` | ✅ | ✅ | All platform reqs |
| `--ignore-platform-req=ext-*` | ✅ | ✅ | Repeatable patterns |
| `--verify-checksums` / shasum | ✅ | ⚠️ | SHA-1 when dist provides shasum |
| `content-hash` in lock | ✅ | ✅ | validate + install warning |
| `--no-autoloader` | ✅ | ✅ | |
| `--no-scripts` | ✅ | ✅ | |
| `--no-plugins` | ✅ | ⚠️ | Plugins never loaded; flag N/A |

---

## 2. Resolution & lockfile

| Feature | Composer | composer-rs | Notes |
|---------|----------|-------------|-------|
| PubGrub / SAT solving | ✅ | ✅ | |
| `provide` / virtual packages | ✅ | ✅ | Multi-provider |
| `replace` | ✅ | ✅ | |
| `conflict` | ✅ | ✅ | Encoded in PubGrub (reachable packages) + post-solve safety net |
| `minimum-stability` | ✅ | ✅ | |
| `prefer-stable` | ✅ | ✅ | |
| `prefer-lowest` | ✅ | ✅ | update flag |
| Per-package stability flags in lock | ✅ | ❌ | |
| Lock aliases | ✅ | ❌ | |
| Platform packages in lock | ✅ | ✅ | `platform` / `platform-dev` |
| `config.platform` overrides | ✅ | ✅ | |

---

## 3. Repositories & downloads

| Feature | Composer | composer-rs | Notes |
|---------|----------|-------------|-------|
| Packagist (Composer API v2 / P2) | ✅ | ✅ | |
| Custom `composer` repository | ✅ | ✅ | |
| `path` repository | ✅ | ✅ | Symlink default |
| `vcs` / `git` / `github` / `gitlab` | ✅ | ✅ | Clone + checkout |
| Inline `package` repository | ✅ | ✅ | |
| Disable Packagist (`packagist.org: false`) | ✅ | ✅ | |
| Private repo auth (`auth.json`) | ✅ | ✅ | Project + global; resolve/require/show/search/download |
| `COMPOSER_AUTH` env | ✅ | ✅ | |
| GitHub OAuth / rate limits | ✅ | ⚠️ | Token applied when configured |
| Dist mirrors / failover | ✅ | ✅ | Primary `dist.url` then `dist.mirrors` (string or `{url}`); p2 → lock preserved |
| Parallel downloads | ⚠️ | ✅ | Core strength |
| CAS + hardlink install | ❌ | ✅ | composer-rs differentiator |

---

## 4. Install layout & autoload

| Feature | Composer | composer-rs | Notes |
|---------|----------|-------------|-------|
| `vendor/` install | ✅ | ✅ | |
| `config.vendor-dir` | ✅ | ✅ | |
| `extra.installer-paths` | ✅ | ✅ | Static patterns |
| Custom installers (`composer/installers` plugin) | ✅ | ⚠️ | Built-in type→path defaults (no PHP plugin) |
| `vendor/bin` symlinks | ✅ | ✅ | |
| `config.bin-dir` | ✅ | ✅ | |
| PSR-4 / PSR-0 autoload | ✅ | ✅ | |
| `autoload-dev` | ✅ | ✅ | |
| Classmap / files autoload | ✅ | ✅ | File-level classmap entries; `files` ordered deps-first (PackageSorter) |
| `-o` / `--optimize-autoloader` | ✅ | ✅ | |
| `-a` / `--classmap-authoritative` | ✅ | ✅ | |
| APCu autoloader prefix | ✅ | ⚠️ | PHP stub exists, no CLI flag |
| `vendor/autoload.php` | ✅ | ✅ | Ships Composer’s ClassLoader (incl. `$includeFile`) |
| `installed.json` / `installed.php` | ✅ | ✅ | Composer 2 shape + version_normalized + install-path |
| `InstalledVersions.php` | ✅ | ✅ | Official Composer runtime class |
| `platform_check.php` | ✅ | ✅ | |

---

## 5. Scripts & plugins

| Feature | Composer | composer-rs | Notes |
|---------|----------|-------------|-------|
| `scripts` in composer.json | ✅ | ✅ | Shell + `@script` + `@php` + `Class::method` |
| Lifecycle: `pre/post-install-cmd` | ✅ | ✅ | |
| Lifecycle: `pre/post-update-cmd` | ✅ | ✅ | |
| Lifecycle: `pre/post-autoload-dump` | ✅ | ✅ | |
| Per-package install/update scripts | ✅ | ❌ | |
| `composer run-script` / `run` | ✅ | ✅ | |
| Script `@references` | ✅ | ✅ | Cycle detection |
| PHP `@callable` scripts | ✅ | ✅ | `Class::method` via `php` + Event stub (`vendor-dir`, IO, `isDevMode`) |
| `composer-plugin` packages | ✅ | ❌ | See [ADR 0001](adr/0001-plugin-execution.md) |
| `config.allow-plugins` | ✅ | ⚠️ | Parsed; plugins are never executed |
| Symfony Flex / recipes | ✅ | ❌ | Plugin-dependent |

---

## 6. CLI commands

| Command | Composer | composer-rs | Priority |
|---------|----------|-------------|----------|
| `install` | ✅ | ✅ | — |
| `update` | ✅ | ✅ | — |
| `require` | ✅ | ✅ | — |
| `remove` | ✅ | ✅ | — |
| `init` | ✅ | ✅ | — |
| `validate` | ✅ | ✅ | Constraints + lock hash + `--strict` |
| `dump-autoload` | ✅ | ✅ | Falls back to `installed.json` if lock is missing |
| `search` | ✅ | ✅ | Uses project repos + auth.json |
| `show` | ✅ | ✅ | `--tree`, `--direct`, `--path` |
| `cache` | ✅ | ✅ | clear / info / dir / repo / prune (`gc`; nlink GC; copy-install caveat) |
| `bump` | ✅ | ✅ | |
| `fund` | ✅ | ✅ | Reads vendor composer.json funding |
| `exec` | ✅ | ✅ | vendor/bin on PATH |
| `global` | ✅ | ✅ | Runs under COMPOSER_HOME |
| **`outdated`** | ✅ | ✅ | |
| **`why` / `why-not`** | ✅ | ✅ | |
| **`depends` / `prohibits`** | ✅ | ✅ | |
| **`check-platform-reqs`** | ✅ | ✅ | |
| **`reinstall`** | ✅ | ✅ | |
| **`config`** | ✅ | ✅ | Subset: vendor-dir, bin-dir, platform.*, allow-plugins, --auth |
| **`run-script` / `run`** | ✅ | ✅ | |
| `create-project` | ✅ | ⚠️ | Scaffold + resolve/install; `type:project` unpacks to root. No Flex/plugin recipes; flag set is a subset |
| `licenses` | ✅ | ✅ | |
| `diagnose` | ✅ | ✅ | |
| `status` | ✅ | ✅ | Install markers + version/cache_key checks |
| `audit` | ✅ | ✅ | Packagist security advisories API |
| `self-update` | ✅ | — | N/A (use `cargo install`) |
| `archive` / `publish` | ✅ | — | Out of scope for package manager core |

---

## 7. Security & compliance

| Feature | Composer | composer-rs | Notes |
|---------|----------|-------------|-------|
| `allow-plugins` enforcement | ✅ | ⚠️ | Warning only; granting it does not run plugins |
| `composer audit` | ✅ | ✅ | |
| Zip-slip protection | ✅ | ✅ | `safe_join` on extract |
| Audit on install (`--audit`) | ✅ | ✅ | |

---

## Implementation tasks

### Phase 0 — Baseline (done)

- [x] **P0-0.1** PubGrub resolve + lock read/write
- [x] **P0-0.2** Parallel dist download + CAS hardlink install
- [x] **P0-0.3** Path / VCS / Composer repositories
- [x] **P0-0.4** Platform requirements + `config.platform`
- [x] **P0-0.5** Partial update (`-w` / `-W`, `require-dev` edges)
- [x] **P0-0.6** Autoload generation (`-o`, `-a`, `platform_check.php`)
- [x] **P0-0.7** E2E tests (path repo + mock dist zip)
- [x] **P0-0.8** Partial-update mock integration tests

### Phase 1 — Daily-driver parity (done)

- [x] **1.1** `vendor/bin` links
- [x] **1.2** `config.bin-dir` + manifest helpers
- [x] **1.3** `outdated` command
- [x] **1.4** Scripts runner (minimal)
- [x] **1.5** `run-script` / `run` command

### Phase 2 — Private repos & install flags (done)

- [x] **2.1** Auth config loader (`auth.json` + `COMPOSER_AUTH`)
- [x] **2.2** HTTP auth on download + repository
- [x] **2.3** `config` command (subset)
- [x] **2.4** Per-platform-req ignore
- [x] **2.5** `--no-autoloader` flag
- [x] **2.6** `check-platform-reqs` command
- [x] **2.7** `reinstall` command
- [x] **2.8** Dependency insight (`depends` / `why` / `prohibits` / `why-not`)

### Phase 3 — Security, diagnostics, scaffolding (done)

- [x] **3.1** `allow-plugins` parsing + warnings
- [x] **3.2** `audit` command
- [x] **3.3** `status` command (install markers + version/`cache_key`; `composer.json` fallback)
- [x] **3.4** `diagnose` command
- [x] **3.5** `licenses` command
- [x] **3.6** `create-project` (scaffold + resolve/install; `type:project` unpacks to root)
- [x] **3.7** Richer `validate`
- [x] **3.8** `installed.json` schema improvements

### Phase 4 — Plugins & long tail

- [x] **4.1** Plugin execution strategy (ADR)
- [x] **4.2** Symfony Flex compatibility spike ([symfony-flex-spike.md](symfony-flex-spike.md))
- [x] **4.3** `composer/installers` type path defaults
- [x] **4.4** `bump`, `fund`, `exec`, `global`
- [x] **4.5** `show --tree`, precise `status`, installed.json golden fields

### Phase 5 — Daily-driver polish (Tier 1–3)

Closing remaining gaps after Phases 1–4. Items below are **done** unless marked remaining.

#### Tier 1 — Auth / install wiring

- [x] **5.1** `show` uses project `auth.json`
- [x] **5.2** `reinstall` regenerates autoload + bin links
- [x] **5.3** Duplicate `vendor/bin` names reported (`BinInstallResult.conflicts`)
- [x] **5.4** `exec` PATH separator (`:` vs `;`)
- [x] **5.5** `RepositoryRegistry::from_manifest` deprecated in favor of `from_manifest_auth`

#### Tier 2 — Config, solver, downloads

- [x] **5.6** `config.platform-check` honored by `platform_check.php`
- [x] **5.7** `preferred-install` / prefer-dist
- [x] **5.8** `cache` `{clear,info,dir,repo,prune}`
- [x] **5.9** `create-project` unpacks `type:project` to the target root
- [x] **5.10** `update --lock` refreshes content-hash only
- [x] **5.11** Solver `conflict` encoding (reachable complement + post-solve check)
- [x] **5.12** Dist `mirrors` failover (p2 → lock + wiremock)

#### Tier 3 — Tests, CI, docs

- [x] **5.13** E2E scripts + `vendor/bin`
- [x] **5.14** `audit` wiremock (`COMPOSER_RS_AUDIT_URL`)
- [x] **5.15** `outdated` markers `!` / `~` / `=` and `--strict`
- [x] **5.16** [BENCHMARK.md](BENCHMARK.md) (manual; no automated CI bench)
- [x] **5.17** CI on `v*` tags (and manual `workflow_dispatch`): test on `ubuntu-24.04`; native release builds on `ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-14`; tags publish GitHub Releases

---

## Non-goals (documented)

These are **structural** gaps, not leftover checklist items:

| Gap | Why it stays out |
|-----|------------------|
| Full PHP plugin API (`composer-plugin`) | Requires embedding / reimplementing Composer’s plugin runtime. See [ADR 0001](adr/0001-plugin-execution.md). |
| Symfony Flex recipes | Plugin-dependent; hybrid workflow only ([symfony-flex-spike.md](symfony-flex-spike.md)). |
| Per-package stability flags / lock aliases | Lock schema extras; not needed for typical Packagist installs. |
| `self-update` | Use `cargo install --path crates/composer-cli`. |
| Packagist `publish` / `archive` | Out of scope for the package-manager core. |
| Replacing Composer in plugin-heavy monoliths | Hybrid: Composer for plugins/Flex, composer-rs for lock install. |

Also not planned soon: APCu autoloader CLI flag, Symfony Command-class scripts, per-package install/update scripts. PHP `Class::method` handlers get a stub `Composer\Script\Event` (enough for Laravel `ComposerScripts` and `Composer\Config::disableProcessTimeout`); they do not receive a full in-process Composer object.

---

## Related docs

- [README](../README.md) — user-facing usage and architecture
- [ADR 0001: Plugin execution](adr/0001-plugin-execution.md)
- [Symfony Flex spike](symfony-flex-spike.md)
- [BENCHMARK.md](BENCHMARK.md)
