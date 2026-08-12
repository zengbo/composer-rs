# Benchmark notes: composer-rs vs Composer

These numbers are **indicative**. Always re-run on the machine you care about.
The point of composer-rs is **parallel HTTP + CAS hardlinks**, not a faster solver.

## What to measure

| Scenario | Command | What it stresses |
|----------|---------|------------------|
| Cold resolve + install | empty cache, `composer-rs install` (no lock) | metadata fetch + PubGrub + first download |
| Warm install (CAS hit) | cache populated, wipe `vendor/`, `composer-rs install` | hardlink from CAS (should dominate Composer extract) |
| Worktree clone | new git worktree, same cache | CAS sharing across trees (disk, not just time) |
| Partial update | `composer-rs update vendor/pkg -w` | pin + prefetch |

Compare against official Composer on the **same** `composer.json` / lock / network.

## How to run (local)

```bash
# Isolated cache so runs don't pollute ~/.cache
export COMPOSER_RS_CACHE=/tmp/composer-rs-bench
rm -rf "$COMPOSER_RS_CACHE" vendor

# Cold
/usr/bin/time -f 'elapsed %e  maxrss %M' composer-rs install

# Warm (keep cache, drop vendor)
rm -rf vendor
/usr/bin/time -f 'elapsed %e  maxrss %M' composer-rs install

# Disk: CAS vs duplicated vendor
du -sh "$COMPOSER_RS_CACHE" vendor
# After a second worktree install, vendor grows; CAS should stay roughly flat.

composer-rs cache info
```

Composer equivalent (for comparison):

```bash
export COMPOSER_CACHE_DIR=/tmp/composer-php-bench
rm -rf vendor
/usr/bin/time -f 'elapsed %e  maxrss %M' composer install --no-plugins --no-scripts
```

## What we expect (qualitative)

| Metric | Cold | Warm / extra worktree |
|--------|------|------------------------|
| Wall time | Similar or slightly faster on fat lockfiles (HTTP/2 parallel) | **Much faster** when CAS hits (hardlink vs unzip) |
| Disk | One CAS copy + one vendor tree | N worktrees ≈ 1× package bytes + N× directory metadata |
| CPU | Extract once into CAS | Near-zero extract on hit |

## Reporting

When publishing numbers, include:

- Package count (prod / dev)
- Machine (CPU, disk type)
- Composer and composer-rs versions
- Whether `--prefer-source` / VCS packages were involved (clones dwarf zip installs)

There is no automated bench job in CI (network + Packagist variance). Re-run the commands above before claiming a speedup.
