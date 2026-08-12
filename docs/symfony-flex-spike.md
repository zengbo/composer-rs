# Symfony Flex compatibility spike

**Date:** 2026-08-12  
**Related:** [ADR 0001 — plugin execution](adr/0001-plugin-execution.md)

## Goal

Decide how specific the hybrid Composer / composer-rs documentation should be for **Symfony Flex** projects, based on what Flex actually needs at install time.

## What Flex needs from Composer

| Capability | Required for | composer-rs today |
|------------|--------------|-------------------|
| `symfony/flex` as `composer-plugin` | Auto-enable Flex | ❌ plugins not executed |
| Plugin API (`PluginInterface`, events) | Recipe apply on require/update | ❌ |
| Packagist + Flex recipes endpoint | Download recipe JSON/patches | ⚠️ HTTP works; no Flex client |
| `extra.symfony` / `runtime` config | Flex options | ⚠️ stored in JSON, unused |
| `post-install-cmd` / `auto-scripts` | `cache:clear`, assets | ✅ shell scripts run if listed |
| `symfony/runtime` + `.env` | App bootstrap | ✅ packages install; no Flex copy |
| Recipe file operations (copy, gitignore, Makefile) | Project scaffold | ❌ |
| `composer recipes` / `recipes:install` | Manual recipe apply | ❌ |

## Spike conclusions

1. **composer-rs cannot replace Composer for a greenfield `symfony/skeleton` create-project flow.**  
   Flex must run as a PHP plugin during `require` / `create-project` to unpack recipes.

2. **composer-rs can speed up dependency install in an existing Symfony app** after Flex has already applied recipes, **if**:
   - lockfile is current;
   - no new Flex-managed packages are being added;
   - scripts that only shell out (`bin/console …`) are enough for CI.

3. **Hybrid is the right default** (confirm ADR 0001). Documentation should be **concrete**, not vague.

## Recommended hybrid workflows

### A. Day-to-day dependency refresh (no new Flex packages)

```bash
# Lock already contains all packages; only want faster vendor fill
composer-rs install --no-scripts          # optional: skip auto-scripts
# or
composer-rs update some/package -w        # if that package is not Flex-managed

# When auto-scripts matter:
composer-rs install
# post-install-cmd / auto-scripts run via shell if defined as strings
```

Use official Composer when:

```bash
composer require symfony/orm-pack     # Flex recipes must run
composer update symfony/*            # often pulls recipe updates
composer recipes                     # inspect / reinstall recipes
```

### B. CI: fast install, scripts via Composer once

```bash
# Preferred CI pattern for Flex apps
composer-rs install --no-scripts --ignore-platform-reqs=false
composer run-script auto-scripts     # or: composer install --no-download (if available)
# Simpler: keep one official Composer install for recipe-heavy branches
```

If `auto-scripts` is a map of console commands, composer-rs runs **string** scripts only.  
Complex Flex `auto-scripts` objects may need official Composer.

### C. New project

```bash
composer create-project symfony/skeleton my-app   # always Composer + Flex
cd my-app
# Later lockfile installs can use composer-rs install
```

Do **not** document `composer-rs create-project symfony/skeleton` as Flex-compatible.

### D. Detect Flex and warn (future enhancement)

Not implemented yet. Useful heuristic:

- lock or installed packages include `symfony/flex`, **and**
- command is `require` / `create-project` / partial update that adds packages

Then print:

```text
! Project uses symfony/flex. composer-rs does not execute Flex recipes.
! Use `composer require …` for new packages; `composer-rs install` is fine for lockfile installs.
```

## Documentation decision

| Question | Answer |
|----------|--------|
| Keep hybrid ADR high-level only? | **No** — Flex users need copy-paste workflows |
| Document Flex as “unsupported”? | **Partial** — unsupported for recipes/plugins; supported for lock install after Flex |
| Implement Flex natively? | **Out of scope** (plugin API); revisit only if shelling to `composer` for events is accepted |

## Acceptance for this spike

- [x] Gap table (above)
- [x] Concrete hybrid workflows A–C
- [x] Explicit non-support of Flex create-project / require recipes
- [x] Link from COMPOSER_PARITY / ADR

## Follow-ups (optional)

1. CLI warning when `symfony/flex` is in lock and user runs `require`
2. `composer-rs install --scripts-via=composer` to shell lifecycle to PHP Composer
3. Document in README “Symfony / Flex” subsection pointing here
