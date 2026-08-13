# ADR 0001: Plugin execution strategy

## Status

Accepted (2026-08-12)

## Context

Composer plugins (`type: composer-plugin`) and Symfony Flex recipes require a PHP plugin API that composer-rs does not implement. Many real-world PHP apps depend on plugins for installers, Flex, and tooling hooks.

## Decision

composer-rs uses a **hybrid** approach:

1. **Native path (default):** install packages, generate autoload, run shell/`@php`/`Class::method` scripts, link bins. Do not load or execute Composer plugins. Warn when `composer-plugin` packages are present (`config.allow-plugins` does not enable them).
2. **Documented escape hatch:** for plugin-heavy projects, run official Composer for install/update when plugins are required:
   ```bash
   composer install   # plugins / Flex
   # or keep using composer-rs where plugins are not needed
   ```
3. **Long-term options (not implemented):**
   - (A) Shell out to `composer` for plugin events only
   - (B) Embed PHP (php-embed) to run plugin code
   - (C) Reimplement high-value plugins natively (e.g. `composer/installers` path map — partially done via built-in type paths)

## Non-goals

- Full PHP plugin API compatibility in the near term
- Shipping a bundled PHP runtime
- Silent no-op of critical plugin behavior without warnings

## Consequences

- WordPress/Drupal-style package types get default install paths without plugins.
- Flex recipes still need official Composer (or manual recipes).
- `config.allow-plugins` is parsed for `config` / `composer.json` fidelity, but granting it does not execute plugins.

## Symfony Flex (concrete hybrid)

See **[docs/symfony-flex-spike.md](../symfony-flex-spike.md)** for the spike write-up. Summary:

| Task | Tool |
|------|------|
| `create-project symfony/skeleton` | **Composer only** |
| `require` packs that apply recipes | **Composer only** |
| `install` from an existing lock (no new recipes) | **composer-rs OK** (faster vendor fill) |
| `auto-scripts` / `bin/console` post-install | composer-rs if scripts are shell strings; else Composer |

Do not advertise composer-rs as a Flex replacement.
